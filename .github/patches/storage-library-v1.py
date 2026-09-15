from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


# ---------------------------------------------------------------------------
# Container-owned typed Storage library.
# ---------------------------------------------------------------------------
storage_library = r'''use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::storage::{EnvironmentStorageManager, StorageCommit};

pub const STORAGE_LIBRARY_OPERATIONS: [&str; 8] = [
    "readBytes",
    "writeBytes",
    "readText",
    "writeText",
    "readJson",
    "writeJson",
    "exists",
    "remove",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageLibraryError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for StorageLibraryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for StorageLibraryError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Ascii,
    Latin1,
}

impl TextEncoding {
    fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16Le => "utf-16le",
            Self::Utf16Be => "utf-16be",
            Self::Ascii => "ascii",
            Self::Latin1 => "latin1",
        }
    }
}

pub fn storage_library_operation_allowed(operation: &str) -> bool {
    STORAGE_LIBRARY_OPERATIONS.contains(&operation)
}

/// Execute one typed file operation inside an exact Environment Storage
/// namespace. This layer never accepts host filesystem paths: `path` is always
/// a namespace-relative logical path validated by EnvironmentStorageManager.
pub fn dispatch_storage_library(
    storage: &Arc<EnvironmentStorageManager>,
    namespace: &str,
    operation: &str,
    args: &[Value],
) -> Result<Value, StorageLibraryError> {
    match operation {
        "readBytes" => {
            let path = path_only_args(args, "readBytes")?;
            match read_file(storage, namespace, &path)? {
                Some(bytes) => Ok(json!({
                    "found": true,
                    "path": path,
                    "bytes": bytes.len(),
                    "dataHex": hex::encode(bytes),
                })),
                None => Ok(json!({ "found": false, "path": path })),
            }
        }
        "writeBytes" => {
            let (path, data_hex) = write_bytes_args(args)?;
            let bytes = hex::decode(&data_hex).map_err(|_| {
                data_error("writeBytes dataHex must contain an even number of hexadecimal digits")
            })?;
            let bytes_len = bytes.len();
            let commit = put_file(storage, namespace, &path, &bytes)?;
            Ok(commit_response(commit, &path, bytes_len))
        }
        "readText" => {
            let (path, requested_encoding) = read_text_args(args, "readText")?;
            match read_file(storage, namespace, &path)? {
                Some(bytes) => {
                    let (text, encoding) = decode_text(&bytes, requested_encoding.as_deref())?;
                    Ok(json!({
                        "found": true,
                        "path": path,
                        "bytes": bytes.len(),
                        "encoding": encoding.label(),
                        "text": text,
                    }))
                }
                None => Ok(json!({ "found": false, "path": path })),
            }
        }
        "writeText" => {
            let (path, text, requested_encoding) = write_text_args(args)?;
            let encoding = parse_write_encoding(requested_encoding.as_deref())?;
            let bytes = encode_text(&text, encoding)?;
            let bytes_len = bytes.len();
            let commit = put_file(storage, namespace, &path, &bytes)?;
            let mut response = commit_response(commit, &path, bytes_len);
            response["encoding"] = Value::String(encoding.label().to_string());
            Ok(response)
        }
        "readJson" => {
            let (path, requested_encoding) = read_text_args(args, "readJson")?;
            match read_file(storage, namespace, &path)? {
                Some(bytes) => {
                    let (text, encoding) = decode_text(&bytes, requested_encoding.as_deref())?;
                    let value = serde_json::from_str::<Value>(&text).map_err(|error| {
                        json_error(format!("readJson could not decode JSON: {error}"))
                    })?;
                    Ok(json!({
                        "found": true,
                        "path": path,
                        "bytes": bytes.len(),
                        "encoding": encoding.label(),
                        "value": value,
                    }))
                }
                None => Ok(json!({ "found": false, "path": path })),
            }
        }
        "writeJson" => {
            let (path, value, requested_encoding, pretty) = write_json_args(args)?;
            let encoding = parse_write_encoding(requested_encoding.as_deref())?;
            let text = if pretty {
                serde_json::to_string_pretty(&value)
            } else {
                serde_json::to_string(&value)
            }
            .map_err(|error| json_error(format!("writeJson could not encode JSON: {error}")))?;
            let bytes = encode_text(&text, encoding)?;
            let bytes_len = bytes.len();
            let commit = put_file(storage, namespace, &path, &bytes)?;
            let mut response = commit_response(commit, &path, bytes_len);
            response["encoding"] = Value::String(encoding.label().to_string());
            response["pretty"] = Value::Bool(pretty);
            Ok(response)
        }
        "exists" => {
            let path = path_only_args(args, "exists")?;
            let exists = storage
                .exists(namespace, &path)
                .map_err(|_| storage_call_failed())?;
            Ok(json!({ "path": path, "exists": exists }))
        }
        "remove" => {
            let path = path_only_args(args, "remove")?;
            if !storage
                .exists(namespace, &path)
                .map_err(|_| storage_call_failed())?
            {
                return Ok(json!({ "path": path, "removed": false }));
            }
            let mut transaction = storage.begin(namespace).map_err(|_| storage_call_failed())?;
            transaction
                .delete(&path)
                .map_err(|_| storage_call_failed())?;
            let commit = transaction.commit().map_err(|_| storage_call_failed())?;
            Ok(json!({
                "path": path,
                "removed": true,
                "namespace": commit.namespace,
                "generation": commit.generation,
                "files": commit.files,
                "logicalBytes": commit.logical_bytes,
                "changedPaths": commit.changed_paths,
            }))
        }
        _ => Err(error(
            "CAPABILITY_STORAGE_OPERATION_INVALID",
            "Storage library operation is not supported",
        )),
    }
}

fn path_only_args(args: &[Value], operation: &str) -> Result<String, StorageLibraryError> {
    if let [Value::String(path)] = args {
        return Ok(path.clone());
    }
    if let [Value::Object(fields)] = args {
        return object_string(fields, "path", operation);
    }
    Err(args_error(format!(
        "{operation} expects a path string or one descriptor object"
    )))
}

fn read_text_args(
    args: &[Value],
    operation: &str,
) -> Result<(String, Option<String>), StorageLibraryError> {
    match args {
        [Value::String(path)] => Ok((path.clone(), None)),
        [Value::String(path), Value::String(encoding)] => {
            Ok((path.clone(), Some(encoding.clone())))
        }
        [Value::Object(fields)] => Ok((
            object_string(fields, "path", operation)?,
            object_optional_string(fields, "encoding", operation)?,
        )),
        _ => Err(args_error(format!(
            "{operation} expects (path[, encoding]) or one descriptor object"
        ))),
    }
}

fn write_bytes_args(args: &[Value]) -> Result<(String, String), StorageLibraryError> {
    match args {
        [Value::String(path), Value::String(data_hex)] => Ok((path.clone(), data_hex.clone())),
        [Value::Object(fields)] => {
            let path = object_string(fields, "path", "writeBytes")?;
            let data_hex = fields
                .get("dataHex")
                .or_else(|| fields.get("data_hex"))
                .and_then(Value::as_str)
                .ok_or_else(|| args_error("writeBytes descriptor requires string dataHex"))?;
            Ok((path, data_hex.to_string()))
        }
        _ => Err(args_error(
            "writeBytes expects (path, dataHex) or one descriptor object",
        )),
    }
}

fn write_text_args(
    args: &[Value],
) -> Result<(String, String, Option<String>), StorageLibraryError> {
    match args {
        [Value::String(path), Value::String(text)] => Ok((path.clone(), text.clone(), None)),
        [Value::String(path), Value::String(text), Value::String(encoding)] => {
            Ok((path.clone(), text.clone(), Some(encoding.clone())))
        }
        [Value::Object(fields)] => Ok((
            object_string(fields, "path", "writeText")?,
            object_string(fields, "text", "writeText")?,
            object_optional_string(fields, "encoding", "writeText")?,
        )),
        _ => Err(args_error(
            "writeText expects (path, text[, encoding]) or one descriptor object",
        )),
    }
}

fn write_json_args(
    args: &[Value],
) -> Result<(String, Value, Option<String>, bool), StorageLibraryError> {
    if let [Value::Object(fields)] = args {
        let path = object_string(fields, "path", "writeJson")?;
        let value = fields
            .get("value")
            .cloned()
            .ok_or_else(|| args_error("writeJson descriptor requires value"))?;
        let encoding = object_optional_string(fields, "encoding", "writeJson")?;
        let pretty = match fields.get("pretty") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(args_error("writeJson descriptor pretty must be boolean")),
        };
        return Ok((path, value, encoding, pretty));
    }
    if !(2..=4).contains(&args.len()) {
        return Err(args_error(
            "writeJson expects (path, value[, encoding[, pretty]]) or one descriptor object",
        ));
    }
    let path = args[0]
        .as_str()
        .ok_or_else(|| args_error("writeJson path must be a string"))?
        .to_string();
    let value = args[1].clone();
    let encoding = match args.get(2) {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => return Err(args_error("writeJson encoding must be a string")),
    };
    let pretty = match args.get(3) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(args_error("writeJson pretty must be boolean")),
    };
    Ok((path, value, encoding, pretty))
}

fn object_string(
    fields: &Map<String, Value>,
    key: &str,
    operation: &str,
) -> Result<String, StorageLibraryError> {
    fields
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| args_error(format!("{operation} descriptor requires string {key}")))
}

fn object_optional_string(
    fields: &Map<String, Value>,
    key: &str,
    operation: &str,
) -> Result<Option<String>, StorageLibraryError> {
    match fields.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(args_error(format!(
            "{operation} descriptor {key} must be a string"
        ))),
    }
}

fn read_file(
    storage: &EnvironmentStorageManager,
    namespace: &str,
    path: &str,
) -> Result<Option<Vec<u8>>, StorageLibraryError> {
    storage
        .read(namespace, path)
        .map_err(|_| storage_call_failed())
}

fn put_file(
    storage: &Arc<EnvironmentStorageManager>,
    namespace: &str,
    path: &str,
    bytes: &[u8],
) -> Result<StorageCommit, StorageLibraryError> {
    let mut transaction = storage.begin(namespace).map_err(|_| storage_call_failed())?;
    transaction
        .put(path, bytes)
        .map_err(|_| storage_call_failed())?;
    transaction.commit().map_err(|_| storage_call_failed())
}

fn commit_response(commit: StorageCommit, path: &str, bytes: usize) -> Value {
    json!({
        "path": path,
        "bytes": bytes,
        "namespace": commit.namespace,
        "generation": commit.generation,
        "files": commit.files,
        "logicalBytes": commit.logical_bytes,
        "changedPaths": commit.changed_paths,
    })
}

fn parse_write_encoding(requested: Option<&str>) -> Result<TextEncoding, StorageLibraryError> {
    let label = requested.unwrap_or("utf-8");
    if label.eq_ignore_ascii_case("auto") {
        return Err(encoding_error(
            "Storage write encoding cannot be auto; choose an explicit encoding",
        ));
    }
    parse_encoding(label)
}

fn parse_encoding(label: &str) -> Result<TextEncoding, StorageLibraryError> {
    let normalized = label.trim().to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "utf8" | "utf-8" => Ok(TextEncoding::Utf8),
        "utf16" | "utf-16" | "utf16le" | "utf-16le" => Ok(TextEncoding::Utf16Le),
        "utf16be" | "utf-16be" => Ok(TextEncoding::Utf16Be),
        "ascii" | "us-ascii" => Ok(TextEncoding::Ascii),
        "latin1" | "latin-1" | "iso-8859-1" => Ok(TextEncoding::Latin1),
        _ => Err(encoding_error(format!(
            "unsupported Storage text encoding {label:?}; expected utf-8, utf-16le, utf-16be, ascii, latin1, or auto for reads"
        ))),
    }
}

fn decode_text(
    bytes: &[u8],
    requested: Option<&str>,
) -> Result<(String, TextEncoding), StorageLibraryError> {
    let encoding = match requested {
        None => detect_encoding(bytes),
        Some(label) if label.eq_ignore_ascii_case("auto") => detect_encoding(bytes),
        Some(label) => parse_encoding(label)?,
    };
    let text = match encoding {
        TextEncoding::Utf8 => {
            let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
            String::from_utf8(bytes.to_vec())
                .map_err(|_| data_error("file is not valid UTF-8"))?
        }
        TextEncoding::Utf16Le => decode_utf16(bytes, true)?,
        TextEncoding::Utf16Be => decode_utf16(bytes, false)?,
        TextEncoding::Ascii => {
            if !bytes.is_ascii() {
                return Err(data_error("file contains bytes outside ASCII"));
            }
            bytes.iter().map(|byte| char::from(*byte)).collect()
        }
        TextEncoding::Latin1 => bytes.iter().map(|byte| char::from(*byte)).collect(),
    };
    Ok((text, encoding))
}

fn detect_encoding(bytes: &[u8]) -> TextEncoding {
    if bytes.starts_with(&[0xff, 0xfe]) {
        TextEncoding::Utf16Le
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        TextEncoding::Utf16Be
    } else {
        TextEncoding::Utf8
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<String, StorageLibraryError> {
    let bytes = if little_endian {
        bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes)
    } else {
        bytes.strip_prefix(&[0xfe, 0xff]).unwrap_or(bytes)
    };
    if bytes.len() % 2 != 0 {
        return Err(data_error("UTF-16 file has an odd byte length"));
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16(&units).map_err(|_| data_error("file contains invalid UTF-16"))
}

fn encode_text(text: &str, encoding: TextEncoding) -> Result<Vec<u8>, StorageLibraryError> {
    match encoding {
        TextEncoding::Utf8 => Ok(text.as_bytes().to_vec()),
        TextEncoding::Utf16Le => {
            let mut out = vec![0xff, 0xfe];
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
            Ok(out)
        }
        TextEncoding::Utf16Be => {
            let mut out = vec![0xfe, 0xff];
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_be_bytes());
            }
            Ok(out)
        }
        TextEncoding::Ascii => {
            if !text.is_ascii() {
                return Err(data_error("text contains characters outside ASCII"));
            }
            Ok(text.as_bytes().to_vec())
        }
        TextEncoding::Latin1 => {
            let mut out = Vec::with_capacity(text.len());
            for character in text.chars() {
                let value = u32::from(character);
                if value > 0xff {
                    return Err(data_error(
                        "text contains characters that cannot be represented in Latin-1",
                    ));
                }
                out.push(value as u8);
            }
            Ok(out)
        }
    }
}

fn args_error(message: impl Into<String>) -> StorageLibraryError {
    error("CAPABILITY_STORAGE_ARGS_INVALID", message)
}

fn encoding_error(message: impl Into<String>) -> StorageLibraryError {
    error("CAPABILITY_STORAGE_ENCODING_INVALID", message)
}

fn data_error(message: impl Into<String>) -> StorageLibraryError {
    error("CAPABILITY_STORAGE_DATA_INVALID", message)
}

fn json_error(message: impl Into<String>) -> StorageLibraryError {
    error("CAPABILITY_STORAGE_JSON_INVALID", message)
}

fn storage_call_failed() -> StorageLibraryError {
    error(
        "CAPABILITY_STORAGE_CALL_FAILED",
        "Environment Storage operation failed",
    )
}

fn error(code: &'static str, message: impl Into<String>) -> StorageLibraryError {
    StorageLibraryError {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn temp_storage(name: &str) -> (PathBuf, Arc<EnvironmentStorageManager>) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-storage-library-{name}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let storage = EnvironmentStorageManager::open(root.clone(), 1024 * 1024).unwrap();
        (root, storage)
    }

    fn call(
        storage: &Arc<EnvironmentStorageManager>,
        operation: &str,
        args: Value,
    ) -> Result<Value, StorageLibraryError> {
        dispatch_storage_library(
            storage,
            "accounts",
            operation,
            args.as_array().expect("test args must be array"),
        )
    }

    #[test]
    fn utf16_text_round_trip_and_auto_detection() {
        let (root, storage) = temp_storage("utf16");
        call(
            &storage,
            "writeText",
            json!(["notes/hello.txt", "Hello ✓", "utf-16le"]),
        )
        .unwrap();
        let raw = storage.read("accounts", "notes/hello.txt").unwrap().unwrap();
        assert!(raw.starts_with(&[0xff, 0xfe]));
        let read = call(&storage, "readText", json!(["notes/hello.txt", "auto"])).unwrap();
        assert_eq!(read["text"], "Hello ✓");
        assert_eq!(read["encoding"], "utf-16le");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn json_descriptor_round_trips_utf16be() {
        let (root, storage) = temp_storage("json");
        call(
            &storage,
            "writeJson",
            json!([{
                "path": "users/kate.json",
                "value": {"name":"Kate", "active":true},
                "encoding": "utf-16be",
                "pretty": true
            }]),
        )
        .unwrap();
        let read = call(&storage, "readJson", json!(["users/kate.json", "auto"])).unwrap();
        assert_eq!(read["value"], json!({"name":"Kate", "active":true}));
        assert_eq!(read["encoding"], "utf-16be");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bytes_cover_arbitrary_files_and_remove_is_scoped() {
        let (root, storage) = temp_storage("bytes");
        call(
            &storage,
            "writeBytes",
            json!([{"path":"assets/blob.bin", "dataHex":"00ff804142"}]),
        )
        .unwrap();
        let read = call(&storage, "readBytes", json!(["assets/blob.bin"])).unwrap();
        assert_eq!(read["dataHex"], "00ff804142");
        assert_eq!(call(&storage, "exists", json!(["assets/blob.bin"])).unwrap()["exists"], true);
        assert_eq!(call(&storage, "remove", json!(["assets/blob.bin"])).unwrap()["removed"], true);
        assert_eq!(call(&storage, "exists", json!(["assets/blob.bin"])).unwrap()["exists"], false);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn incompatible_text_encodings_fail_closed() {
        let (root, storage) = temp_storage("encoding-reject");
        let error = call(
            &storage,
            "writeText",
            json!(["notes/ascii.txt", "not-ascii-✓", "ascii"]),
        )
        .unwrap_err();
        assert_eq!(error.code, "CAPABILITY_STORAGE_DATA_INVALID");
        assert!(storage.read("accounts", "notes/ascii.txt").unwrap().is_none());

        let error = call(
            &storage,
            "writeText",
            json!(["notes/unknown.txt", "value", "shift-jis"]),
        )
        .unwrap_err();
        assert_eq!(error.code, "CAPABILITY_STORAGE_ENCODING_INVALID");
        let _ = std::fs::remove_dir_all(root);
    }
}
'''
Path("container-runtime/crates/container-runtime-core/src/storage_library.rs").write_text(storage_library)


