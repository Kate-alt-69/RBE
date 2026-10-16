//! Source-accurate Phase 4 discovery from one already-compiled Runtime Image.
//!
//! RELC already parsed every Route/Module/Service into `RuntimeExecutable`.
//! Re-parsing those sources for OID discovery would create a second semantic
//! path and could drift from the image that is actually being activated. This
//! bridge therefore combines the immutable parsed ASTs with the exact original
//! physical/embedded source bytes only to derive per-source SHA-256 identity.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sha2::{Digest, Sha256};

use crate::embedded_rel::{extract_embedded_rel, EmbeddedRelError};
use crate::rel_symbol_discovery::{
    discover_linked_rel_symbols, linked_source_sha256, LinkedRelDiscovery, LinkedRelDiscoveryError,
    LinkedRelSourceUnit,
};
use crate::relc::PhysicalRelSource;
use crate::runtime_image::{RuntimeCapabilityRequirement, RuntimeExecutable, RuntimeImage};
use crate::source_registry::RelSourceKind;

type SourceKey = (RelSourceKind, String);

const CAPABILITY_IDENTITY_DOMAIN: &[u8] = b"RBE_RUNTIME_CAPABILITY_IDENTITY_V1";

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
    let mut capability_identities = BTreeMap::<String, (String, BTreeSet<String>)>::new();

    for manifest in &image.sources {
        let kind = manifest.kind;
        if !matches!(
            kind,
            RelSourceKind::Route | RelSourceKind::Module | RelSourceKind::Service
        ) {
            continue;
        }

        let key = (kind, manifest.logical_name.clone());
        let source =
            sources
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

        let source_sha256 = linked_source_sha256(source);
        let capabilities = image
            .capability_requirements(&manifest.id)
            .into_iter()
            .flatten()
            .map(capability_identity)
            .collect::<BTreeSet<_>>();
        let source_label = manifest.id.to_string();
        if let Some((existing_source, existing_capabilities)) =
            capability_identities.get(&source_sha256)
        {
            if existing_capabilities != &capabilities {
                return Err(RuntimeImageLinkedRelError::AmbiguousSourceCapabilities {
                    source_sha256,
                    first_source: existing_source.clone(),
                    second_source: source_label,
                });
            }
        } else {
            capability_identities.insert(source_sha256, (source_label, capabilities));
        }

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

    let mut discovery =
        discover_linked_rel_symbols(&units).map_err(RuntimeImageLinkedRelError::Discovery)?;
    for symbol in &mut discovery.symbols {
        let (_, capabilities) = capability_identities
            .get(&symbol.source_sha256)
            .ok_or_else(
                || RuntimeImageLinkedRelError::MissingCapabilityIdentitySource {
                    canonical_id: symbol.canonical_id.clone(),
                    source_sha256: symbol.source_sha256.clone(),
                },
            )?;
        symbol.capabilities = capabilities.clone();
    }
    Ok(discovery)
}

/// OID linking needs capability *identity* so a permission-semantic change
/// participates in the linked symbol hash. Runtime Image remains the only
/// authorization authority: this opaque fingerprint is never interpreted as a
/// second grant/policy language by the OID cache or service assembler.
fn capability_identity(requirement: &RuntimeCapabilityRequirement) -> String {
    let mut hash = Sha256::new();
    hash.update(CAPABILITY_IDENTITY_DOMAIN);
    match requirement {
        RuntimeCapabilityRequirement::PublicHttp { operation } => {
            feed_identity(&mut hash, b"public-http");
            feed_identity(&mut hash, operation.as_bytes());
        }
        RuntimeCapabilityRequirement::Storage { owner, operation } => {
            feed_identity(&mut hash, b"storage");
            feed_identity(&mut hash, owner.as_bytes());
            feed_identity(&mut hash, operation.as_bytes());
        }
        RuntimeCapabilityRequirement::Video { owner, operation } => {
            feed_identity(&mut hash, b"video");
            feed_identity(&mut hash, owner.as_bytes());
            feed_identity(&mut hash, operation.as_bytes());
        }
        RuntimeCapabilityRequirement::Service { service, operation } => {
            feed_identity(&mut hash, b"service");
            feed_identity(&mut hash, service.as_bytes());
            feed_identity(&mut hash, operation.as_bytes());
        }
    }
    format!("cap-sha256:{}", hex::encode(hash.finalize()))
}

fn feed_identity(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
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
    AmbiguousSourceCapabilities {
        source_sha256: String,
        first_source: String,
        second_source: String,
    },
    MissingCapabilityIdentitySource {
        canonical_id: String,
        source_sha256: String,
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
            Self::AmbiguousSourceCapabilities {
                source_sha256,
                first_source,
                second_source,
            } => write!(
                formatter,
                "identical REL source identity {source_sha256} resolves to different Runtime Image capabilities for {first_source} and {second_source}; linked OID identity cannot be chosen safely"
            ),
            Self::MissingCapabilityIdentitySource {
                canonical_id,
                source_sha256,
            } => write!(
                formatter,
                "linked REL symbol {canonical_id:?} references source identity {source_sha256}, but no Runtime Image capability identity was recorded"
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

    #[test]
    fn capability_identity_is_stable_and_typed() {
        let get = RuntimeCapabilityRequirement::PublicHttp {
            operation: "get".into(),
        };
        let post = RuntimeCapabilityRequirement::PublicHttp {
            operation: "post".into(),
        };
        let service = RuntimeCapabilityRequirement::Service {
            service: "mail".into(),
            operation: "get".into(),
        };

        assert_eq!(capability_identity(&get), capability_identity(&get));
        assert_ne!(capability_identity(&get), capability_identity(&post));
        assert_ne!(capability_identity(&get), capability_identity(&service));
        assert!(capability_identity(&get).starts_with("cap-sha256:"));
    }
}
