from pathlib import Path

ROOT = Path('.')


def replace_once(path: str, old: str, new: str) -> None:
    file = ROOT / path
    text = file.read_text(encoding='utf-8')
    count = text.count(old)
    if count != 1:
        raise SystemExit(f'{path}: expected exactly one anchor, found {count}: {old[:120]!r}')
    file.write_text(text.replace(old, new, 1), encoding='utf-8')


PROJECT_STORAGE = r'''use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ipc_protocol::MAX_CAPABILITY_PAYLOAD_BYTES;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub const PROJECT_STORAGE_CAPABILITY_TARGET_PREFIX: &str = "project-storage:";
pub const PROJECT_STORAGE_OPERATIONS: [&str; 5] = ["read", "write", "exists", "remove", "list"];
const MAX_PROJECT_PATH_BYTES: usize = 1024;
const MAX_OWNER_BYTES: usize = 64;
const INTERNAL_DIR: &str = ".rbe";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataLevel(u8);

impl DataLevel {
    pub const FIRST: Self = Self(1);
    pub const SECOND: Self = Self(2);
    pub const THIRD: Self = Self(3);

    pub fn get(self) -> u8 {
        self.0
    }

    fn parse(value: Option<&Value>) -> Result<Self, ProjectStorageError> {
        let Some(value) = value else {
            return Ok(Self::SECOND);
        };
        let level = match value {
            Value::Number(number) => number
                .as_u64()
                .and_then(|value| u8::try_from(value).ok()),
            Value::String(value) => value.parse::<u8>().ok(),
            _ => None,
        }
        .ok_or_else(|| invalid_args("storage data level must be 1, 2, or 3"))?;
        match level {
            1 => Ok(Self::FIRST),
            2 => Ok(Self::SECOND),
            3 => Ok(Self::THIRD),
            _ => Err(invalid_args("storage data level must be 1, 2, or 3")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectStorageError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for ProjectStorageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProjectStorageError {}

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

#[derive(Debug)]
pub struct ProjectStorageManager {
    root: PathBuf,
    io: atomic_io::AtomicIo,
    lock: Mutex<()>,
}

impl ProjectStorageManager {
    pub fn open(root: PathBuf) -> anyhow::Result<Arc<Self>> {
        if !root.is_dir() {
            anyhow::bail!("RBE project root is not a directory: {}", root.display());
        }
        let root = root
            .canonicalize()
            .map_err(|error| anyhow::anyhow!("canonicalize RBE project root: {error}"))?;
        Ok(Arc::new(Self {
            root,
            io: atomic_io::AtomicIo::new(),
            lock: Mutex::new(()),
        }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn read_file(&self, raw_path: &str) -> Result<Option<Vec<u8>>, ProjectStorageError> {
        let _guard = self.lock.lock().map_err(|_| storage_failed())?;
        let Some(path) = self.resolve_existing(raw_path, false)? else {
            return Ok(None);
        };
        self.io.read(&path).map(Some).map_err(|_| storage_failed())
    }

    fn write_file(
        &self,
        raw_path: &str,
        bytes: &[u8],
        level: DataLevel,
        owner: &str,
    ) -> Result<PathBuf, ProjectStorageError> {
        let _guard = self.lock.lock().map_err(|_| storage_failed())?;
        let (relative, target) = self.prepare_write_target(raw_path)?;
        // Write scheduling metadata first. If the data write then fails, Cloud Node
        // simply ignores the orphan policy record because the referenced path is
        // absent. The reverse ordering could lose a requested level-1 hint after
        // a crash between the data commit and policy publication.
        self.write_level_record(&relative, level, owner)?;
        self.io
            .write_atomic(&target, bytes)
            .map_err(|_| storage_failed())?;
        Ok(target)
    }

    fn exists(&self, raw_path: &str) -> Result<bool, ProjectStorageError> {
        let _guard = self.lock.lock().map_err(|_| storage_failed())?;
        Ok(self.resolve_existing(raw_path, true)?.is_some())
    }

    fn remove(&self, raw_path: &str) -> Result<bool, ProjectStorageError> {
        let _guard = self.lock.lock().map_err(|_| storage_failed())?;
        let (relative, _) = project_relative(raw_path, false)?;
        let Some(path) = self.resolve_existing(raw_path, false)? else {
            return Ok(false);
        };
        fs::remove_file(path).map_err(|_| storage_failed())?;
        let policy = self.level_record_path(&relative);
        match fs::remove_file(policy) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(storage_failed()),
        }
        Ok(true)
    }

    fn list(&self, raw_path: Option<&str>) -> Result<Vec<String>, ProjectStorageError> {
        let _guard = self.lock.lock().map_err(|_| storage_failed())?;
        let raw_path = raw_path.unwrap_or("$$/");
        let (relative, target) = project_relative(raw_path, true)?;
        let target = if relative.as_os_str().is_empty() {
            self.root.clone()
        } else {
            let Some(target) = self.resolve_existing(raw_path, true)? else {
                return Ok(Vec::new());
            };
            target
        };
        if !target.is_dir() {
            return Err(invalid_args("storage.list path must name a directory"));
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&target).map_err(|_| storage_failed())? {
            let entry = entry.map_err(|_| storage_failed())?;
            let name = entry.file_name().to_string_lossy().to_string();
            if relative.as_os_str().is_empty() && name == INTERNAL_DIR {
                continue;
            }
            let path = if relative.as_os_str().is_empty() {
                name
            } else {
                format!("{}/{}", relative.to_string_lossy().replace('\\', "/"), name)
            };
            entries.push(format!("$$/{path}"));
        }
        entries.sort();
        Ok(entries)
    }

    fn prepare_write_target(
        &self,
        raw_path: &str,
    ) -> Result<(PathBuf, PathBuf), ProjectStorageError> {
        let (relative, target) = project_relative(raw_path, false)?;
        let parent = relative
            .parent()
            .ok_or_else(|| invalid_args("storage write path has no parent"))?;
        let mut cursor = self.root.clone();
        for component in parent.components() {
            cursor.push(component.as_os_str());
            match fs::symlink_metadata(&cursor) {
                Ok(metadata) => {
                    reject_link_or_reparse(&metadata)?;
                    if !metadata.is_dir() {
                        return Err(invalid_args("storage write parent is not a directory"));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&cursor).map_err(|_| storage_failed())?;
                    let metadata = fs::symlink_metadata(&cursor).map_err(|_| storage_failed())?;
                    reject_link_or_reparse(&metadata)?;
                }
                Err(_) => return Err(storage_failed()),
            }
            let canonical = cursor.canonicalize().map_err(|_| storage_failed())?;
            if !canonical.starts_with(&self.root) {
                return Err(path_escape());
            }
        }
        if let Ok(metadata) = fs::symlink_metadata(&target) {
            reject_link_or_reparse(&metadata)?;
            if metadata.is_dir() {
                return Err(invalid_args("storage.write target is a directory"));
            }
            let canonical = target.canonicalize().map_err(|_| storage_failed())?;
            if !canonical.starts_with(&self.root) {
                return Err(path_escape());
            }
        }
        Ok((relative, target))
    }

    fn resolve_existing(
        &self,
        raw_path: &str,
        allow_directory: bool,
    ) -> Result<Option<PathBuf>, ProjectStorageError> {
        let (_, target) = project_relative(raw_path, allow_directory)?;
        let metadata = match fs::symlink_metadata(&target) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(storage_failed()),
        };
        reject_link_or_reparse(&metadata)?;
        if !allow_directory && !metadata.is_file() {
            return Err(invalid_args("storage path must name a file"));
        }
        let canonical = target.canonicalize().map_err(|_| storage_failed())?;
        if !canonical.starts_with(&self.root) {
            return Err(path_escape());
        }
        Ok(Some(canonical))
    }

    fn level_record_path(&self, relative: &Path) -> PathBuf {
        let normalized = relative.to_string_lossy().replace('\\', "/");
        let hash = hex::encode(Sha256::digest(normalized.as_bytes()));
        self.root
            .join(INTERNAL_DIR)
            .join("data-levels")
            .join("exact")
            .join(format!("{hash}.json"))
    }

    fn write_level_record(
        &self,
        relative: &Path,
        level: DataLevel,
        owner: &str,
    ) -> Result<(), ProjectStorageError> {
        let normalized = relative.to_string_lossy().replace('\\', "/");
        let record = json!({
            "version": 1,
            "scope": "project",
            "path": format!("$$/{normalized}"),
            "level": level.get(),
            "owner": owner,
            "source": "storage",
        });
        let bytes = serde_json::to_vec(&record).map_err(|_| storage_failed())?;
        self.io
            .write_atomic(&self.level_record_path(relative), &bytes)
            .map_err(|_| storage_failed())
    }
}

pub fn project_storage_operation_allowed(operation: &str) -> bool {
    PROJECT_STORAGE_OPERATIONS.contains(&operation)
}

pub fn project_storage_target(owner: &str) -> Result<String, ProjectStorageError> {
    validate_owner(owner)?;
    Ok(format!("{PROJECT_STORAGE_CAPABILITY_TARGET_PREFIX}{owner}"))
}

pub fn dispatch_project_storage_capability(
    storage: &Arc<ProjectStorageManager>,
    target: &str,
    operation: &str,
    payload: &[u8],
    max_response_bytes: u64,
) -> Result<Vec<u8>, ProjectStorageError> {
    if payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES {
        return Err(error(
            "CAPABILITY_REQUEST_TOO_LARGE",
            "storage capability request exceeded the protocol limit",
        ));
    }
    if !project_storage_operation_allowed(operation) {
        return Err(error(
            "CAPABILITY_STORAGE_OPERATION_INVALID",
            "project storage operation is not supported",
        ));
    }
    let owner = target
        .strip_prefix(PROJECT_STORAGE_CAPABILITY_TARGET_PREFIX)
        .ok_or_else(|| error("CAPABILITY_STORAGE_TARGET_INVALID", "invalid project storage target"))?;
    validate_owner(owner)?;
    let args: Vec<Value> = serde_json::from_slice(payload)
        .map_err(|_| invalid_args("storage payload must be a JSON argument array"))?;

    let response = match operation {
        "write" => {
            let options = parse_write_options(&args)?;
            let bytes = encode_write_data(&options)?;
            let path = option_string(&options, &["write", "path"])?;
            let level = DataLevel::parse(options.get("level"))?;
            storage.write_file(&path, &bytes, level, owner)?;
            json!({
                "path": path,
                "bytes": bytes.len(),
                "level": level.get(),
                "atomic": true,
                "root": "$$",
            })
        }
        "read" => {
            let options = parse_read_options(&args)?;
            let path = option_string(&options, &["read", "path"])?;
            let format = option_string_optional(&options, "format")
                .unwrap_or_else(|| "text".to_string());
            match storage.read_file(&path)? {
                None => json!({"found": false, "path": path}),
                Some(bytes) if format.eq_ignore_ascii_case("bytes") => json!({
                    "found": true,
                    "path": path,
                    "bytes": bytes.len(),
                    "dataHex": hex::encode(bytes),
                }),
                Some(bytes) => {
                    let requested = option_string_optional(&options, "encode")
                        .or_else(|| option_string_optional(&options, "encoding"));
                    let (text, encoding) = decode_text(&bytes, requested.as_deref())?;
                    if format.eq_ignore_ascii_case("json") {
                        let value: Value = serde_json::from_str(&text)
                            .map_err(|_| data_error("storage.read JSON is invalid"))?;
                        json!({
                            "found": true,
                            "path": path,
                            "bytes": bytes.len(),
                            "encoding": encoding.label(),
                            "value": value,
                        })
                    } else if format.eq_ignore_ascii_case("text") || format.eq_ignore_ascii_case("auto") {
                        json!({
                            "found": true,
                            "path": path,
                            "bytes": bytes.len(),
                            "encoding": encoding.label(),
                            "text": text,
                        })
                    } else {
                        return Err(invalid_args("storage.read format must be text, json, auto, or bytes"));
                    }
                }
            }
        }
        "exists" => {
            let path = single_path_arg(&args, &["exists", "read", "path"])?;
            json!({"path": path, "exists": storage.exists(&path)?})
        }
        "remove" => {
            let path = single_path_arg(&args, &["remove", "write", "path"])?;
            json!({"path": path, "removed": storage.remove(&path)?})
        }
        "list" => {
            let path = if args.is_empty() {
                None
            } else {
                Some(single_path_arg(&args, &["list", "read", "path"] )?)
            };
            json!({"entries": storage.list(path.as_deref())?})
        }
        _ => unreachable!("operation allowlist checked above"),
    };
    encode_response(&response, max_response_bytes)
}

fn parse_write_options(args: &[Value]) -> Result<Map<String, Value>, ProjectStorageError> {
    if args.len() >= 2 && args[0].is_string() {
        let mut out = Map::new();
        out.insert("write".into(), args[0].clone());
        out.insert("data".into(), args[1].clone());
        if let Some(value) = args.get(2) {
            out.insert("encode".into(), value.clone());
        }
        if let Some(value) = args.get(3) {
            out.insert("level".into(), value.clone());
        }
        if args.len() > 4 {
            return Err(invalid_args("storage.write positional form accepts path, data, optional encoding, optional level"));
        }
        return Ok(out);
    }
    merge_tagged_args(args)
}

fn parse_read_options(args: &[Value]) -> Result<Map<String, Value>, ProjectStorageError> {
    if !args.is_empty() && args[0].is_string() {
        let mut out = Map::new();
        out.insert("read".into(), args[0].clone());
        if let Some(value) = args.get(1) {
            out.insert("encode".into(), value.clone());
        }
        if let Some(value) = args.get(2) {
            out.insert("format".into(), value.clone());
        }
        if args.len() > 3 {
            return Err(invalid_args("storage.read positional form accepts path, optional encoding, optional format"));
        }
        return Ok(out);
    }
    merge_tagged_args(args)
}

fn merge_tagged_args(args: &[Value]) -> Result<Map<String, Value>, ProjectStorageError> {
    if args.is_empty() {
        return Err(invalid_args("storage operation requires arguments"));
    }
    if let [Value::Object(fields)] = args {
        if fields.len() > 1 || fields.contains_key("path") {
            return Ok(fields.clone());
        }
    }
    let mut out = Map::new();
    for arg in args {
        let Value::Object(fields) = arg else {
            return Err(invalid_args("tagged storage arguments must be one-key objects"));
        };
        if fields.len() != 1 {
            return Err(invalid_args("each tagged storage argument must contain exactly one field"));
        }
        let (key, value) = fields.iter().next().expect("one field");
        if out.insert(key.clone(), value.clone()).is_some() {
            return Err(invalid_args("duplicate storage argument tag"));
        }
    }
    Ok(out)
}

fn single_path_arg(args: &[Value], keys: &[&str]) -> Result<String, ProjectStorageError> {
    if let [Value::String(path)] = args {
        return Ok(path.clone());
    }
    let options = merge_tagged_args(args)?;
    option_string(&options, keys)
}

fn option_string(options: &Map<String, Value>, keys: &[&str]) -> Result<String, ProjectStorageError> {
    keys.iter()
        .find_map(|key| options.get(*key).and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .ok_or_else(|| invalid_args(format!("storage argument requires one of {keys:?} as a string")))
}

fn option_string_optional(options: &Map<String, Value>, key: &str) -> Option<String> {
    options.get(key).and_then(Value::as_str).map(ToOwned::to_owned)
}

fn encode_write_data(options: &Map<String, Value>) -> Result<Vec<u8>, ProjectStorageError> {
    let format = option_string_optional(options, "format").unwrap_or_else(|| "auto".to_string());
    if format.eq_ignore_ascii_case("bytes") {
        let data_hex = options
            .get("dataHex")
            .or_else(|| options.get("data_hex"))
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_args("storage.write format bytes requires dataHex"))?;
        return hex::decode(data_hex).map_err(|_| data_error("storage.write dataHex is invalid"));
    }
    let data = options
        .get("data")
        .ok_or_else(|| invalid_args("storage.write requires data"))?;
    let text = if format.eq_ignore_ascii_case("json") {
        serde_json::to_string(data).map_err(|_| data_error("storage.write JSON encode failed"))?
    } else if format.eq_ignore_ascii_case("text") {
        data.as_str()
            .ok_or_else(|| invalid_args("storage.write format text requires string data"))?
            .to_string()
    } else if format.eq_ignore_ascii_case("auto") {
        match data {
            Value::String(value) => value.clone(),
            other => serde_json::to_string(other)
                .map_err(|_| data_error("storage.write JSON encode failed"))?,
        }
    } else {
        return Err(invalid_args("storage.write format must be auto, text, json, or bytes"));
    };
    let encoding = option_string_optional(options, "encode")
        .or_else(|| option_string_optional(options, "encoding"));
    let encoding = parse_write_encoding(encoding.as_deref())?;
    encode_text(&text, encoding)
}

fn project_relative(raw_path: &str, allow_root: bool) -> Result<(PathBuf, PathBuf), ProjectStorageError> {
    if raw_path.len() > MAX_PROJECT_PATH_BYTES || raw_path.contains('\0') {
        return Err(invalid_args("storage project path is too long or contains NUL"));
    }
    let Some(relative) = raw_path.strip_prefix("$$/") else {
        return Err(invalid_args("project Storage paths must begin with `$$/`"));
    };
    if relative.is_empty() {
        if allow_root {
            return Ok((PathBuf::new(), PathBuf::new()));
        }
        return Err(invalid_args("storage file path cannot be project root"));
    }
    if relative.contains('\\') || relative.contains(':') {
        return Err(path_escape());
    }
    let mut clean = PathBuf::new();
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(path_escape());
        }
        if clean.as_os_str().is_empty() && segment == INTERNAL_DIR {
            return Err(error(
                "CAPABILITY_STORAGE_INTERNAL_RESERVED",
                "$$/.rbe is reserved for RBE project metadata",
            ));
        }
        clean.push(segment);
    }
    // The caller replaces this placeholder with its canonical project root.
    Ok((clean.clone(), clean))
}

fn validate_owner(owner: &str) -> Result<(), ProjectStorageError> {
    if owner.is_empty() || owner.len() > MAX_OWNER_BYTES {
        return Err(error("CAPABILITY_STORAGE_TARGET_INVALID", "storage owner length is invalid"));
    }
    if !owner
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(error("CAPABILITY_STORAGE_TARGET_INVALID", "storage owner contains unsupported characters"));
    }
    Ok(())
}

fn reject_link_or_reparse(metadata: &fs::Metadata) -> Result<(), ProjectStorageError> {
    if metadata.file_type().is_symlink() {
        return Err(path_escape());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(path_escape());
        }
    }
    Ok(())
}

fn parse_write_encoding(requested: Option<&str>) -> Result<TextEncoding, ProjectStorageError> {
    let label = requested.unwrap_or("utf-8");
    if label.eq_ignore_ascii_case("auto") {
        return Err(encoding_error("storage.write encoding cannot be auto"));
    }
    parse_encoding(label)
}

fn parse_encoding(label: &str) -> Result<TextEncoding, ProjectStorageError> {
    let normalized = label.trim().to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "utf8" | "utf-8" => Ok(TextEncoding::Utf8),
        "utf16" | "utf-16" | "utf16le" | "utf-16le" => Ok(TextEncoding::Utf16Le),
        "utf16be" | "utf-16be" => Ok(TextEncoding::Utf16Be),
        "ascii" | "us-ascii" => Ok(TextEncoding::Ascii),
        "latin1" | "latin-1" | "iso-8859-1" => Ok(TextEncoding::Latin1),
        _ => Err(encoding_error("unsupported storage text encoding")),
    }
}

fn decode_text(bytes: &[u8], requested: Option<&str>) -> Result<(String, TextEncoding), ProjectStorageError> {
    let encoding = match requested {
        None => detect_encoding(bytes),
        Some(value) if value.eq_ignore_ascii_case("auto") => detect_encoding(bytes),
        Some(value) => parse_encoding(value)?,
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

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<String, ProjectStorageError> {
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
        .map(|chunk| if little_endian { u16::from_le_bytes(*chunk) } else { u16::from_be_bytes(*chunk) })
        .collect::<Vec<_>>();
    String::from_utf16(&units).map_err(|_| data_error("file contains invalid UTF-16"))
}

fn encode_text(text: &str, encoding: TextEncoding) -> Result<Vec<u8>, ProjectStorageError> {
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
                    return Err(data_error("text contains characters not representable in Latin-1"));
                }
                out.push(value as u8);
            }
            Ok(out)
        }
    }
}

fn encode_response(value: &Value, max_response_bytes: u64) -> Result<Vec<u8>, ProjectStorageError> {
    let payload = serde_json::to_vec(value).map_err(|_| storage_failed())?;
    let limit = max_response_bytes.min(MAX_CAPABILITY_PAYLOAD_BYTES as u64) as usize;
    if payload.len() > limit {
        return Err(error("CAPABILITY_RESPONSE_TOO_LARGE", "storage response exceeded the capability grant"));
    }
    Ok(payload)
}

fn invalid_args(message: impl Into<String>) -> ProjectStorageError {
    error("CAPABILITY_STORAGE_ARGS_INVALID", message)
}

fn path_escape() -> ProjectStorageError {
    error("CAPABILITY_STORAGE_PATH_ESCAPE", "storage path escaped or aliased outside the RBE project root")
}

fn encoding_error(message: impl Into<String>) -> ProjectStorageError {
    error("CAPABILITY_STORAGE_ENCODING_INVALID", message)
}

fn data_error(message: impl Into<String>) -> ProjectStorageError {
    error("CAPABILITY_STORAGE_DATA_INVALID", message)
}

fn storage_failed() -> ProjectStorageError {
    error("CAPABILITY_STORAGE_CALL_FAILED", "project Storage operation failed")
}

fn error(code: &'static str, message: impl Into<String>) -> ProjectStorageError {
    ProjectStorageError { code, message: message.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rbe-project-storage-{name}-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn dispatch(storage: &Arc<ProjectStorageManager>, target: &str, operation: &str, args: Value) -> Value {
        let payload = serde_json::to_vec(&args).unwrap();
        let response = dispatch_project_storage_capability(
            storage,
            target,
            operation,
            &payload,
            MAX_CAPABILITY_PAYLOAD_BYTES as u64,
        )
        .unwrap();
        serde_json::from_slice(&response).unwrap()
    }

    #[test]
    fn writes_real_project_file_and_level_record() {
        let root = temp_root("write");
        let storage = ProjectStorageManager::open(root.clone()).unwrap();
        let target = project_storage_target("accounts").unwrap();
        let value = dispatch(
            &storage,
            &target,
            "write",
            json!([{
                "path": "$$/data/users/kate.json",
                "data": {"name":"Kate"},
                "format": "json",
                "encoding": "utf-8",
                "level": 1
            }]),
        );
        assert_eq!(value["level"], 1);
        let text = fs::read_to_string(root.join("data/users/kate.json")).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["name"], "Kate");
        let policy_dir = root.join(".rbe/data-levels/exact");
        assert_eq!(fs::read_dir(policy_dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn utf16_json_round_trip() {
        let root = temp_root("utf16");
        let storage = ProjectStorageManager::open(root.clone()).unwrap();
        let target = project_storage_target("settings").unwrap();
        dispatch(
            &storage,
            &target,
            "write",
            json!(["$$/data/value.json", {"ok":true}, "utf-16le", 2]),
        );
        let read = dispatch(
            &storage,
            &target,
            "read",
            json!([{"path":"$$/data/value.json", "encoding":"auto", "format":"json"}]),
        );
        assert_eq!(read["value"]["ok"], true);
        assert_eq!(read["encoding"], "utf-16le");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn traversal_and_internal_metadata_are_rejected() {
        let root = temp_root("escape");
        let storage = ProjectStorageManager::open(root.clone()).unwrap();
        let target = project_storage_target("test").unwrap();
        let payload = serde_json::to_vec(&json!(["$$/../outside", "x"])).unwrap();
        let error = dispatch_project_storage_capability(
            &storage,
            &target,
            "write",
            &payload,
            MAX_CAPABILITY_PAYLOAD_BYTES as u64,
        )
        .unwrap_err();
        assert_eq!(error.code, "CAPABILITY_STORAGE_PATH_ESCAPE");
        let payload = serde_json::to_vec(&json!(["$$/.rbe/owned", "x"])).unwrap();
        let error = dispatch_project_storage_capability(
            &storage,
            &target,
            "write",
            &payload,
            MAX_CAPABILITY_PAYLOAD_BYTES as u64,
        )
        .unwrap_err();
        assert_eq!(error.code, "CAPABILITY_STORAGE_INTERNAL_RESERVED");
        let _ = fs::remove_dir_all(root);
    }
}
'''

