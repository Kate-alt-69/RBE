//! CTI-backed Task diagnostics and binary event rendering.
//!
//! Runtime execution emits fixed-width `TaskEvent` packets. Human names and
//! source locations are resolved only when diagnostics are consumed, using the
//! immutable LOG_EVENTS, SOURCE_FILES, SOURCE_MAP and ERROR_SITES sections that
//! RELC placed in the CTI. No source file scanning is performed at runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use ipc_protocol::{
    ContainerTaskImage, CtiEventClass, CtiLogLevel, CtiSectionKind, TaskEvent,
    TaskEventDescriptor, TaskEventDictionary, TaskEventError, CTI_LOG_ABI_VERSION,
};

use crate::container_task_loader::LoadedTaskImage;

const SECTION_PAYLOAD_VERSION: u16 = 1;
const MAX_LOGICAL_PATH_BYTES: usize = 4096;
const MAX_SECTION_STRING_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSourceLocation {
    pub site_id: u32,
    pub file_id: u32,
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub source_id: String,
    pub symbol: String,
}

#[derive(Debug, Clone)]
pub struct TaskEventRenderer {
    dictionary: TaskEventDictionary,
    sites: BTreeMap<u32, TaskSourceLocation>,
    error_sites: BTreeSet<u32>,
}

impl TaskEventRenderer {
    pub fn from_loaded_task(task: &LoadedTaskImage) -> Result<Self, TaskDiagnosticError> {
        let image = task.image();
        Self::from_image(&image)
    }

    pub fn from_image(image: &ContainerTaskImage) -> Result<Self, TaskDiagnosticError> {
        let dictionary = decode_log_events(required_section(image, CtiSectionKind::LogEvents)?)?;
        let files = decode_source_files(required_section(image, CtiSectionKind::SourceFiles)?)?;
        let sites = decode_source_map(required_section(image, CtiSectionKind::SourceMap)?, &files)?;
        let error_sites = decode_error_sites(
            required_section(image, CtiSectionKind::ErrorSites)?,
            &sites,
        )?;
        Ok(Self {
            dictionary,
            sites,
            error_sites,
        })
    }

    pub fn dictionary(&self) -> &TaskEventDictionary {
        &self.dictionary
    }

    pub fn source_location(&self, site_id: u32) -> Option<&TaskSourceLocation> {
        self.sites.get(&site_id)
    }

    pub fn is_error_site(&self, site_id: u32) -> bool {
        self.error_sites.contains(&site_id)
    }

    pub fn should_emit(&self, mode: CtiLogLevel, event_id: u16) -> bool {
        self.dictionary.should_emit(mode, event_id)
    }

    /// Hot-path helper: filters numerically and returns the fixed-width packet.
    /// It deliberately performs no string formatting or source lookup.
    pub fn emit_binary(&self, mode: CtiLogLevel, event: TaskEvent) -> Option<[u8; 40]> {
        self.should_emit(mode, event.event_id)
            .then(|| event.encode())
    }

    /// Cold-path human rendering. A non-zero site must resolve through CTI
    /// metadata; malformed/forged site IDs fail closed instead of falling back
    /// to source scanning.
    pub fn render_event(&self, event: &TaskEvent) -> Result<String, TaskDiagnosticError> {
        let mut rendered = self.dictionary.render(event)?;
        if event.site_id != 0 {
            let location = self.sites.get(&event.site_id).ok_or_else(|| {
                TaskDiagnosticError::InvalidMetadata(format!(
                    "event {} references unknown CTI site {}",
                    event.event_id, event.site_id
                ))
            })?;
            rendered.push_str(&format!(
                " [{}:{}:{}]",
                location.path, location.line, location.column
            ));
        }
        Ok(rendered)
    }

