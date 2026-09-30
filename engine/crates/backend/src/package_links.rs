#[path = "package_links/host.rs"]
pub(crate) mod host;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context};
use core_lib::{
    LibraryCapabilityGrant, LibraryHostCall, LibraryHostCallReply, LibrarySessionBinding,
};
use rbe_install_runtime::{ProjectInstallRecovery, VerifiedRpxRootIndex, VerifiedRpxRootSnapshot};
use route_engine::relc::{
    PackageExportLink, PackageLinkContext, PackageRootLink, PACKAGE_LINK_FORMAT,
};
use serde::Deserialize;

const RPX_PACKAGE_INDEX_FORMAT: u32 = 1;

type LibraryHostDispatcher = fn(
    &str,
    &LibrarySessionBinding,
    &LibraryHostCall,
) -> anyhow::Result<LibraryHostCallReply>;

struct LibraryHostSessionEntry {
    binding: LibrarySessionBinding,
    dispatcher: LibraryHostDispatcher,
}

static LIBRARY_HOST_SESSIONS: OnceLock<Mutex<BTreeMap<String, LibraryHostSessionEntry>>> =
    OnceLock::new();

#[derive(Debug, Clone)]
pub struct LoadedPackageRoots {
    pub links: PackageLinkContext,
    pub roots: BTreeMap<String, VerifiedRpxRootSnapshot>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpxPackageIndex {
    format: u32,
    package: RpxPackageIdentity,
    exports: Vec<RpxPackageExport>,
    #[serde(default)]
    private_dependencies: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpxPackageIdentity {
    name: String,
    version: String,
    language: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpxPackageExport {
    name: String,
    source: String,
    language: String,
}

/// Build RELC's package namespace from SHA-verified explicit package roots.
///
/// The same verified root snapshot also seeds fail-closed Library Host sessions
/// before the public export namespace is returned. Sessions deliberately start
/// with only host-owned implicit authority until capability admission persists
/// explicit package grants.
pub fn load(project_root: &Path) -> anyhow::Result<PackageLinkContext> {
    let loaded = load_with_workers(project_root)?;
    install_verified_host_sessions(&loaded.roots)?;
    Ok(loaded.links)
}

/// Read one fail-closed package-state snapshot and retain it for both RELC
/// linking and Library Host execution.
///
/// `read_verified_rpx_root_snapshots` joins each public RPX export index with
/// the independently verified worker identity for the same active package
/// graph. Keeping those snapshots beside the derived public link context avoids
/// rebuilding worker identity from a later package-state read.
pub fn load_with_workers(project_root: &Path) -> anyhow::Result<LoadedPackageRoots> {
    let recovery = rbe_install_runtime::recover_project_activation(project_root)
        .context("recover interrupted project package activation before package linking")?;
    if recovery != ProjectInstallRecovery::Clean {
        tracing::warn!(
            recovery = ?recovery,
            "recovered interrupted project package activation before Runtime Image linking"
        );
    }

    let snapshots = rbe_install_runtime::read_verified_rpx_root_snapshots(project_root)
        .context("load verified RPX package root snapshots")?;
    from_verified_snapshots(snapshots)
}

fn build_verified_host_sessions(
    roots: &BTreeMap<String, VerifiedRpxRootSnapshot>,
) -> anyhow::Result<BTreeMap<String, LibraryHostSessionEntry>> {
    let mut sessions = BTreeMap::new();
    for (package, snapshot) in roots {
        let binding = host::bind_session(snapshot, std::iter::empty::<LibraryCapabilityGrant>())
            .with_context(|| format!("bind Library Host session for verified root {package:?}"))?;
        let entry = LibraryHostSessionEntry {
            binding,
            dispatcher: host::dispatch_authorized_host_call,
        };
        if sessions.insert(package.clone(), entry).is_some() {
            bail!("duplicate Library Host session for verified root {package:?}");
        }
    }
    Ok(sessions)
}

fn install_verified_host_sessions(
    roots: &BTreeMap<String, VerifiedRpxRootSnapshot>,
) -> anyhow::Result<()> {
    let sessions = build_verified_host_sessions(roots)?;
    let registry = LIBRARY_HOST_SESSIONS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut active = registry
        .lock()
        .map_err(|_| anyhow::anyhow!("Library Host session registry lock is poisoned"))?;

    for session in active.values_mut() {
        session.binding.close();
    }
    *active = sessions;

    tracing::info!(
        package_sessions = active.len(),
        "prepared verified fail-closed Library Host sessions with trusted dispatchers"
    );
    Ok(())
}

fn from_verified_snapshots(
    snapshots: Vec<VerifiedRpxRootSnapshot>,
) -> anyhow::Result<LoadedPackageRoots> {
    let indexes = snapshots
        .iter()
        .map(|snapshot| VerifiedRpxRootIndex {
            package: snapshot.package.clone(),
            version: snapshot.version.clone(),
            artifact_sha256: snapshot.artifact_sha256.clone(),
            index_json: snapshot.index_json.clone(),
        })
        .collect();
    let links = from_verified_indexes(indexes)?;
    let mut roots = BTreeMap::new();
    for snapshot in snapshots {
        let package = snapshot.package.clone();
        if roots.insert(package.clone(), snapshot).is_some() {
            bail!("duplicate verified RPX root package {package:?}");
        }
    }
    Ok(LoadedPackageRoots { links, roots })
}

fn from_verified_indexes(indexes: Vec<VerifiedRpxRootIndex>) -> anyhow::Result<PackageLinkContext> {
    let mut roots = BTreeMap::new();

    for verified in indexes {
        let index: RpxPackageIndex =
            serde_json::from_str(&verified.index_json).with_context(|| {
                format!(
                    "parse RPX package index for explicit root {:?}",
                    verified.package
                )
            })?;

        if index.format != RPX_PACKAGE_INDEX_FORMAT {
            bail!(
                "unsupported RPX package index format {} for explicit root {:?}",
                index.format,
                verified.package
            );
        }
        if index.package.name != verified.package {
            bail!(
                "RPX package index identity mismatch for explicit root {:?}: index declares {:?}",
                verified.package,
                index.package.name
            );
        }
        if index.package.version != verified.version {
            bail!(
                "RPX package index version mismatch for explicit root {:?}: lock={}, index={}",
                verified.package,
                verified.version,
                index.package.version
            );
        }
        if !matches!(
            index.package.language.as_str(),
            "rust" | "javascript" | "typescript" | "python" | "global"
        ) {
            bail!(
                "unsupported RPX package language {:?} for explicit root {:?}",
                index.package.language,
                verified.package
            );
        }

        // Parsed deliberately, never linked. This keeps malformed indexes from
        // slipping through while making the visibility rule impossible to
        // accidentally widen through iteration over transitive dependency data.
        let _private_dependency_names = index.private_dependencies.keys().collect::<BTreeSet<_>>();

        let mut exports = BTreeMap::new();
        for export in index.exports {
            if export.language == "global" {
                bail!(
                    "RPX export {:?} from {:?} resolved to global instead of a concrete SDK language",
                    export.name,
                    verified.package
                );
            }
            if !matches!(
                export.language.as_str(),
                "rust" | "javascript" | "typescript" | "python"
            ) {
                bail!(
                    "unsupported RPX export language {:?} for {} from {}",
                    export.language,
                    export.name,
                    verified.package
                );
            }
            let name = export.name;
            if exports
                .insert(
                    name.clone(),
                    PackageExportLink {
                        entry: export.source,
                        language: export.language,
                    },
                )
                .is_some()
            {
                bail!(
                    "duplicate RPX public export {:?} in explicit root {:?}",
                    name,
                    verified.package
                );
            }
        }

        let package_name = verified.package;
        if roots
            .insert(
                package_name.clone(),
                PackageRootLink {
                    version: verified.version,
                    artifact_sha256: verified.artifact_sha256,
                    exports,
                },
            )
            .is_some()
        {
            bail!("duplicate verified RPX root package {package_name:?}");
        }
    }

    let context = PackageLinkContext {
        format: PACKAGE_LINK_FORMAT,
        roots,
    };
    context
        .validate()
        .context("validate RELC package-link context derived from verified roots")?;
    Ok(context)
}

#[cfg(test)]
mod tests {
    use rbe_install_runtime::VerifiedPackageWorkerIdentity;

    use super::*;

    fn verified(index_json: &str) -> VerifiedRpxRootIndex {
        VerifiedRpxRootIndex {
            package: "advancenet".into(),
            version: "2.0.0".into(),
            artifact_sha256: "a".repeat(64),
            index_json: index_json.into(),
        }
    }

    fn snapshot(index_json: &str) -> VerifiedRpxRootSnapshot {
        VerifiedRpxRootSnapshot {
            package: "advancenet".into(),
            version: "2.0.0".into(),
            artifact_sha256: "a".repeat(64),
            index_json: index_json.into(),
            worker: VerifiedPackageWorkerIdentity {
                package: "advancenet".into(),
                version: "2.0.0".into(),
                artifact_sha256: "a".repeat(64),
                rbe_abi_min: 1,
                rbe_abi_max: 1,
                sdk_language: "bun".into(),
                sdk_name: "@rbe/sdk".into(),
                sdk_version: "0.1.9".into(),
                runtime_kind: "bun".into(),
                runtime_version: "1.3.7".into(),
                runtime_entry: "src/index.js".into(),
                runtime_managed: true,
            },
        }
    }

    fn index(private_dependencies: &str, exports: &str) -> String {
        format!(
            r#"{{
                "format": 1,
                "package": {{
                    "name": "advancenet",
                    "version": "2.0.0",
                    "language": "typescript"
                }},
                "exports": {exports},
                "private_dependencies": {private_dependencies}
            }}"#
        )
    }

    #[test]
    fn verified_root_exports_become_relc_links() {
        let input = index(
            r#"{"secret-parser":"^9"}"#,
            r#"[{"name":"request","source":"components/request/request.ts","language":"typescript"}]"#,
        );
        let links = from_verified_indexes(vec![verified(&input)]).unwrap();
        let request = links.resolve("advancenet", "request").unwrap();
        assert_eq!(request.entry, "components/request/request.ts");
        assert_eq!(request.language, "typescript");
        assert!(links.root("secret-parser").is_none());
    }

    #[test]
    fn verified_snapshot_is_retained_beside_relc_links() {
        let input = index(
            "{}",
            r#"[{"name":"request","source":"components/request/request.ts","language":"typescript"}]"#,
        );
        let loaded = from_verified_snapshots(vec![snapshot(&input)]).unwrap();
        assert_eq!(loaded.links.roots.len(), 1);
        let retained = loaded.roots.get("advancenet").unwrap();
        assert_eq!(retained.worker.runtime_kind, "bun");
        assert_eq!(retained.worker.runtime_version, "1.3.7");
        assert_eq!(retained.artifact_sha256, "a".repeat(64));

        let sessions = build_verified_host_sessions(&loaded.roots).unwrap();
        assert_eq!(sessions.len(), 1);
        let session = sessions.get("advancenet").unwrap();
        assert_eq!(
            session.binding.state(),
            core_lib::LibrarySessionState::AwaitHello
        );
        assert_eq!(
            session.dispatcher as usize,
            host::dispatch_authorized_host_call as usize
        );
    }

    #[test]
    fn private_dependency_metadata_never_creates_rel_roots() {
        let input = index(
            r#"{"secret-parser":"^9","syntax":"^1"}"#,
            r#"[{"name":"request","source":"components/request/request.ts","language":"typescript"}]"#,
        );
        let links = from_verified_indexes(vec![verified(&input)]).unwrap();
        assert_eq!(links.roots.len(), 1);
        assert!(links.roots.contains_key("advancenet"));
        assert!(!links.roots.contains_key("secret-parser"));
        assert!(!links.roots.contains_key("syntax"));
    }

    #[test]
    fn package_identity_must_match_trusted_lock_root() {
        let input = index(
            "{}",
            r#"[{"name":"request","source":"components/request/request.ts","language":"typescript"}]"#,
        )
        .replace("\"name\": \"advancenet\"", "\"name\": \"evil-root\"");
        let error = from_verified_indexes(vec![verified(&input)]).unwrap_err();
        assert!(error.to_string().contains("identity mismatch"));
    }

    #[test]
    fn package_version_must_match_trusted_lock_root() {
        let input = index(
            "{}",
            r#"[{"name":"request","source":"components/request/request.ts","language":"typescript"}]"#,
        )
        .replace("\"version\": \"2.0.0\"", "\"version\": \"3.0.0\"");
        let error = from_verified_indexes(vec![verified(&input)]).unwrap_err();
        assert!(error.to_string().contains("version mismatch"));
    }

    #[test]
    fn duplicate_public_exports_are_rejected() {
        let input = index(
            "{}",
            r#"[
                {"name":"request","source":"components/request/request.ts","language":"typescript"},
                {"name":"request","source":"components/request/other.ts","language":"typescript"}
            ]"#,
        );
        let error = from_verified_indexes(vec![verified(&input)]).unwrap_err();
        assert!(error.to_string().contains("duplicate RPX public export"));
    }
}