project_storage_path = ROOT / 'container-runtime/crates/container-runtime-core/src/project_storage.rs'
project_storage_path.write_text(PROJECT_STORAGE, encoding='utf-8')

replace_once(
    'container-runtime/crates/container-runtime-core/src/lib.rs',
    'mod execution;\nmod runtime;\nmod storage;\n',
    'mod execution;\nmod project_storage;\nmod runtime;\nmod storage;\n',
)
replace_once(
    'container-runtime/crates/container-runtime-core/src/lib.rs',
    'pub use execution::{\n    ExecutionId, ExecutionOutcome, ExecutionProvenance, ExecutionRecord, ExecutionState,\n    ExecutionTask, WorkCost,\n};\npub use runtime::{Runtime, RuntimeConfig, DEFAULT_ENVIRONMENT_STORAGE_BYTES};\n',
    'pub use execution::{\n    ExecutionId, ExecutionOutcome, ExecutionProvenance, ExecutionRecord, ExecutionState,\n    ExecutionTask, WorkCost,\n};\npub use project_storage::{\n    dispatch_project_storage_capability, project_storage_operation_allowed, project_storage_target,\n    DataLevel, ProjectStorageError, ProjectStorageManager, PROJECT_STORAGE_CAPABILITY_TARGET_PREFIX,\n    PROJECT_STORAGE_OPERATIONS,\n};\npub use runtime::{Runtime, RuntimeConfig, DEFAULT_ENVIRONMENT_STORAGE_BYTES};\n',
)

