use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use ipc_protocol::MAX_CAPABILITY_PAYLOAD_BYTES;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::EnvironmentStorageManager;

pub const STORAGE_CAPABILITY_TARGET_PREFIX: &str = "storage:";
pub const STORAGE_CAPABILITY_OPERATIONS: [&str; 5] =
    ["read", "list", "snapshot", "commit", "write"];
const MAX_STORAGE_NAMESPACE_BYTES: usize = 64;
const MAX_COMMIT_MUTATIONS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageCapabilityError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for StorageCapabilityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for StorageCapabilityError {}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum StorageMutationRequest {
    Put {
        path: String,
        #[serde(default)]
        data_hex: Option<String>,
        #[serde(default)]
        data: Option<Value>,
    },
    Delete {
        path: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectWriteRequest {
    path: String,
    data: Value,
    encoding: String,
    level: u8,
}

pub fn storage_capability_operation_allowed(operation: &str) -> bool {
    STORAGE_CAPABILITY_OPERATIONS.contains(&operation)
}

pub fn storage_capability_target(namespace: &str) -> Result<String, StorageCapabilityError> {
    validate_namespace(namespace)?;
    Ok(format!("{STORAGE_CAPABILITY_TARGET_PREFIX}{namespace}"))
}

/// Execute an already-Controller-authorized Storage capability against the
/// authoritative storage manager owned by one exact Environment process.
///
/// This compatibility entry point deliberately has no project-root authority.
/// Existing Environment-CAS operations work normally; a `write` request fails
/// closed until the caller supplies the frozen backend ProjectRoot through
/// [`dispatch_storage_capability_with_project_root`].
pub fn dispatch_storage_capability(
    storage: &Arc<EnvironmentStorageManager>,
    target: &str,
    operation: &str,
    payload: &[u8],
    max_response_bytes: u64,
) -> Result<Vec<u8>, StorageCapabilityError> {
    dispatch_storage_capability_inner(
        storage,
        None,
        target,
        operation,
        payload,
        max_response_bytes,
    )
}

/// Execute Storage with explicit authority to RBE's frozen project root.
///
/// The root must be captured by `backend.exe` at boot and propagated through
/// Container bootstrap. REL never supplies this host path: it can only name a
/// symbolic `$$/...` destination, which is resolved and contained here.
pub fn dispatch_storage_capability_with_project_root(
    storage: &Arc<EnvironmentStorageManager>,
    project_root: &Path,
    target: &str,
    operation: &str,
    payload: &[u8],
    max_response_bytes: u64,
) -> Result<Vec<u8>, StorageCapabilityError> {
    dispatch_storage_capability_inner(
        storage,
        Some(project_root),
        target,
        operation,
        payload,
        max_response_bytes,
    )
}

fn dispatch_storage_capability_inner(
    storage: &Arc<EnvironmentStorageManager>,
    project_root: Option<&Path>,
    target: &str,
    operation: &str,
    payload: &[u8],
    max_response_bytes: u64,
) -> Result<Vec<u8>, StorageCapabilityError> {
    if payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES {
        return Err(error(
            "CAPABILITY_REQUEST_TOO_LARGE",
            "Storage capability request exceeded the protocol limit",
        ));
    }
    if !storage_capability_operation_allowed(operation) {
        return Err(error(
            "CAPABILITY_STORAGE_OPERATION_INVALID",
            "Storage capability operation is not supported",
        ));
    }
    let namespace = namespace_from_target(target)?;
    let args: Vec<Value> = serde_json::from_slice(payload).map_err(|_| {
        error(
            "CAPABILITY_STORAGE_ARGS_INVALID",
            "Storage capability payload must be a JSON argument array",
        )
    })?;

    let response = match operation {
        "read" => {
            let [path] = args.as_slice() else {
                return Err(invalid_args("read expects exactly one path argument"));
            };
            let path = path
                .as_str()
                .ok_or_else(|| invalid_args("read path must be a string"))?;
            match storage
                .read(namespace, path)
                .map_err(|_| storage_call_failed())?
            {
                Some(bytes) => {
                    let data_hex = hex::encode(&bytes);
                    match serde_json::from_slice::<Value>(&bytes) {
                        Ok(data) => json!({
                            "found": true,
                            "dataHex": data_hex,
                            "data": data,
                        }),
                        Err(_) => json!({
                            "found": true,
                            "dataHex": data_hex,
                        }),
                    }
                }
                None => json!({ "found": false }),
            }
        }
        "list" => {
            require_no_args(&args, "list")?;
            let entries = storage.list(namespace).map_err(|_| storage_call_failed())?;
            json!({ "entries": entries })
        }
        "snapshot" => {
            require_no_args(&args, "snapshot")?;
            let snapshot = storage
                .snapshot(namespace)
                .map_err(|_| storage_call_failed())?;
            json!({
                "namespace": snapshot.namespace,
                "generation": snapshot.generation,
                "files": snapshot.files,
                "logicalBytes": snapshot.logical_bytes,
                "limitBytes": snapshot.limit_bytes,
            })
        }
        "commit" => {
            let [mutations] = args.as_slice() else {
                return Err(invalid_args(
                    "commit expects exactly one mutation-array argument",
                ));
            };
            let mutations: Vec<StorageMutationRequest> = serde_json::from_value(mutations.clone())
                .map_err(|_| invalid_args("commit mutations must use put/delete objects"))?;
            if mutations.is_empty() || mutations.len() > MAX_COMMIT_MUTATIONS {
                return Err(invalid_args(
                    "commit mutation count must be between 1 and 256",
                ));
            }

            let mut transaction = storage
                .begin(namespace)
                .map_err(|_| storage_call_failed())?;
            for mutation in mutations {
                match mutation {
                    StorageMutationRequest::Put {
                        path,
                        data_hex,
                        data,
                    } => {
                        let bytes = encode_transactional_put(data_hex, data)?;
                        transaction
                            .put(&path, &bytes)
                            .map_err(|_| storage_call_failed())?;
                    }
                    StorageMutationRequest::Delete { path } => {
                        transaction
                            .delete(&path)
                            .map_err(|_| storage_call_failed())?;
                    }
                }
            }
            let commit = transaction.commit().map_err(|_| storage_call_failed())?;
            json!({
                "namespace": commit.namespace,
                "generation": commit.generation,
                "files": commit.files,
                "logicalBytes": commit.logical_bytes,
                "changedPaths": commit.changed_paths,
            })
        }
        "write" => {
            let project_root = project_root.ok_or_else(|| {
                error(
                    "CAPABILITY_STORAGE_PROJECT_ROOT_UNAVAILABLE",
                    "project-root Storage authority was not supplied",
                )
            })?;
            let [descriptor] = args.as_slice() else {
                return Err(invalid_args(
                    "write expects exactly one normalized write descriptor",
                ));
            };
            let descriptor: ProjectWriteRequest = serde_json::from_value(descriptor.clone())
                .map_err(|_| invalid_args("write descriptor is malformed"))?;
            let (bytes, normalized_encoding) = write_project_file(project_root, &descriptor)?;
            json!({
                "path": descriptor.path,
                "bytes": bytes,
                "encoding": normalized_encoding,
                "level": descriptor.level,
            })
        }
        _ => unreachable!("operation allowlist checked above"),
    };

    encode_response(&response, max_response_bytes)
}

fn encode_transactional_put(
    data_hex: Option<String>,
    data: Option<Value>,
) -> Result<Vec<u8>, StorageCapabilityError> {
    match (data_hex, data) {
        (Some(data_hex), None) => {
            hex::decode(data_hex).map_err(|_| invalid_args("put dataHex must be hexadecimal"))
        }
        (None, Some(data)) => serde_json::to_vec(&data)
            .map_err(|_| invalid_args("put data could not be encoded as JSON")),
        (Some(_), Some(_)) => Err(invalid_args(
            "put must provide exactly one of data_hex or data",
        )),
        (None, None) => Err(invalid_args(
            "put must provide exactly one of data_hex or data",
        )),
    }
}

fn write_project_file(
    project_root: &Path,
    descriptor: &ProjectWriteRequest,
) -> Result<(usize, &'static str), StorageCapabilityError> {
    if !(1..=3).contains(&descriptor.level) {
        return Err(invalid_args("write level must be 1, 2, or 3"));
    }

    let target = resolve_project_write_path(project_root, &descriptor.path)?;
    let (bytes, normalized_encoding) =
        encode_project_write_data(&descriptor.data, &descriptor.encoding)?;
    let logical_path = descriptor.path.strip_prefix("$$/").ok_or_else(|| {
        error(
            "CAPABILITY_STORAGE_PATH_INVALID",
            "project write path must start with $$/",
        )
    })?;
    let prepared =
        storage_sync_journal::prepare(project_root, logical_path, descriptor.level, &bytes)
            .map_err(|_| {
                error(
                    "CAPABILITY_STORAGE_WRITE_FAILED",
                    "project-root Storage sync intent could not be staged",
                )
            })?;
    if atomic_io::AtomicIo::new()
        .write_atomic(&target, &bytes)
        .is_err()
    {
        let _ = storage_sync_journal::cancel(&prepared);
        return Err(error(
            "CAPABILITY_STORAGE_WRITE_FAILED",
            "project-root Storage write failed",
        ));
    }
    storage_sync_journal::commit(&prepared).map_err(|_| {
        error(
            "CAPABILITY_STORAGE_WRITE_FAILED",
            "project-root Storage sync intent could not be published",
        )
    })?;
    Ok((bytes.len(), normalized_encoding))
}

fn resolve_project_write_path(
    project_root: &Path,
    logical_path: &str,
) -> Result<PathBuf, StorageCapabilityError> {
    let root = project_root.canonicalize().map_err(|_| {
        error(
            "CAPABILITY_STORAGE_PROJECT_ROOT_INVALID",
            "frozen ProjectRoot is unavailable",
        )
    })?;
    if !root.is_absolute() || !root.is_dir() {
        return Err(error(
            "CAPABILITY_STORAGE_PROJECT_ROOT_INVALID",
            "frozen ProjectRoot must be an existing absolute directory",
        ));
    }

    let relative = logical_path.strip_prefix("$$/").ok_or_else(|| {
        error(
            "CAPABILITY_STORAGE_PATH_INVALID",
            "project write path must start with $$/",
        )
    })?;
    if relative.is_empty() || relative.contains('\\') || relative.contains('\0') {
        return Err(error(
            "CAPABILITY_STORAGE_PATH_INVALID",
            "project write path is empty or contains a non-portable separator",
        ));
    }

    let mut segments = Vec::<OsString>::new();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(segment) => {
                if segment.to_string_lossy().contains(':') {
                    return Err(error(
                        "CAPABILITY_STORAGE_PATH_INVALID",
                        "project write path contains a drive or stream separator",
                    ));
                }
                segments.push(segment.to_os_string());
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(error(
                    "CAPABILITY_STORAGE_PATH_INVALID",
                    "project write path may not contain root, dot, or parent components",
                ));
            }
        }
    }

    if segments
        .first()
        .is_some_and(|segment| segment.to_string_lossy().eq_ignore_ascii_case(".rbe"))
    {
        return Err(error(
            "CAPABILITY_STORAGE_PATH_INVALID",
            "project write path targets RBE internal state",
        ));
    }

    let Some(file_name) = segments.pop() else {
        return Err(error(
            "CAPABILITY_STORAGE_PATH_INVALID",
            "project write path must name a file",
        ));
    };

    let mut parent = root.clone();
    for segment in segments {
        let next = parent.join(segment);
        match std::fs::symlink_metadata(&next) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let resolved = next.canonicalize().map_err(|_| {
                    error(
                        "CAPABILITY_STORAGE_PATH_INVALID",
                        "project write path contains a broken symbolic link",
                    )
                })?;
                if !resolved.starts_with(&root) || !resolved.is_dir() {
                    return Err(error(
                        "CAPABILITY_STORAGE_PATH_INVALID",
                        "project write path escapes frozen ProjectRoot",
                    ));
                }
                parent = resolved;
            }
            Ok(metadata) if metadata.is_dir() => {
                parent = next;
            }
            Ok(_) => {
                return Err(error(
                    "CAPABILITY_STORAGE_PATH_INVALID",
                    "project write parent component is not a directory",
                ));
            }
            Err(error_value) if error_value.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&next).map_err(|_| {
                    error(
                        "CAPABILITY_STORAGE_WRITE_FAILED",
                        "project write could not create a parent directory",
                    )
                })?;
                parent = next;
            }
            Err(_) => {
                return Err(error(
                    "CAPABILITY_STORAGE_WRITE_FAILED",
                    "project write could not inspect a parent directory",
                ));
            }
        }

        let resolved_parent = parent.canonicalize().map_err(|_| {
            error(
                "CAPABILITY_STORAGE_WRITE_FAILED",
                "project write parent directory could not be canonicalized",
            )
        })?;
        if !resolved_parent.starts_with(&root) {
            return Err(error(
                "CAPABILITY_STORAGE_PATH_INVALID",
                "project write path escapes frozen ProjectRoot",
            ));
        }
        parent = resolved_parent;
    }

    let target = parent.join(file_name);
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(error(
                "CAPABILITY_STORAGE_PATH_INVALID",
                "project write target may not be a symbolic link",
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(error(
                "CAPABILITY_STORAGE_PATH_INVALID",
                "project write target exists and is not a file",
            ));
        }
        Ok(_) => {}
        Err(error_value) if error_value.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(error(
                "CAPABILITY_STORAGE_WRITE_FAILED",
                "project write target could not be inspected",
            ));
        }
    }
    Ok(target)
}