# storage.rs: cheap existence checks stay manifest-only.
path = Path("container-runtime/crates/container-runtime-core/src/storage.rs")
text = path.read_text()
text = replace_once(
    text,
    """    pub fn list(&self, namespace: &str) -> Result<Vec<String>> {""",
    """    pub fn exists(&self, namespace: &str, path: &str) -> Result<bool> {
        validate_namespace(namespace)?;
        let path = normalize_relative_path(path)?;
        let _guard = self
            .commit_lock
            .lock()
            .expect("storage commit lock poisoned");
        Ok(self.load_manifest(namespace)?.files.contains_key(&path))
    }

    pub fn list(&self, namespace: &str) -> Result<Vec<String>> {""",
    "Environment Storage exists method",
)
path.write_text(text)


# lib.rs: register/export the typed library layer.
path = Path("container-runtime/crates/container-runtime-core/src/lib.rs")
text = path.read_text()
text = replace_once(
    text,
    """mod storage;
mod storage_capability;
mod swamp;""",
    """mod storage;
mod storage_capability;
mod storage_library;
mod swamp;""",
    "Storage library module registration",
)
text = replace_once(
    text,
    """pub use storage_capability::{
    dispatch_storage_capability, storage_capability_operation_allowed, storage_capability_target,
    StorageCapabilityError, STORAGE_CAPABILITY_OPERATIONS, STORAGE_CAPABILITY_TARGET_PREFIX,
};""",
    """pub use storage_capability::{
    dispatch_storage_capability, storage_capability_operation_allowed, storage_capability_target,
    StorageCapabilityError, STORAGE_CAPABILITY_OPERATIONS, STORAGE_CAPABILITY_TARGET_PREFIX,
};
pub use storage_library::{
    dispatch_storage_library, storage_library_operation_allowed, StorageLibraryError,
    STORAGE_LIBRARY_OPERATIONS,
};""",
    "Storage library exports",
)
path.write_text(text)