replace_once(
    'engine/crates/backend/src/container_process.rs',
    '    pub async fn spawn(\n        binary: &Path,\n        settings: &config::ContainersConfig,\n        host_capability: &crate::host_capability::HostCapabilityEndpoint,\n    ) -> anyhow::Result<Self> {',
    '    pub async fn spawn(\n        binary: &Path,\n        settings: &config::ContainersConfig,\n        host_capability: &crate::host_capability::HostCapabilityEndpoint,\n        project_root: &Path,\n    ) -> anyhow::Result<Self> {',
)
replace_once(
    'engine/crates/backend/src/container_process.rs',
    '                .env("RBE_HOST_CAPABILITY_TOKEN", host_capability.token())\n                .stdin(std::process::Stdio::piped())',
    '                .env("RBE_HOST_CAPABILITY_TOKEN", host_capability.token())\n                .env("RBE_PROJECT_ROOT", project_root)\n                .stdin(std::process::Stdio::piped())',
)

replace_once(
    'engine/crates/backend/src/main.rs',
    '    boot_trace(format!(\n        "cwd={}",\n        std::env::current_dir()\n            .map(|path| path.display().to_string())\n            .unwrap_or_else(|error| format!("<unavailable: {error}>"))\n    ));\n\n    let settings_path = resolve_settings_path();',
    '    let project_root = std::env::current_dir()\n        .map_err(|error| anyhow::anyhow!("resolve backend startup project root: {error}"))?\n        .canonicalize()\n        .map_err(|error| anyhow::anyhow!("canonicalize backend startup project root: {error}"))?;\n    boot_trace(format!("cwd={}", project_root.display()));\n    boot_trace(format!("project root=$$ => {}", project_root.display()));\n\n    let settings_path = resolve_settings_path();',
)
replace_once(
    'engine/crates/backend/src/main.rs',
    '    let initial_container = container_process::ContainerProcess::spawn(\n        &container_path,\n        &config.containers,\n        &host_capability_endpoint,\n    )',
    '    let initial_container = container_process::ContainerProcess::spawn(\n        &container_path,\n        &config.containers,\n        &host_capability_endpoint,\n        &project_root,\n    )',
)
replace_once(
    'engine/crates/backend/src/main.rs',
    '        ContainerSupervisorContext {\n            host_capability: host_capability_endpoint.clone(),\n            process: container_process.clone(),',
    '        ContainerSupervisorContext {\n            host_capability: host_capability_endpoint.clone(),\n            project_root: project_root.clone(),\n            process: container_process.clone(),',
)
replace_once(
    'engine/crates/backend/src/main.rs',
    'struct ContainerSupervisorContext {\n    host_capability: host_capability::HostCapabilityEndpoint,\n    process:',
    'struct ContainerSupervisorContext {\n    host_capability: host_capability::HostCapabilityEndpoint,\n    project_root: PathBuf,\n    process:',
)
replace_once(
    'engine/crates/backend/src/main.rs',
    '    let ContainerSupervisorContext {\n        host_capability,\n        process,',
    '    let ContainerSupervisorContext {\n        host_capability,\n        project_root,\n        process,',
)
# Both crash-restart and rolling-refresh spawn calls share this exact shape.
main_path = ROOT / 'engine/crates/backend/src/main.rs'
main_text = main_path.read_text(encoding='utf-8')
old_spawn = '''                        match container_process::ContainerProcess::spawn(\n                            &binary,\n                            &settings,\n                            &host_capability,\n                        )'''
if main_text.count(old_spawn) != 2:
    raise SystemExit(f'main.rs: expected 2 supervisor spawn anchors, found {main_text.count(old_spawn)}')
