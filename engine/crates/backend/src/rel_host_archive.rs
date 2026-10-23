//! In-process native ZIP capabilities. No network or language runtime is used.
//! Work is bounded and admitted before spawning, so request bursts cannot fill
//! Tokio's blocking queue. The permit and temporary workspace live with the
//! blocking task even if its waiting HTTP request is cancelled.
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::{rel_error, FreshWorkspace};
use rbe_install_runtime::workspace_archive as zip;
use route_engine::{
    ArchiveFormat, ArchivePlan, ModuleEvalError, Value, WorkspacePath, WorkspaceRoot,
};
use tokio::sync::Semaphore;

const CONCURRENT_ARCHIVES: usize = 2;
static ADMISSION: OnceLock<Arc<Semaphore>> = OnceLock::new();
// Atomic replacement prevents partial files. Serializing mutations also prevents
// two request-time read/modify/write operations from losing each other's edits.
static MUTATION: Mutex<()> = Mutex::new(());

pub(super) async fn execute(
    project: PathBuf,
    plan: ArchivePlan,
    workspace: Option<FreshWorkspace>,
) -> Result<Value, ModuleEvalError> {
    let permit = ADMISSION
        .get_or_init(|| Arc::new(Semaphore::new(CONCURRENT_ARCHIVES)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            rel_error(
                "REL2200",
                "local archive workers are busy; retry the request",
            )
        })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let temp = workspace.as_ref().map(FreshWorkspace::path);
        run(&project, &plan, temp)
    })
    .await
    .map_err(|error| rel_error("REL2200", format!("local archive worker failed: {error}")))?
}

fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect::<HashMap<_, _>>(),
    )
}
fn resolve(
    project: &Path,
    temp: Option<&Path>,
    path: &WorkspacePath,
) -> Result<PathBuf, ModuleEvalError> {
    let root = match path.root() {
        WorkspaceRoot::Project => project,
        WorkspaceRoot::Temp => {
            temp.ok_or_else(|| rel_error("REL2213", "??/ archive paths require workspace.temp()"))?
        }
    };
    // Reject drive/ADS paths on Unix too, so the same REL has the same boundary
    // on every OS. WorkspacePath has already rejected absolute paths and `..`.
    if path.relative().contains(':')
        || Path::new(path.relative())
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(rel_error(
            "REL2213",
            "archive path has a non-normal workspace component",
        ));
    }
    let resolved = root.join(path.relative());
    if !resolved.starts_with(root) {
        return Err(rel_error(
            "REL2213",
            "archive path escaped its symbolic root",
        ));
    }
    super::ensure_no_symlink_components_allow_missing(&resolved)
        .map_err(|e| rel_error("REL2213", e.to_string()))?;
    Ok(resolved)
}
fn run(project: &Path, plan: &ArchivePlan, temp: Option<&Path>) -> Result<Value, ModuleEvalError> {
    let format = match plan {
        ArchivePlan::List { format, .. }
        | ArchivePlan::Read { format, .. }
        | ArchivePlan::Extract { format, .. }
        | ArchivePlan::Create { format, .. }
        | ArchivePlan::Replace { format, .. }
        | ArchivePlan::Remove { format, .. } => format,
    };
    if *format != ArchiveFormat::Zip {
        return Err(rel_error(
            "REL2200",
            "local archive execution currently supports ZIP only",
        ));
    }
    let mutate = !matches!(plan, ArchivePlan::List { .. } | ArchivePlan::Read { .. });
    let _mutation = if mutate {
        Some(
            MUTATION
                .lock()
                .map_err(|_| rel_error("REL2200", "archive mutation lock is poisoned"))?,
        )
    } else {
        None
    };
    let result = (|| -> Result<Value, zip::WorkspaceArchiveError> {
        // Resolve outside the ZIP library: only this boundary knows the frozen
        // ProjectRoot and whether a temporary workspace belongs to this call.
        let read = |archive: &WorkspacePath| resolve(project, temp, archive);
        match plan {
            ArchivePlan::List { archive, .. } => {
                let entries = zip::list_zip(&read(archive).map_err(path_error)?)?;
                Ok(object([
                    ("ok", Value::Bool(true)),
                    (
                        "entries",
                        Value::Array(
                            entries
                                .into_iter()
                                .map(|entry| {
                                    object([
                                        ("path", Value::String(entry.path)),
                                        ("directory", Value::Bool(entry.directory)),
                                        ("size", Value::Number(entry.size as f64)),
                                        (
                                            "compressedSize",
                                            Value::Number(entry.compressed_size as f64),
                                        ),
                                    ])
                                })
                                .collect(),
                        ),
                    ),
                ]))
            }
            ArchivePlan::Read { archive, entry, .. } => {
                let entry =
                    zip::read_zip_entry(&read(archive).map_err(path_error)?, entry.as_str())?;
                let text = std::str::from_utf8(&entry.bytes)
                    .ok()
                    .map(|s| Value::String(s.into()))
                    .unwrap_or(Value::Null);
                Ok(object([
                    ("ok", Value::Bool(true)),
                    ("path", Value::String(entry.path)),
                    ("size", Value::Number(entry.size as f64)),
                    ("sha256", Value::String(entry.sha256)),
                    ("text", text),
                    ("dataHex", Value::String(hex::encode(entry.bytes))),
                ]))
            }
            _ => {
                let written = match plan {
                    ArchivePlan::Extract {
                        archive,
                        entries,
                        destination,
                        ..
                    } => zip::extract_zip(
                        &read(archive).map_err(path_error)?,
                        &entries
                            .iter()
                            .map(|e| e.as_str().to_string())
                            .collect::<Vec<_>>(),
                        &read(destination).map_err(path_error)?,
                    )?,
                    ArchivePlan::Create {
                        source,
                        destination,
                        ..
                    } => zip::create_zip(
                        &read(source).map_err(path_error)?,
                        &read(destination).map_err(path_error)?,
                    )?,
                    ArchivePlan::Replace {
                        archive,
                        entry,
                        source,
                        ..
                    } => zip::replace_zip_entry(
                        &read(archive).map_err(path_error)?,
                        entry.as_str(),
                        &read(source).map_err(path_error)?,
                    )?,
                    ArchivePlan::Remove { archive, entry, .. } => {
                        zip::remove_zip_entry(&read(archive).map_err(path_error)?, entry.as_str())?
                    }
                    _ => unreachable!(),
                };
                Ok(object([
                    ("ok", Value::Bool(true)),
                    ("bytesWritten", Value::Number(written.archive_bytes as f64)),
                    ("entries", Value::Number(written.entries as f64)),
                ]))
            }
        }
    })();
    result.map_err(|error| {
        let mut message = error.to_string();
        if let Some(temp) = temp {
            message = message.replace(&temp.to_string_lossy().to_string(), "??");
        }
        message = message.replace(&project.to_string_lossy().to_string(), "$$");
        rel_error(
            "REL2200",
            format!("local ZIP operation rejected: {message}"),
        )
    })
}
fn path_error(error: ModuleEvalError) -> zip::WorkspaceArchiveError {
    zip::WorkspaceArchiveError::InvalidSource(error.message)
}