# storage_capability.rs: preserve the raw protocol and delegate typed file ops.
path = Path("container-runtime/crates/container-runtime-core/src/storage_capability.rs")
text = path.read_text()
text = replace_once(
    text,
    """use crate::EnvironmentStorageManager;

pub const STORAGE_CAPABILITY_TARGET_PREFIX: &str = "storage:";
pub const STORAGE_CAPABILITY_OPERATIONS: [&str; 4] = ["read", "list", "snapshot", "commit"];""",
    """use crate::storage_library::{
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
];""",
    "Storage capability typed operations",
)
text = replace_once(
    text,
    """        "commit" => {
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
        _ => unreachable!("operation allowlist checked above"),""",
    """        "commit" => {
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
        _ => unreachable!("operation allowlist checked above"),""",
    "Storage capability typed dispatcher",
)
text = replace_once(
    text,
    """fn invalid_args(message: &str) -> StorageCapabilityError {
    error("CAPABILITY_STORAGE_ARGS_INVALID", message)
}""",
    """fn storage_library_error(error: StorageLibraryError) -> StorageCapabilityError {
    StorageCapabilityError {
        code: error.code,
        message: error.message,
    }
}

fn invalid_args(message: &str) -> StorageCapabilityError {
    error("CAPABILITY_STORAGE_ARGS_INVALID", message)
}""",
    "Storage library error bridge",
)
path.write_text(text)