main_text = main_text.replace(
    old_spawn,
    '''                        match container_process::ContainerProcess::spawn(\n                            &binary,\n                            &settings,\n                            &host_capability,\n                            &project_root,\n                        )''',
)
main_path.write_text(main_text, encoding='utf-8')

replace_once(
    'container-runtime/crates/container-bin/src/main.rs',
    'use container_runtime_core::{\n    artifact_sha256_matches, CapabilityBroker, EnvironmentId, EnvironmentProfile,\n    EnvironmentRegistry, ExecutionProvenance, Runtime, RuntimeConfig, WorkCost,\n};',
    'use container_runtime_core::{\n    artifact_sha256_matches, CapabilityBroker, EnvironmentId, EnvironmentProfile,\n    EnvironmentRegistry, ExecutionProvenance, ProjectStorageManager, Runtime, RuntimeConfig, WorkCost,\n};',
)
replace_once(
    'container-runtime/crates/container-bin/src/main.rs',
    '    let environment_processes = environment_process::EnvironmentProcessSupervisor::start(\n        general_environments,\n        debug,\n        token.as_deref(),\n        Arc::clone(&capability_broker),\n        capability_dispatcher,\n    )?;',
    '    let project_root = env::var_os("RBE_PROJECT_ROOT")\n        .ok_or_else(|| anyhow::anyhow!("RBE_PROJECT_ROOT is required for Controller project Storage"))?;\n    let project_storage = ProjectStorageManager::open(PathBuf::from(project_root))?;\n    let environment_processes = environment_process::EnvironmentProcessSupervisor::start(\n        general_environments,\n        debug,\n        token.as_deref(),\n        Arc::clone(&capability_broker),\n        capability_dispatcher,\n        project_storage,\n    )?;',
)
replace_once(
    'container-runtime/crates/container-bin/src/main.rs',
    '                .env_remove("RBE_HOST_CAPABILITY_TOKEN")\n                .arg("--pid")',
    '                .env_remove("RBE_HOST_CAPABILITY_TOKEN")\n                .env_remove("RBE_PROJECT_ROOT")\n                .arg("--pid")',
)