#[cfg(test)]
mod tests {
    use super::super::BackendRelHostExecutor;
    use super::*;
    use route_engine::{RelHostExecutor, RelHostRequest};
    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("rbe-local-archive-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn fields(value: Value) -> HashMap<String, Value> {
        let Value::Object(fields) = value else {
            panic!("expected archive result");
        };
        fields
    }
    #[tokio::test]
    async fn real_executor_completes_local_zip_operations_without_a_node_or_script_runtime() {
        let root = Root::new();
        std::fs::create_dir(root.0.join("package")).unwrap();
        std::fs::write(
            root.0.join("package/package.rbe.yaml"),
            "name: local-test\nversion: 1.0.0\n",
        )
        .unwrap();
        std::fs::write(root.0.join("replacement.txt"), "updated").unwrap();
        let executor = BackendRelHostExecutor {
            project_root: root.0.clone(),
            container_path: root.0.join("no-container-used"),
            cgroup_root: None,
        };
        let create = ArchivePlan::create("$$/package", "$$/release.zip").unwrap();
        assert!(matches!(
            fields(
                executor
                    .execute(RelHostRequest::Archive(create))
                    .await
                    .unwrap()
            )["ok"],
            Value::Bool(true)
        ));
        let list = fields(
            executor
                .execute(RelHostRequest::Archive(
                    ArchivePlan::list("$$/release.zip").unwrap(),
                ))
                .await
                .unwrap(),
        );
        assert!(matches!(&list["entries"], Value::Array(entries) if entries.len() == 1));
        let read = fields(
            executor
                .execute(RelHostRequest::Archive(
                    ArchivePlan::read("$$/release.zip", "package.rbe.yaml").unwrap(),
                ))
                .await
                .unwrap(),
        );
        assert!(matches!(&read["text"], Value::String(text) if text.contains("local-test")));
        executor
            .execute(RelHostRequest::Archive(
                ArchivePlan::extract(
                    "$$/release.zip",
                    vec!["package.rbe.yaml".into()],
                    "$$/unpacked",
                )
                .unwrap(),
            ))
            .await
            .unwrap();
        assert!(root.0.join("unpacked/package.rbe.yaml").is_file());
        executor
            .execute(RelHostRequest::Archive(
                ArchivePlan::replace("$$/release.zip", "package.rbe.yaml", "$$/replacement.txt")
                    .unwrap(),
            ))
            .await
            .unwrap();
        let read = fields(
            executor
                .execute(RelHostRequest::Archive(
                    ArchivePlan::read("$$/release.zip", "package.rbe.yaml").unwrap(),
                ))
                .await
                .unwrap(),
        );
        assert!(matches!(&read["text"], Value::String(text) if text == "updated"));
        executor
            .execute(RelHostRequest::Archive(
                ArchivePlan::remove("$$/release.zip", "package.rbe.yaml").unwrap(),
            ))
            .await
            .unwrap();
        assert!(executor
            .execute(RelHostRequest::Archive(
                ArchivePlan::read("$$/release.zip", "package.rbe.yaml").unwrap()
            ))
            .await
            .is_err());
        executor
            .execute(RelHostRequest::Temp(Box::new(RelHostRequest::Archive(
                ArchivePlan::create("$$/package", "??/disposable.zip").unwrap(),
            ))))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_dir(root.0.join(".cache/rbe/workspaces"))
                .unwrap()
                .count(),
            0
        );
        assert!(executor
            .execute(RelHostRequest::Archive(
                ArchivePlan::list("??/not-owned.zip").unwrap()
            ))
            .await
            .is_err());
        assert!(executor
            .execute(RelHostRequest::Archive(
                ArchivePlan::list("$$/unsupported.tar").unwrap()
            ))
            .await
            .is_err());
        // Admission is checked before spawning: overload produces an immediate
        // failure rather than creating an unbounded blocking-worker queue.
        let slots = ADMISSION
            .get()
            .unwrap()
            .clone()
            .acquire_many_owned(CONCURRENT_ARCHIVES as u32)
            .await
            .unwrap();
        assert!(executor
            .execute(RelHostRequest::Archive(
                ArchivePlan::list("$$/release.zip").unwrap()
            ))
            .await
            .unwrap_err()
            .message
            .contains("busy"));
        drop(slots);
    }
    #[cfg(unix)]
    #[test]
    fn symbolic_roots_do_not_follow_a_project_symlink() {
        let root = Root::new();
        let outside = Root::new();
        std::os::unix::fs::symlink(&outside.0, root.0.join("escape")).unwrap();
        assert!(resolve(
            &root.0,
            None,
            &WorkspacePath::project("escape/data.zip").unwrap()
        )
        .is_err());
        assert!(resolve(
            &root.0,
            None,
            &WorkspacePath::project("C:/outside.zip").unwrap()
        )
        .is_err());
    }
}