# Runtime Image: public Storage library and raw storage share one exact
# Controller capability kind, but RELC can distinguish their language surfaces.
path = Path("engine/crates/route-engine/src/runtime_image.rs")
text = path.read_text()
text = replace_once(
    text,
    """pub(crate) const STORAGE_CAPABILITY_TARGET_PREFIX: &str = "storage:";
pub(crate) const STORAGE_CAPABILITY_OPERATIONS: [&str; 4] = ["read", "list", "snapshot", "commit"];
pub(crate) const MAX_STORAGE_NAMESPACE_BYTES: usize = 64;

pub(crate) fn storage_capability_operation_allowed(operation: &str) -> bool {
    STORAGE_CAPABILITY_OPERATIONS.contains(&operation)
}""",
    """pub(crate) const STORAGE_CAPABILITY_TARGET_PREFIX: &str = "storage:";
pub(crate) const STORAGE_RAW_CAPABILITY_OPERATIONS: [&str; 4] =
    ["read", "list", "snapshot", "commit"];
pub(crate) const STORAGE_LIBRARY_OPERATIONS: [&str; 10] = [
    "readBytes",
    "writeBytes",
    "readText",
    "writeText",
    "readJson",
    "writeJson",
    "exists",
    "remove",
    "list",
    "snapshot",
];
pub(crate) const STORAGE_CAPABILITY_OPERATIONS: [&str; 12] = [
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
pub(crate) const MAX_STORAGE_NAMESPACE_BYTES: usize = 64;

pub(crate) fn storage_capability_operation_allowed(operation: &str) -> bool {
    STORAGE_CAPABILITY_OPERATIONS.contains(&operation)
}

pub(crate) fn storage_raw_operation_allowed(operation: &str) -> bool {
    STORAGE_RAW_CAPABILITY_OPERATIONS.contains(&operation)
}

pub(crate) fn storage_library_operation_allowed(operation: &str) -> bool {
    STORAGE_LIBRARY_OPERATIONS.contains(&operation)
}""",
    "Runtime Image Storage library surface",
)
path.write_text(text)