replace_once(
    'container-runtime/crates/container-bin/src/environment_process.rs',
    'use container_runtime_core::{\n    dispatch_storage_capability, Canceller, CapabilityBroker, CapabilityCall, EnvironmentId,\n    EnvironmentProfile, EnvironmentStorageManager, ExecutionProvenance, ExecutionTask, Runner,\n    DEFAULT_ENVIRONMENT_STORAGE_BYTES,\n};',
    'use container_runtime_core::{\n    dispatch_project_storage_capability, dispatch_storage_capability, Canceller, CapabilityBroker,\n    CapabilityCall, EnvironmentId, EnvironmentProfile, EnvironmentStorageManager,\n    ExecutionProvenance, ExecutionTask, ProjectStorageManager, Runner,\n    DEFAULT_ENVIRONMENT_STORAGE_BYTES, PROJECT_STORAGE_CAPABILITY_TARGET_PREFIX,\n};',
)
replace_once(
    'container-runtime/crates/container-bin/src/environment_process.rs',
    '    capability_broker: Arc<CapabilityBroker>,\n    capability_dispatcher: CapabilityDispatcher,\n    artifact_crash_circuit:',
    '    capability_broker: Arc<CapabilityBroker>,\n    capability_dispatcher: CapabilityDispatcher,\n    project_storage: Arc<ProjectStorageManager>,\n    artifact_crash_circuit:',
)
replace_once(
    'container-runtime/crates/container-bin/src/environment_process.rs',
    '        capability_broker: Arc<CapabilityBroker>,\n        capability_dispatcher: CapabilityDispatcher,\n    ) -> Result<Arc<Self>> {',
    '        capability_broker: Arc<CapabilityBroker>,\n        capability_dispatcher: CapabilityDispatcher,\n        project_storage: Arc<ProjectStorageManager>,\n    ) -> Result<Arc<Self>> {',
)
replace_once(
    'container-runtime/crates/container-bin/src/environment_process.rs',
    '            capability_broker,\n            capability_dispatcher,\n            artifact_crash_circuit:',
    '            capability_broker,\n            capability_dispatcher,\n            project_storage,\n            artifact_crash_circuit:',
)
replace_once(
    'container-runtime/crates/container-bin/src/environment_process.rs',
    '        if environment_owned_capability(call.kind) {\n            return self.dispatch_environment_storage(',
    '        if call.kind == CapabilityKind::Storage\n            && call.target.starts_with(PROJECT_STORAGE_CAPABILITY_TARGET_PREFIX)\n        {\n            return match dispatch_project_storage_capability(\n                &self.project_storage,\n                &call.target,\n                &call.operation,\n                &call.payload,\n                authorized.max_response_bytes,\n            ) {\n                Ok(payload) => WorkerCapabilityResult::Success {\n                    call_id: call.call_id,\n                    payload,\n                },\n                Err(error) => capability_error(call.call_id, error.code, error.message),\n            };\n        }\n        if environment_owned_capability(call.kind) {\n            return self.dispatch_environment_storage(',
)
replace_once(
    'container-runtime/crates/container-bin/src/environment_process.rs',
    '            .env_remove("RBE_HOST_CAPABILITY_TOKEN");',
    '            .env_remove("RBE_HOST_CAPABILITY_TOKEN")\n            .env_remove("RBE_PROJECT_ROOT");',
)