    pub fn render_error(
        &self,
        code: &str,
        event: &TaskEvent,
        message: &str,
    ) -> Result<String, TaskDiagnosticError> {
        if event.site_id == 0 || !self.error_sites.contains(&event.site_id) {
            return Err(TaskDiagnosticError::InvalidMetadata(format!(
                "error event {} does not reference a declared CTI ERROR_SITES entry",
                event.event_id
            )));
        }
        let rendered = self.render_event(event)?;
        Ok(format!("{code}: {rendered}: {message}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskDiagnosticError {
    MissingSection(CtiSectionKind),
    InvalidMetadata(String),
    Event(TaskEventError),
}

impl fmt::Display for TaskDiagnosticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSection(kind) => write!(
                formatter,
                "CTI diagnostic section {} is missing",
                kind.code()
            ),
            Self::InvalidMetadata(message) => {
                write!(formatter, "invalid CTI diagnostic metadata: {message}")
            }
            Self::Event(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TaskDiagnosticError {}

impl From<TaskEventError> for TaskDiagnosticError {
    fn from(value: TaskEventError) -> Self {
        Self::Event(value)
    }
}

fn required_section(
    image: &ContainerTaskImage,
    kind: CtiSectionKind,
) -> Result<&[u8], TaskDiagnosticError> {
    image
        .sections
        .iter()
        .find(|section| section.kind == kind.code())
        .map(|section| section.data.as_slice())
        .ok_or(TaskDiagnosticError::MissingSection(kind))
}

fn decode_log_events(bytes: &[u8]) -> Result<TaskEventDictionary, TaskDiagnosticError> {
    let mut reader = SectionReader::new(bytes);
    let abi = reader.u16()?;
    if abi != CTI_LOG_ABI_VERSION {
        return Err(invalid(format!(
            "LOG_EVENTS ABI {abi} does not match runtime ABI {CTI_LOG_ABI_VERSION}"
        )));
    }
    let count = usize::from(reader.u16()?);
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let event_id = reader.u16()?;
        let class = event_class(reader.u8()?)?;
        let level = log_level(reader.u8()?)?;
        let symbol = reader.string()?;
        let template = reader.string()?;
        entries.push(TaskEventDescriptor {
            event_id,
            class,
            level,
            symbol,
            template,
        });
    }
    reader.finish()?;
    TaskEventDictionary::new(entries).map_err(Into::into)
}

fn decode_source_files(bytes: &[u8]) -> Result<BTreeMap<u32, String>, TaskDiagnosticError> {
    let mut reader = SectionReader::new(bytes);
    expect_payload_header(&mut reader, "SOURCE_FILES")?;
    let count = reader.u32()? as usize;
    let mut files = BTreeMap::new();
    for _ in 0..count {
        let file_id = reader.u32()?;
        let path = normalize_logical_path(&reader.string()?)?;
        if files.insert(file_id, path).is_some() {
            return Err(invalid(format!("duplicate source file ID {file_id}")));
        }
    }
    reader.finish()?;
    Ok(files)
}

fn decode_source_map(
    bytes: &[u8],
    files: &BTreeMap<u32, String>,
) -> Result<BTreeMap<u32, TaskSourceLocation>, TaskDiagnosticError> {
    let mut reader = SectionReader::new(bytes);
    expect_payload_header(&mut reader, "SOURCE_MAP")?;
    let count = reader.u32()? as usize;
    let mut sites = BTreeMap::new();
    for _ in 0..count {
        let site_id = reader.u32()?;
        let file_id = reader.u32()?;
        let line = reader.u32()?;
        let column = reader.u32()?;
        let source_id = reader.string()?;
        let symbol = reader.string()?;
        if site_id == 0 || line == 0 || column == 0 {
            return Err(invalid("SOURCE_MAP site, line and column IDs must be non-zero"));
        }
        if source_id.is_empty() || symbol.is_empty() {
            return Err(invalid("SOURCE_MAP source and symbol must be non-empty"));
        }
        let path = files
            .get(&file_id)
            .cloned()
            .ok_or_else(|| invalid(format!("site {site_id} references unknown file ID {file_id}")))?;
        let location = TaskSourceLocation {
            site_id,
            file_id,
            path,
            line,
            column,
            source_id,
            symbol,
        };
        if sites.insert(site_id, location).is_some() {
            return Err(invalid(format!("duplicate source site ID {site_id}")));
        }
    }
    reader.finish()?;
    Ok(sites)
}

fn decode_error_sites(
    bytes: &[u8],
    sites: &BTreeMap<u32, TaskSourceLocation>,
) -> Result<BTreeSet<u32>, TaskDiagnosticError> {
    let mut reader = SectionReader::new(bytes);
    expect_payload_header(&mut reader, "ERROR_SITES")?;
    let count = reader.u32()? as usize;
    let mut out = BTreeSet::new();
    for _ in 0..count {
        let site_id = reader.u32()?;
        if !sites.contains_key(&site_id) {
            return Err(invalid(format!(
                "ERROR_SITES references unknown SOURCE_MAP site {site_id}"
            )));
        }
        if !out.insert(site_id) {
            return Err(invalid(format!("duplicate ERROR_SITES site {site_id}")));
        }
    }
    reader.finish()?;
    Ok(out)
}

fn expect_payload_header(
    reader: &mut SectionReader<'_>,
    label: &str,
) -> Result<(), TaskDiagnosticError> {
    let version = reader.u16()?;
    let reserved = reader.u16()?;
    if version != SECTION_PAYLOAD_VERSION || reserved != 0 {
        return Err(invalid(format!(
            "{label} header version/reserved fields are unsupported"
        )));
    }
    Ok(())
}

fn event_class(value: u8) -> Result<CtiEventClass, TaskDiagnosticError> {
    match value {
        1 => Ok(CtiEventClass::Task),
        2 => Ok(CtiEventClass::Function),
        3 => Ok(CtiEventClass::Service),
        4 => Ok(CtiEventClass::QuickDb),
        5 => Ok(CtiEventClass::Capability),
        6 => Ok(CtiEventClass::Wasm),
        7 => Ok(CtiEventClass::Memory),
        8 => Ok(CtiEventClass::Network),
        9 => Ok(CtiEventClass::Error),
        10 => Ok(CtiEventClass::Warning),
        11 => Ok(CtiEventClass::Debug),
        _ => Err(invalid(format!("unknown CTI event class {value}"))),
    }
}

fn log_level(value: u8) -> Result<CtiLogLevel, TaskDiagnosticError> {
    match value {
        0 => Ok(CtiLogLevel::Off),
        1 => Ok(CtiLogLevel::Error),
        2 => Ok(CtiLogLevel::Task),
        3 => Ok(CtiLogLevel::Normal),
        4 => Ok(CtiLogLevel::Trace),
        _ => Err(invalid(format!("unknown CTI log level {value}"))),
    }
}

fn normalize_logical_path(path: &str) -> Result<String, TaskDiagnosticError> {
    if path.is_empty() || path.len() > MAX_LOGICAL_PATH_BYTES {
        return Err(invalid("source logical path is empty or too long"));
    }
    if path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || (path.len() >= 2 && path.as_bytes()[1] == b':')
    {
        return Err(invalid(format!("unsafe source logical path {path:?}")));
    }
    if path
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(invalid(format!("unsafe source logical path {path:?}")));
    }
    Ok(path.to_string())
}

fn invalid(message: impl Into<String>) -> TaskDiagnosticError {
    TaskDiagnosticError::InvalidMetadata(message.into())
}

struct SectionReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> SectionReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], TaskDiagnosticError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| invalid("section offset overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| invalid("section is truncated"))?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, TaskDiagnosticError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, TaskDiagnosticError> {
        let bytes: [u8; 2] = self.take(2)?.try_into().expect("fixed slice");
        Ok(u16::from_be_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, TaskDiagnosticError> {
        let bytes: [u8; 4] = self.take(4)?.try_into().expect("fixed slice");
        Ok(u32::from_be_bytes(bytes))
    }

    fn string(&mut self) -> Result<String, TaskDiagnosticError> {
        let length = self.u32()? as usize;
        if length > MAX_SECTION_STRING_BYTES {
            return Err(invalid("CTI diagnostic string exceeds runtime limit"));
        }
        let bytes = self.take(length)?;
        let value = std::str::from_utf8(bytes)
            .map_err(|_| invalid("CTI diagnostic string is not UTF-8"))?;
        Ok(value.to_string())
    }

    fn finish(self) -> Result<(), TaskDiagnosticError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid("CTI diagnostic section contains trailing bytes"))
        }
    }
}