fn encode_project_write_data(
    data: &Value,
    encoding: &str,
) -> Result<(Vec<u8>, &'static str), StorageCapabilityError> {
    let normalized = encoding.trim().to_ascii_uppercase().replace('-', "");
    match normalized.as_str() {
        "UTF8" => {
            let bytes = match data {
                Value::String(value) => value.as_bytes().to_vec(),
                _ => serde_json::to_vec(data)
                    .map_err(|_| invalid_args("write data could not be encoded as UTF-8 JSON"))?,
            };
            Ok((bytes, "UTF8"))
        }
        "UTF16" | "UTF16LE" => {
            let text = project_write_text(data)?;
            let mut bytes = Vec::with_capacity(text.len().saturating_mul(2));
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            Ok((bytes, "UTF16LE"))
        }
        "UTF16BE" => {
            let text = project_write_text(data)?;
            let mut bytes = Vec::with_capacity(text.len().saturating_mul(2));
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&unit.to_be_bytes());
            }
            Ok((bytes, "UTF16BE"))
        }
        "HEX" => {
            let value = data
                .as_str()
                .ok_or_else(|| invalid_args("HEX write data must be a hexadecimal string"))?;
            let bytes =
                hex::decode(value).map_err(|_| invalid_args("HEX write data is malformed"))?;
            Ok((bytes, "HEX"))
        }
        "BYTES" => {
            let values = data
                .as_array()
                .ok_or_else(|| invalid_args("BYTES write data must be an array of integers"))?;
            let mut bytes = Vec::with_capacity(values.len());
            for value in values {
                let byte = value
                    .as_u64()
                    .filter(|value| *value <= u8::MAX as u64)
                    .ok_or_else(|| {
                        invalid_args("BYTES values must be integers from 0 through 255")
                    })?;
                bytes.push(byte as u8);
            }
            Ok((bytes, "BYTES"))
        }
        _ => Err(invalid_args(
            "write encoding must be UTF8, UTF16, UTF16LE, UTF16BE, HEX, or BYTES",
        )),
    }
}

