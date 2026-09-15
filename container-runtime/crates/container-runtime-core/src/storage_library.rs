use std::sync::Arc;

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
            let mut transaction = storage
                .begin(namespace)
                .map_err(|_| storage_call_failed())?;
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
    let mut transaction = storage
        .begin(namespace)
        .map_err(|_| storage_call_failed())?;
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
            String::from_utf8(bytes.to_vec()).map_err(|_| data_error("file is not valid UTF-8"))?
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
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes(*chunk)
            } else {
                u16::from_be_bytes(*chunk)
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
        let raw = storage
            .read("accounts", "notes/hello.txt")
            .unwrap()
            .unwrap();
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
        assert_eq!(
            call(&storage, "exists", json!(["assets/blob.bin"])).unwrap()["exists"],
            true
        );
        assert_eq!(
            call(&storage, "remove", json!(["assets/blob.bin"])).unwrap()["removed"],
            true
        );
        assert_eq!(
            call(&storage, "exists", json!(["assets/blob.bin"])).unwrap()["exists"],
            false
        );
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
        assert!(storage
            .read("accounts", "notes/ascii.txt")
            .unwrap()
            .is_none());

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