replace_once(
    'engine/crates/route-engine/src/runtime_image.rs',
    'pub(crate) const STORAGE_CAPABILITY_TARGET_PREFIX: &str = "storage:";\npub(crate) const STORAGE_RAW_CAPABILITY_OPERATIONS: [&str; 4] =',
    'pub(crate) const STORAGE_CAPABILITY_TARGET_PREFIX: &str = "storage:";\npub(crate) const PROJECT_STORAGE_CAPABILITY_TARGET_PREFIX: &str = "project-storage:";\npub(crate) const PROJECT_STORAGE_OPERATIONS: [&str; 5] = ["read", "write", "exists", "remove", "list"];\npub(crate) const STORAGE_RAW_CAPABILITY_OPERATIONS: [&str; 4] =',
)
replace_once(
    'engine/crates/route-engine/src/runtime_image.rs',
    'pub(crate) fn storage_capability_operation_allowed(operation: &str) -> bool {\n    STORAGE_CAPABILITY_OPERATIONS.contains(&operation)\n}\n\npub(crate) fn storage_raw_operation_allowed',
    'pub(crate) fn storage_capability_operation_allowed(operation: &str) -> bool {\n    STORAGE_CAPABILITY_OPERATIONS.contains(&operation)\n}\n\npub(crate) fn project_storage_operation_allowed(operation: &str) -> bool {\n    PROJECT_STORAGE_OPERATIONS.contains(&operation)\n}\n\npub(crate) fn storage_raw_operation_allowed',
)
replace_once(
    'engine/crates/route-engine/src/runtime_image.rs',
    'pub(crate) fn storage_capability_target(owner: &str) -> Option<String> {\n    if !storage_capability_owner_allowed(owner) {\n        return None;\n    }\n    let target = format!("{STORAGE_CAPABILITY_TARGET_PREFIX}{owner}");\n    (target.len() <= CONTAINER_MAX_CAPABILITY_TARGET_BYTES).then_some(target)\n}\n',
    'pub(crate) fn storage_capability_target(owner: &str) -> Option<String> {\n    if !storage_capability_owner_allowed(owner) {\n        return None;\n    }\n    let target = format!("{STORAGE_CAPABILITY_TARGET_PREFIX}{owner}");\n    (target.len() <= CONTAINER_MAX_CAPABILITY_TARGET_BYTES).then_some(target)\n}\n\npub(crate) fn project_storage_target(owner: &str) -> Option<String> {\n    if !storage_capability_owner_allowed(owner) {\n        return None;\n    }\n    let target = format!("{PROJECT_STORAGE_CAPABILITY_TARGET_PREFIX}{owner}");\n    (target.len() <= CONTAINER_MAX_CAPABILITY_TARGET_BYTES).then_some(target)\n}\n',
)
replace_once(
    'engine/crates/route-engine/src/runtime_image.rs',
    '    PublicHttp { operation: String },\n    Storage { owner: String, operation: String },\n    Video',
    '    PublicHttp { operation: String },\n    ProjectStorage { owner: String, operation: String },\n    Storage { owner: String, operation: String },\n    Video',
)
replace_once(
    'engine/crates/route-engine/src/runtime_image.rs',
    '    let mut public_http_operations = BTreeSet::new();\n    let mut storage_operations = BTreeMap::<String, BTreeSet<String>>::new();',
    '    let mut public_http_operations = BTreeSet::new();\n    let mut project_storage_operations = BTreeMap::<String, BTreeSet<String>>::new();\n    let mut storage_operations = BTreeMap::<String, BTreeSet<String>>::new();',
)
replace_once(
    'engine/crates/route-engine/src/runtime_image.rs',
    '            RuntimeCapabilityRequirement::Storage { owner, operation } => {',
    '            RuntimeCapabilityRequirement::ProjectStorage { owner, operation } => {\n                if !storage_capability_owner_allowed(owner) {\n                    return Err(RuntimeCapabilityLoweringError {\n                        message: format!("project Storage principal {owner:?} is invalid"),\n                    });\n                }\n                if !project_storage_operation_allowed(operation) {\n                    return Err(RuntimeCapabilityLoweringError {\n                        message: format!("project Storage operation {operation:?} is not supported"),\n                    });\n                }\n                project_storage_operations\n                    .entry(owner.clone())\n                    .or_default()\n                    .insert(operation.clone());\n            }\n            RuntimeCapabilityRequirement::Storage { owner, operation } => {',
)
replace_once(
    'engine/crates/route-engine/src/runtime_image.rs',
    '    for (owner, operations) in storage_operations {\n        let target =',
    '    for (owner, operations) in project_storage_operations {\n        let target = project_storage_target(&owner).ok_or_else(|| RuntimeCapabilityLoweringError {\n            message: format!("project Storage principal {owner:?} cannot be lowered to an exact target"),\n        })?;\n        grants.push(ContainerCapabilityGrant {\n            kind: ContainerCapabilityKind::Storage,\n            target,\n            operations: operations.into_iter().collect(),\n            max_request_bytes: CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64,\n            max_response_bytes: CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64,\n        });\n    }\n    for (owner, operations) in storage_operations {\n        let target =',
)