# RELC: uppercase Storage is the friendly Module-only library. Lowercase
# storage remains the low-level exact raw capability surface.
path = Path("engine/crates/route-engine/src/relc.rs")
text = path.read_text()
text = replace_once(
    text,
    """use crate::runtime_image::{
    stable_image_hash, stable_source_hash, storage_capability_operation_allowed,
    storage_capability_owner_allowed, RuntimeCapabilityRequirement, RuntimeExecutable,
    RuntimeImage, RuntimeSourceManifest, STORAGE_CAPABILITY_OPERATIONS,
};""",
    """use crate::runtime_image::{
    stable_image_hash, stable_source_hash, storage_capability_operation_allowed,
    storage_capability_owner_allowed, storage_library_operation_allowed,
    storage_raw_operation_allowed, RuntimeCapabilityRequirement, RuntimeExecutable, RuntimeImage,
    RuntimeSourceManifest, STORAGE_CAPABILITY_OPERATIONS, STORAGE_LIBRARY_OPERATIONS,
    STORAGE_RAW_CAPABILITY_OPERATIONS,
};""",
    "RELC Storage language imports",
)
old_validation = r'''            if name == "storage" {
                if kind != RelSourceKind::Module {
                    return Err(RelcError::Capability {
                        code: "RELC2101",
                        source: source.clone(),
                        message: "Environment Storage authority is Module-owned; import an exported Module function instead of using Storage directly"
                            .into(),
                    });
                }
                match base {
                    ImportTarget::Builtin(_) => {
                        return Err(RelcError::Capability {
                            code: "RELC2102",
                            source: source.clone(),
                            message: "Storage namespace imports are forbidden; import one exact operation such as `storage.read`"
                                .into(),
                        });
                    }
                    ImportTarget::BuiltinFunction { function, .. }
                        if !storage_capability_operation_allowed(function) =>
                    {
                        return Err(RelcError::Capability {
                            code: "RELC2102",
                            source: source.clone(),
                            message: format!(
                                "unsupported Environment Storage operation {function:?}; expected one of {:?}",
                                STORAGE_CAPABILITY_OPERATIONS
                            ),
                        });
                    }
                    _ => {}
                }
            }
'''
new_validation = r'''            if matches!(name.as_str(), "storage" | "Storage") {
                if kind != RelSourceKind::Module {
                    return Err(RelcError::Capability {
                        code: "RELC2101",
                        source: source.clone(),
                        message: "Environment Storage authority is Module-owned; import an exported Module function instead of using Storage directly"
                            .into(),
                    });
                }
                match base {
                    ImportTarget::Builtin(_) => {
                        return Err(RelcError::Capability {
                            code: "RELC2102",
                            source: source.clone(),
                            message: if name == "Storage" {
                                "Storage namespace imports are forbidden; import one exact function such as `Storage.readJson`"
                                    .into()
                            } else {
                                "Storage namespace imports are forbidden; import one exact operation such as `storage.read`"
                                    .into()
                            },
                        });
                    }
                    ImportTarget::BuiltinFunction { function, .. }
                        if name == "storage" && !storage_raw_operation_allowed(function) =>
                    {
                        return Err(RelcError::Capability {
                            code: "RELC2102",
                            source: source.clone(),
                            message: format!(
                                "unsupported low-level Environment Storage operation {function:?}; expected one of {:?}",
                                STORAGE_RAW_CAPABILITY_OPERATIONS
                            ),
                        });
                    }
                    ImportTarget::BuiltinFunction { function, .. }
                        if name == "Storage" && !storage_library_operation_allowed(function) =>
                    {
                        return Err(RelcError::Capability {
                            code: "RELC2102",
                            source: source.clone(),
                            message: format!(
                                "unsupported Storage library function {function:?}; expected one of {:?}",
                                STORAGE_LIBRARY_OPERATIONS
                            ),
                        });
                    }
                    _ => {}
                }
            }
'''
text = replace_once(text, old_validation, new_validation, "RELC Storage capability validation")
text = replace_once(
    text,
    """    let operations: &[&str] = match module {
        "http" => HTTP_HOST_OPERATIONS,
        "storage" => &STORAGE_CAPABILITY_OPERATIONS,
        "vm" | "video-manager" => VIDEO_LANGUAGE_OPERATIONS,
        _ => return Ok(BTreeSet::new()),
    };
    let module_owner = if matches!(module, "storage" | "vm" | "video-manager") {""",
    """    let operations: &[&str] = match module {
        "http" => HTTP_HOST_OPERATIONS,
        "storage" => &STORAGE_RAW_CAPABILITY_OPERATIONS,
        "Storage" => &STORAGE_LIBRARY_OPERATIONS,
        "vm" | "video-manager" => VIDEO_LANGUAGE_OPERATIONS,
        _ => return Ok(BTreeSet::new()),
    };
    let module_owner = if matches!(module, "storage" | "Storage" | "vm" | "video-manager") {""",
    "RELC Storage host requirements",
)
text = replace_once(
    text,
    """        if module == "storage" && !storage_capability_owner_allowed(owner) {""",
    """        if matches!(module, "storage" | "Storage") && !storage_capability_owner_allowed(owner) {""",
    "RELC Storage principal validation",
)
text = replace_once(
    text,
    """            "storage" => RuntimeCapabilityRequirement::Storage {
                owner: module_owner
                    .expect("Storage owner validated above")
                    .to_string(),
                operation: operation.to_string(),
            },""",
    """            "storage" | "Storage" => RuntimeCapabilityRequirement::Storage {
                owner: module_owner
                    .expect("Storage owner validated above")
                    .to_string(),
                operation: operation.to_string(),
            },""",
    "RELC Storage requirement lowering",
)
path.write_text(text)