fn project_write_text(data: &Value) -> Result<String, StorageCapabilityError> {
    match data {
        Value::String(value) => Ok(value.clone()),
        _ => serde_json::to_string(data)
            .map_err(|_| invalid_args("write data could not be serialized as text")),
    }
}

fn namespace_from_target(target: &str) -> Result<&str, StorageCapabilityError> {
    let namespace = target
        .strip_prefix(STORAGE_CAPABILITY_TARGET_PREFIX)
        .ok_or_else(|| {
            error(
                "CAPABILITY_STORAGE_TARGET_INVALID",
                "Storage target must use storage:<namespace>",
            )
        })?;
    validate_namespace(namespace)?;
    Ok(namespace)
}

fn validate_namespace(namespace: &str) -> Result<(), StorageCapabilityError> {
    if namespace.is_empty() || namespace.len() > MAX_STORAGE_NAMESPACE_BYTES {
        return Err(error(
            "CAPABILITY_STORAGE_TARGET_INVALID",
            "Storage namespace length is invalid",
        ));
    }
    if !namespace
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(error(
            "CAPABILITY_STORAGE_TARGET_INVALID",
            "Storage namespace contains unsupported characters",
        ));
    }
    Ok(())
}

fn require_no_args(args: &[Value], operation: &str) -> Result<(), StorageCapabilityError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(invalid_args(&format!(
            "{operation} does not accept arguments"
        )))
    }
}