replace_once(
    'engine/crates/route-engine/src/relc.rs',
    '    stable_image_hash, stable_source_hash, storage_capability_owner_allowed,\n    storage_library_operation_allowed, storage_raw_operation_allowed, RuntimeCapabilityRequirement,\n    RuntimeExecutable, RuntimeImage, RuntimeSourceManifest, STORAGE_LIBRARY_OPERATIONS,\n    STORAGE_RAW_CAPABILITY_OPERATIONS,\n};',
    '    project_storage_operation_allowed, stable_image_hash, stable_source_hash,\n    storage_capability_owner_allowed, storage_library_operation_allowed, storage_raw_operation_allowed,\n    RuntimeCapabilityRequirement, RuntimeExecutable, RuntimeImage, RuntimeSourceManifest,\n    PROJECT_STORAGE_OPERATIONS, STORAGE_LIBRARY_OPERATIONS, STORAGE_RAW_CAPABILITY_OPERATIONS,\n};',
)
replace_once(
    'engine/crates/route-engine/src/relc.rs',
    '        "http" => HTTP_HOST_OPERATIONS,\n        "storage" => &STORAGE_RAW_CAPABILITY_OPERATIONS,\n        "Storage" => &STORAGE_LIBRARY_OPERATIONS,',
    '        "http" => HTTP_HOST_OPERATIONS,\n        "storage" => &PROJECT_STORAGE_OPERATIONS,\n        "Storage" => &STORAGE_LIBRARY_OPERATIONS,\n        "envStorage" => &STORAGE_RAW_CAPABILITY_OPERATIONS,',
)
replace_once(
    'engine/crates/route-engine/src/relc.rs',
    '    let module_owner = if matches!(module, "storage" | "Storage" | "vm" | "video-manager") {',
    '    let module_owner = if matches!(module, "storage" | "Storage" | "envStorage" | "vm" | "video-manager") {',
)
replace_once(
    'engine/crates/route-engine/src/relc.rs',
    '        if matches!(module, "storage" | "Storage") && !storage_capability_owner_allowed(owner) {',
    '        if matches!(module, "storage" | "Storage" | "envStorage") && !storage_capability_owner_allowed(owner) {',
)
replace_once(
    'engine/crates/route-engine/src/relc.rs',
    '            "storage" | "Storage" => RuntimeCapabilityRequirement::Storage {\n                owner: module_owner\n                    .expect("Storage owner validated above")\n                    .to_string(),\n                operation: operation.to_string(),\n            },',
    '            "storage" => RuntimeCapabilityRequirement::ProjectStorage {\n                owner: module_owner\n                    .expect("project Storage owner validated above")\n                    .to_string(),\n                operation: operation.to_string(),\n            },\n            "Storage" | "envStorage" => RuntimeCapabilityRequirement::Storage {\n                owner: module_owner\n                    .expect("Environment Storage owner validated above")\n                    .to_string(),\n                operation: operation.to_string(),\n            },',
)
replace_once(
    'engine/crates/route-engine/src/relc.rs',
    '.any(|requirement| matches!(requirement, RuntimeCapabilityRequirement::Storage { .. }))',
    '.any(|requirement| matches!(requirement, RuntimeCapabilityRequirement::Storage { .. } | RuntimeCapabilityRequirement::ProjectStorage { .. }))',
)
replace_once(
    'engine/crates/route-engine/src/relc.rs',
    'message: "Environment Storage authority may propagate through Module REL only into a Route that executes inside Container"',
    'message: "Storage authority may propagate through Module REL only into a Route that executes inside Container"',
)

