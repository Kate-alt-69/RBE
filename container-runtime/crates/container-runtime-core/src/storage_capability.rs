use std::sync::Arc;

use ipc_protocol::MAX_CAPABILITY_PAYLOAD_BYTES;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::storage_library::{
    dispatch_storage_library, storage_library_operation_allowed, StorageLibraryError,
};
use crate::EnvironmentStorageManager;

pub const STORAGE_CAPABILITY_TARGET_PREFIX: &str = "storage:";
pub const STORAGE_CAPABILITY_OPERATIONS: [&str; 12] = [
    "read",
    "list",
    "snapshot",
    "commit",
    "readBytes",
    "writeBytes",
    "readText",
    "writeText",
    "readJson",
    "writeJson",
    "exists",
    "remove",
];
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
    Put { path: String, data_hex: String },
    Delete { path: String },
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
/// The target carries the namespace (`storage:<namespace>`), so namespace
/// authority is grant-exact rather than selected from untrusted payload data.
/// Payloads follow the guest capability ABI convention and are JSON argument
/// arrays. `commit` applies all requested mutations through one
/// `StorageTransaction`, preserving all-or-nothing namespace publication.
pub fn dispatch_storage_capability(
    storage: &Arc<EnvironmentStorageManager>,
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
                Some(bytes) => json!({
                    "found": true,
                    "dataHex": hex::encode(bytes),
                }),
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
                    StorageMutationRequest::Put { path, data_hex } => {
                        let bytes = hex::decode(data_hex)
                            .map_err(|_| invalid_args("put dataHex must be hexadecimal"))?;
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
        operation if storage_library_operation_allowed(operation) => {
            dispatch_storage_library(storage, namespace, operation, &args)
                .map_err(storage_library_error)?
        }
        _ => unreachable!("operation allowlist checked above"),
    };

    encode_response(&response, max_response_bytes)
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

fn storage_library_error(error: StorageLibraryError) -> StorageCapabilityError {
    StorageCapabilityError {
        code: error.code,
        message: error.message,
    }
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

        let listed = dispatch(&storage, &target, "list", json!([])).unwrap();
        assert_eq!(listed["entries"].as_array().unwrap().len(), 2);
        let snapshot = dispatch(&storage, &target, "snapshot", json!([])).unwrap();
        assert_eq!(snapshot["namespace"], "uac");
        assert_eq!(snapshot["generation"], 1);
        assert_eq!(snapshot["files"], 2);
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