#[cfg(test)]
mod tests {
    use ipc_protocol::{CtiHeaderV1, CtiSection};

    use super::*;

    fn push_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_be_bytes());
    }

    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }

    fn push_string(out: &mut Vec<u8>, value: &str) {
        push_u32(out, value.len() as u32);
        out.extend_from_slice(value.as_bytes());
    }

    fn image(path: &str) -> ContainerTaskImage {
        let mut logs = Vec::new();
        push_u16(&mut logs, CTI_LOG_ABI_VERSION);
        push_u16(&mut logs, 2);
        push_u16(&mut logs, 1);
        logs.push(CtiEventClass::Error as u8);
        logs.push(CtiLogLevel::Error as u8);
        push_string(&mut logs, "verifyPassword");
        push_string(&mut logs, "ER : Function {symbol} failed at site {site}");
        push_u16(&mut logs, 2);
        logs.push(CtiEventClass::Function as u8);
        logs.push(CtiLogLevel::Trace as u8);
        push_string(&mut logs, "verifyPassword");
        push_string(&mut logs, "FNCT : executed {symbol}");

        let mut files = Vec::new();
        push_u16(&mut files, SECTION_PAYLOAD_VERSION);
        push_u16(&mut files, 0);
        push_u32(&mut files, 1);
        push_u32(&mut files, 7);
        push_string(&mut files, path);

        let mut source_map = Vec::new();
        push_u16(&mut source_map, SECTION_PAYLOAD_VERSION);
        push_u16(&mut source_map, 0);
        push_u32(&mut source_map, 1);
        push_u32(&mut source_map, 28);
        push_u32(&mut source_map, 7);
        push_u32(&mut source_map, 84);
        push_u32(&mut source_map, 17);
        push_string(&mut source_map, "module:auth/password");
        push_string(&mut source_map, "verifyPassword");

        let mut error_sites = Vec::new();
        push_u16(&mut error_sites, SECTION_PAYLOAD_VERSION);
        push_u16(&mut error_sites, 0);
        push_u32(&mut error_sites, 1);
        push_u32(&mut error_sites, 28);

        ContainerTaskImage {
            header: CtiHeaderV1::new(31_844, 1, [1; 32], [2; 32]),
            sections: vec![
                CtiSection::required(CtiSectionKind::LogEvents, logs),
                CtiSection::required(CtiSectionKind::SourceFiles, files),
                CtiSection::required(CtiSectionKind::SourceMap, source_map),
                CtiSection::required(CtiSectionKind::ErrorSites, error_sites),
            ],
        }
    }

    #[test]
    fn error_resolves_symbol_and_original_source_location_without_scanning() {
        let renderer = TaskEventRenderer::from_image(&image("auth/password.module")).unwrap();
        let event = TaskEvent {
            timestamp_delta_ns: 5,
            execution_id: 9,
            event_id: 1,
            oid: 31_844,
            site_id: 28,
            arg0: 0,
            arg1: 0,
        };
        let rendered = renderer
            .render_error("REL4021", &event, "verification failed")
            .unwrap();
        assert!(rendered.contains("REL4021"));
        assert!(rendered.contains("verifyPassword"));
        assert!(rendered.contains("auth/password.module:84:17"));
    }

    #[test]
    fn hot_path_filters_and_emits_binary_without_rendering() {
        let renderer = TaskEventRenderer::from_image(&image("auth/password.module")).unwrap();
        let trace = TaskEvent {
            timestamp_delta_ns: 1,
            execution_id: 2,
            event_id: 2,
            oid: 31_844,
            site_id: 28,
            arg0: 0,
            arg1: 0,
        };
        assert!(renderer.emit_binary(CtiLogLevel::Error, trace).is_none());
        let packet = renderer.emit_binary(CtiLogLevel::Trace, trace).unwrap();
        assert_eq!(TaskEvent::decode(packet), trace);
    }

    #[test]
    fn unsafe_source_path_fails_closed() {
        let error = TaskEventRenderer::from_image(&image("../secret.module")).unwrap_err();
        assert!(error.to_string().contains("unsafe source logical path"));
    }
}