# wasm_compiler.rs: linked uppercase Storage functions lower to the same exact
# Storage capability target, retaining the Module owner as namespace authority.
path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()
text = replace_once(
    text,
    """        ImportTarget::BuiltinFunction { module, function }
            if module == "storage" && storage_capability_operation_allowed(function) =>""",
    """        ImportTarget::BuiltinFunction { module, function }
            if matches!(module.as_str(), "storage" | "Storage")
                && storage_capability_operation_allowed(function) =>""",
    "Route-WASM Storage library lowering",
)
anchor = """    #[test]
    fn linked_module_storage_call_uses_canonical_module_owner() {"""
if anchor not in text:
    raise SystemExit("Storage compiler test anchor missing")
new_test = r'''    #[test]
    fn linked_storage_library_json_read_uses_exact_module_namespace() {
        let module = parse_module(
            r#":import[Storage.readJson as readProfile]
               export function load(path) { return readProfile(path); }"#,
        );
        let links = link_module_function("load", "accounts.profile", &module, "load");
        let route = parse(
            r#":import["./module/accounts/profile".load]
               class Route { get(req) { return load("users/kate.json"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("Storage.readJson wrapper should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Storage);
            assert_eq!(request.target, "storage:accounts.profile");
            assert_eq!(request.operation, "readJson");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["users/kate.json"])
            );
            Ok(br#"{"found":true,"value":{"name":"Kate"}}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(
            result.output,
            br#"{"found":true,"value":{"name":"Kate"}}"#
        );
    }

'''
text = text.replace(anchor, new_test + anchor, 1)
path.write_text(text)