replace_once(
    'engine/crates/route-engine/src/modules.rs',
    '        "storage" => matches!(function, "read" | "list" | "snapshot" | "commit"),\n        "Storage" => matches!(',
    '        "storage" => matches!(function, "read" | "write" | "exists" | "remove" | "list"),\n        "envStorage" => matches!(function, "read" | "list" | "snapshot" | "commit"),\n        "Storage" => matches!(',
)
replace_once(
    'engine/crates/route-engine/src/modules.rs',
    '                        "storage" | "Storage" => ModuleKind::Builtin(BuiltinModule::Storage),',
    '                        "storage" | "Storage" | "envStorage" => ModuleKind::Builtin(BuiltinModule::Storage),',
)
replace_once(
    'engine/crates/route-engine/src/modules.rs',
    '                        "storage" | "Storage" => ModuleKind::Builtin(BuiltinModule::Storage),',
    '                        "storage" | "Storage" | "envStorage" => ModuleKind::Builtin(BuiltinModule::Storage),',
)

replace_once(
    'engine/crates/route-engine/src/wasm_compiler.rs',
    'use crate::runtime_image::{storage_capability_operation_allowed, storage_capability_target};',
    'use crate::runtime_image::{\n    project_storage_operation_allowed, project_storage_target, storage_capability_operation_allowed,\n    storage_capability_target, storage_library_operation_allowed, storage_raw_operation_allowed,\n};',
)
replace_once(
    'engine/crates/route-engine/src/wasm_compiler.rs',
    '        ImportTarget::BuiltinFunction { module, function }\n            if matches!(module.as_str(), "storage" | "Storage")\n                && storage_capability_operation_allowed(function) =>\n        {\n            (\n                ContainerCapabilityKind::Storage,\n                storage_capability_target(owner)?,\n                function.clone(),\n            )\n        }',
    '        ImportTarget::BuiltinFunction { module, function }\n            if module == "storage" && project_storage_operation_allowed(function) =>\n        {\n            (\n                ContainerCapabilityKind::Storage,\n                project_storage_target(owner)?,\n                function.clone(),\n            )\n        }\n        ImportTarget::BuiltinFunction { module, function }\n            if ((module == "Storage" && storage_library_operation_allowed(function))\n                || (module == "envStorage" && storage_raw_operation_allowed(function)))\n                && storage_capability_operation_allowed(function) =>\n        {\n            (\n                ContainerCapabilityKind::Storage,\n                storage_capability_target(owner)?,\n                function.clone(),\n            )\n        }',
)

print('project Storage patch applied')