fn encode_response(
    value: &Value,
    max_response_bytes: u64,
) -> Result<Vec<u8>, StorageCapabilityError> {
    let payload = serde_json::to_vec(value).map_err(|_| {
        error(
            "CAPABILITY_STORAGE_RESPONSE_INVALID",
            "Storage capability response could not be encoded",
        )
    })?;
    let limit = max_response_bytes.min(MAX_CAPABILITY_PAYLOAD_BYTES as u64) as usize;
    if payload.len() > limit {
        return Err(error(
            "CAPABILITY_RESPONSE_TOO_LARGE",
            "Storage capability response exceeded the capability grant",
        ));
    }
    Ok(payload)
}

fn invalid_args(message: &str) -> StorageCapabilityError {
    error("CAPABILITY_STORAGE_ARGS_INVALID", message)
}

fn storage_call_failed() -> StorageCapabilityError {
    error(
        "CAPABILITY_STORAGE_CALL_FAILED",
        "Environment Storage operation failed",
    )
}

fn error(code: &'static str, message: &str) -> StorageCapabilityError {
    StorageCapabilityError {
        code,
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn temp_storage(name: &str, limit: u64) -> (PathBuf, Arc<EnvironmentStorageManager>) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-storage-capability-{name}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let storage = EnvironmentStorageManager::open(root.clone(), limit).unwrap();
        (root, storage)
    }

    fn dispatch(
        storage: &Arc<EnvironmentStorageManager>,
        target: &str,
        operation: &str,
        args: Value,
    ) -> Result<Value, StorageCapabilityError> {
        let payload = serde_json::to_vec(&args).unwrap();
        let response = dispatch_storage_capability(
            storage,
            target,
            operation,
            &payload,
            MAX_CAPABILITY_PAYLOAD_BYTES as u64,
        )?;
        Ok(serde_json::from_slice(&response).unwrap())
    }

    fn dispatch_project(
        storage: &Arc<EnvironmentStorageManager>,
        project_root: &Path,
        target: &str,
        operation: &str,
        args: Value,
    ) -> Result<Value, StorageCapabilityError> {
        let payload = serde_json::to_vec(&args).unwrap();
        let response = dispatch_storage_capability_with_project_root(
            storage,
            project_root,
            target,
            operation,
            &payload,
            MAX_CAPABILITY_PAYLOAD_BYTES as u64,
        )?;
        Ok(serde_json::from_slice(&response).unwrap())
    }

    #[test]
    fn exact_namespace_target_round_trips_atomic_commit_and_reads() {
        let (root, storage) = temp_storage("round-trip", 4096);
        let target = storage_capability_target("uac").unwrap();
        let committed = dispatch(
            &storage,
            &target,
            "commit",
            json!([[
                {"op":"put", "path":"users/kate.bin", "data_hex":"6b617465"},
                {"op":"put", "path":"meta/version", "data_hex":"31"}
            ]]),
        )
        .unwrap();
        assert_eq!(committed["generation"], 1);
        assert_eq!(committed["changedPaths"], 2);

        let read = dispatch(&storage, &target, "read", json!(["users/kate.bin"])).unwrap();
        assert_eq!(read["found"], true);
        assert_eq!(read["dataHex"], "6b617465");
        assert!(read.get("data").is_none());

        let listed = dispatch(&storage, &target, "list", json!([])).unwrap();
        assert_eq!(listed["entries"].as_array().unwrap().len(), 2);
        let snapshot = dispatch(&storage, &target, "snapshot", json!([])).unwrap();
        assert_eq!(snapshot["namespace"], "uac");
        assert_eq!(snapshot["generation"], 1);
        assert_eq!(snapshot["files"], 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn structured_commit_round_trips_json_and_preserves_data_hex() {
        let (root, storage) = temp_storage("structured-round-trip", 4096);
        let target = storage_capability_target("uac").unwrap();
        dispatch(
            &storage,
            &target,
            "commit",
            json!([[
                {
                    "op":"put",
                    "path":"users/usr_1.json",
                    "data":{
                        "userId":"usr_1",
                        "serviceId":"engine-studio",
                        "quotaBytes":104857600
                    }
                }
            ]]),
        )
        .unwrap();

        let read = dispatch(&storage, &target, "read", json!(["users/usr_1.json"])).unwrap();
        assert_eq!(read["found"], true);
        assert_eq!(read["data"]["userId"], "usr_1");
        assert_eq!(read["data"]["serviceId"], "engine-studio");
        assert_eq!(read["data"]["quotaBytes"], 104857600);
        assert!(read["dataHex"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn structured_put_rejects_missing_or_ambiguous_payloads() {
        let (root, storage) = temp_storage("structured-reject", 4096);
        let target = storage_capability_target("uac").unwrap();

        for mutation in [
            json!({"op":"put", "path":"missing"}),
            json!({"op":"put", "path":"both", "data_hex":"31", "data":1}),
        ] {
            let error = dispatch(&storage, &target, "commit", json!([[mutation]])).unwrap_err();
            assert_eq!(error.code, "CAPABILITY_STORAGE_ARGS_INVALID");
        }
        assert_eq!(storage.snapshot("uac").unwrap().generation, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_write_uses_frozen_root_and_creates_parent_directories() {
        let (root, storage) = temp_storage("project-write", 4096);
        let target = storage_capability_target("accounts").unwrap();
        let response = dispatch_project(
            &storage,
            &root,
            &target,
            "write",
            json!([{
                "path":"$$/data/users/kate.json",
                "data":{"name":"Kate","active":true},
                "encoding":"UTF8",
                "level":1
            }]),
        )
        .unwrap();
        assert_eq!(response["path"], "$$/data/users/kate.json");
        assert_eq!(response["encoding"], "UTF8");
        assert_eq!(response["level"], 1);
        let saved: Value =
            serde_json::from_slice(&std::fs::read(root.join("data/users/kate.json")).unwrap())
                .unwrap();
        assert_eq!(saved["name"], "Kate");
        assert_eq!(saved["active"], true);
        let intents = storage_sync_journal::ready_intents(&root).unwrap();
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].logical_path, "data/users/kate.json");
        assert_eq!(intents[0].level, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_write_supports_text_and_binary_encodings() {
        let (root, storage) = temp_storage("project-encodings", 4096);
        let target = storage_capability_target("media").unwrap();
        dispatch_project(
            &storage,
            &root,
            &target,
            "write",
            json!([{
                "path":"$$/utf16.txt",
                "data":"K8",
                "encoding":"UTF16LE",
                "level":2
            }]),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.join("utf16.txt")).unwrap(),
            vec![b'K', 0, b'8', 0]
        );

        dispatch_project(
            &storage,
            &root,
            &target,
            "write",
            json!([{
                "path":"$$/bytes.bin",
                "data":[0,1,127,255],
                "encoding":"BYTES",
                "level":3
            }]),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.join("bytes.bin")).unwrap(),
            vec![0, 1, 127, 255]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_write_rejects_escape_symlink_target_and_invalid_level() {
        let (root, storage) = temp_storage("project-closed", 4096);
        let target = storage_capability_target("accounts").unwrap();
        let escape = dispatch_project(
            &storage,
            &root,
            &target,
            "write",
            json!([{
                "path":"$$/../escape.txt",
                "data":"nope",
                "encoding":"UTF8",
                "level":1
            }]),
        )
        .unwrap_err();
        assert_eq!(escape.code, "CAPABILITY_STORAGE_PATH_INVALID");

        let invalid_level = dispatch_project(
            &storage,
            &root,
            &target,
            "write",
            json!([{
                "path":"$$/data/nope.txt",
                "data":"nope",
                "encoding":"UTF8",
                "level":4
            }]),
        )
        .unwrap_err();
        assert_eq!(invalid_level.code, "CAPABILITY_STORAGE_ARGS_INVALID");
        assert!(!root.join("data/nope.txt").exists());

        let internal = dispatch_project(
            &storage,
            &root,
            &target,
            "write",
            json!([{
                "path":"$$/.rbe/cloud-node/tamper.json",
                "data":"nope",
                "encoding":"UTF8",
                "level":1
            }]),
        )
        .unwrap_err();
        assert_eq!(internal.code, "CAPABILITY_STORAGE_PATH_INVALID");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_write_without_root_authority_fails_closed() {
        let (root, storage) = temp_storage("project-no-root", 4096);
        let target = storage_capability_target("accounts").unwrap();
        let error = dispatch(
            &storage,
            &target,
            "write",
            json!([{
                "path":"$$/data/nope.txt",
                "data":"nope",
                "encoding":"UTF8",
                "level":1
            }]),
        )
        .unwrap_err();
        assert_eq!(error.code, "CAPABILITY_STORAGE_PROJECT_ROOT_UNAVAILABLE");
        assert!(!root.join("data/nope.txt").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_mutation_aborts_the_entire_transaction() {
        let (root, storage) = temp_storage("atomic-reject", 4096);
        let target = storage_capability_target("cache").unwrap();
        let error = dispatch(
            &storage,
            &target,
            "commit",
            json!([[
                {"op":"put", "path":"good", "data_hex":"aa"},
                {"op":"put", "path":"../escape", "data_hex":"bb"}
            ]]),
        )
        .unwrap_err();
        assert_eq!(error.code, "CAPABILITY_STORAGE_CALL_FAILED");
        assert_eq!(storage.snapshot("cache").unwrap().generation, 0);
        assert!(storage.read("cache", "good").unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn target_and_operation_are_closed_and_response_limit_fails_closed() {
        let (root, storage) = temp_storage("closed", 4096);
        assert_eq!(
            storage_capability_target("../host").unwrap_err().code,
            "CAPABILITY_STORAGE_TARGET_INVALID"
        );
        let error =
            dispatch_storage_capability(&storage, "storage:uac", "open_host_file", b"[]", 1024)
                .unwrap_err();
        assert_eq!(error.code, "CAPABILITY_STORAGE_OPERATION_INVALID");

        dispatch(
            &storage,
            "storage:uac",
            "commit",
            json!([[{"op":"put", "path":"blob", "data_hex":"00112233445566778899"}]]),
        )
        .unwrap();
        let payload = serde_json::to_vec(&json!(["blob"])).unwrap();
        let error =
            dispatch_storage_capability(&storage, "storage:uac", "read", &payload, 8).unwrap_err();
        assert_eq!(error.code, "CAPABILITY_RESPONSE_TOO_LARGE");
        let _ = std::fs::remove_dir_all(root);
    }
}
