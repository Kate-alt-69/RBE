//! Source-accurate Phase 4 discovery from one already-compiled Runtime Image.
//!
//! RELC already parsed every Route/Module/Service into `RuntimeExecutable`.
//! Re-parsing those sources for OID discovery would create a second semantic
//! path and could drift from the image that is actually being activated. This
//! bridge therefore combines the immutable parsed ASTs with the exact original
//! physical/embedded source bytes only to derive per-source SHA-256 identity.

use std::collections::BTreeMap;
use std::fmt;

use crate::embedded_rel::{extract_embedded_rel, EmbeddedRelError};
use crate::rel_symbol_discovery::{
    discover_linked_rel_symbols, LinkedRelDiscovery, LinkedRelDiscoveryError, LinkedRelSourceUnit,
};
use crate::relc::PhysicalRelSource;
use crate::runtime_image::{RuntimeExecutable, RuntimeImage};
use crate::source_registry::RelSourceKind;

type SourceKey = (RelSourceKind, String);

/// Build the linked-REL reachability snapshot for the exact Runtime Image that
/// RELC already produced.
///
/// `raw_server_source` is required because embedded Route/Module/Service blocks
/// are literal source files owned by `server.server`; their original bytes must
/// participate in OID identity just like physical files do.
pub fn discover_linked_rel_from_runtime_image(
    image: &RuntimeImage,
    raw_server_source: &str,
    physical_sources: &[PhysicalRelSource],
) -> Result<LinkedRelDiscovery, RuntimeImageLinkedRelError> {
    let sources = collect_original_sources(raw_server_source, physical_sources)?;
    let mut units = Vec::new();

    for manifest in &image.sources {
        let kind = manifest.kind;
        if !matches!(
            kind,
            RelSourceKind::Route | RelSourceKind::Module | RelSourceKind::Service
        ) {
            continue;
        }

        let key = (kind, manifest.logical_name.clone());
        let source = sources
            .get(&key)
            .ok_or_else(|| RuntimeImageLinkedRelError::MissingOriginalSource {
                kind,
                logical_name: manifest.logical_name.clone(),
            })?;
        let executable = image.executable(&manifest.id).ok_or_else(|| {
            RuntimeImageLinkedRelError::MissingExecutable {
                source: manifest.id.to_string(),
            }
        })?;

        let unit = match (kind, executable) {
            (RelSourceKind::Route, RuntimeExecutable::Route(file)) => {
                LinkedRelSourceUnit::route(&manifest.logical_name, source, file.as_ref())
            }
            (RelSourceKind::Module, RuntimeExecutable::Module(file)) => {
                LinkedRelSourceUnit::module(&manifest.logical_name, source, file.as_ref())
            }
            (RelSourceKind::Service, RuntimeExecutable::Service(file)) => {
                LinkedRelSourceUnit::service(&manifest.logical_name, source, file.as_ref())
            }
            _ => {
                return Err(RuntimeImageLinkedRelError::ExecutableKindMismatch {
                    source: manifest.id.to_string(),
                    expected: kind,
                })
            }
        };
        units.push(unit);
    }

    discover_linked_rel_symbols(&units).map_err(RuntimeImageLinkedRelError::Discovery)
}

fn collect_original_sources(
    raw_server_source: &str,
    physical_sources: &[PhysicalRelSource],
) -> Result<BTreeMap<SourceKey, String>, RuntimeImageLinkedRelError> {
    let mut sources = BTreeMap::new();
    for physical in physical_sources {
        insert_source(
            &mut sources,
            physical.kind,
            &physical.logical_name,
            &physical.source,
            "physical",
        )?;
    }

    let extracted = extract_embedded_rel(raw_server_source)
        .map_err(RuntimeImageLinkedRelError::EmbeddedSource)?;
    for embedded in extracted.embedded {
        insert_source(
            &mut sources,
            embedded.kind,
            &embedded.logical_name,
            &embedded.source,
            "embedded",
        )?;
    }
    Ok(sources)
}

fn insert_source(
    sources: &mut BTreeMap<SourceKey, String>,
    kind: RelSourceKind,
    logical_name: &str,
    source: &str,
    origin: &'static str,
) -> Result<(), RuntimeImageLinkedRelError> {
    let key = (kind, logical_name.trim().to_string());
    if sources.insert(key.clone(), source.to_string()).is_some() {
        return Err(RuntimeImageLinkedRelError::DuplicateOriginalSource {
            kind,
            logical_name: key.1,
            origin,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeImageLinkedRelError {
    EmbeddedSource(EmbeddedRelError),
    Discovery(LinkedRelDiscoveryError),
    DuplicateOriginalSource {
        kind: RelSourceKind,
        logical_name: String,
        origin: &'static str,
    },
    MissingOriginalSource {
        kind: RelSourceKind,
        logical_name: String,
    },
    MissingExecutable {
        source: String,
    },
    ExecutableKindMismatch {
        source: String,
        expected: RelSourceKind,
    },
}

impl fmt::Display for RuntimeImageLinkedRelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmbeddedSource(error) => write!(formatter, "could not recover embedded REL source bytes: {error}"),
            Self::Discovery(error) => write!(formatter, "linked REL discovery failed: {error}"),
            Self::DuplicateOriginalSource {
                kind,
                logical_name,
                origin,
            } => write!(
                formatter,
                "duplicate {kind} REL source {logical_name:?} while collecting {origin} Runtime Image source bytes"
            ),
            Self::MissingOriginalSource { kind, logical_name } => write!(
                formatter,
                "compiled Runtime Image contains {kind} REL source {logical_name:?}, but its original source bytes are unavailable"
            ),
            Self::MissingExecutable { source } => write!(
                formatter,
                "compiled Runtime Image source {source} has no immutable executable AST"
            ),
            Self::ExecutableKindMismatch { source, expected } => write!(
                formatter,
                "compiled Runtime Image source {source} does not carry the expected {expected} executable AST"
            ),
        }
    }
}

impl std::error::Error for RuntimeImageLinkedRelError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_physical_and_embedded_original_source_bytes() {
        let physical = vec![PhysicalRelSource::new(
            RelSourceKind::Module,
            "Physical",
            "module/Physical.module",
            "export function run() { return true; }",
        )];
        let server = r#"server Main {}
[file-start:service.Worker]
export function run() { return true; }
[file-end:service]
"#;
        let sources = collect_original_sources(server, &physical).unwrap();
        assert!(sources.contains_key(&(RelSourceKind::Module, "Physical".into())));
        assert!(sources.contains_key(&(RelSourceKind::Service, "Worker".into())));
    }

    #[test]
    fn duplicate_physical_and_embedded_logical_target_fails_closed() {
        let physical = vec![PhysicalRelSource::new(
            RelSourceKind::Module,
            "Auth",
            "module/Auth.module",
            "export function auth() { return true; }",
        )];
        let server = r#"server Main {}
[file-start:module.Auth]
export function auth() { return false; }
[file-end:module]
"#;
        let error = collect_original_sources(server, &physical).unwrap_err();
        assert!(matches!(
            error,
            RuntimeImageLinkedRelError::DuplicateOriginalSource { .. }
        ));
    }
}