# modules.rs: analyzer/runtime registry knows both the compatibility raw storage
# surface and the friendly async Storage library surface.
path = Path("engine/crates/route-engine/src/modules.rs")
text = path.read_text()
text = replace_once(
    text,
    """    Response,
    VideoManager,
}""",
    """    Response,
    Storage,
    VideoManager,
}""",
    "Storage builtin module enum",
)
text = replace_once(
    text,
    """        "response" => matches!(
            function,
            "json"
                | "text"
                | "html"
                | "status"
                | "noContent"
                | "no_content"
                | "redirect"
                | "withHeader"
                | "with_header"
                | "cookie"
                | "clearCookie"
                | "clear_cookie"
        ),
        "vm" | "video-manager" => matches!(""",
    """        "response" => matches!(
            function,
            "json"
                | "text"
                | "html"
                | "status"
                | "noContent"
                | "no_content"
                | "redirect"
                | "withHeader"
                | "with_header"
                | "cookie"
                | "clearCookie"
                | "clear_cookie"
        ),
        "storage" => matches!(function, "read" | "list" | "snapshot" | "commit"),
        "Storage" => matches!(
            function,
            "readBytes"
                | "writeBytes"
                | "readText"
                | "writeText"
                | "readJson"
                | "writeJson"
                | "exists"
                | "remove"
                | "list"
                | "snapshot"
        ),
        "vm" | "video-manager" => matches!(""",
    "Storage builtin function registry",
)
# Two registry match blocks use the same module-name shape.
text = text.replace(
    '                        "response" => ModuleKind::Builtin(BuiltinModule::Response),\n                        "vm" | "video-manager" => ModuleKind::Builtin(BuiltinModule::VideoManager),',
    '                        "response" => ModuleKind::Builtin(BuiltinModule::Response),\n                        "storage" | "Storage" => ModuleKind::Builtin(BuiltinModule::Storage),\n                        "vm" | "video-manager" => ModuleKind::Builtin(BuiltinModule::VideoManager),',
)
if text.count('BuiltinModule::Storage') != 2:
    raise SystemExit(f"Storage registry mappings expected two, found {text.count('BuiltinModule::Storage')}")
text = replace_once(
    text,
    """            ModuleKind::Builtin(BuiltinModule::Response) => call_response(function_name, args),
            ModuleKind::Builtin(BuiltinModule::VideoManager) => Err(ModuleError {""",
    """            ModuleKind::Builtin(BuiltinModule::Response) => call_response(function_name, args),
            ModuleKind::Builtin(BuiltinModule::Storage) => Err(ModuleError {
                message: format!(
                    "{module_name}.{function_name}() requires the Container-owned Storage capability"
                ),
            }),
            ModuleKind::Builtin(BuiltinModule::VideoManager) => Err(ModuleError {""",
    "Storage builtin async registry behavior",
)
path.write_text(text)
