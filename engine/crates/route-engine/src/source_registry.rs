//! RELC source identity and registration primitives.
//!
//! Source roles describe capability/lifecycle ownership. They must never be
//! used to artificially gate grammar shared by REL source types.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The semantic role of a REL source.
///
/// This is deliberately separate from grammar: Route, Module, Service and
/// Server REL share the language grammar while exposing different runtime
/// capabilities and lifecycle surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RelSourceKind {
    Route,
    Module,
    Service,
    Server,
}

impl RelSourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Route => "route",
            Self::Module => "module",
            Self::Service => "service",
            Self::Server => "server",
        }
    }

    /// Classifies a physical REL source by its file name/extension.
    ///
    /// Server REL intentionally recognizes only the root `server.server`
    /// spelling. Arbitrary `*.server` files are not additional server roots.
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("route") => Some(Self::Route),
            Some("module") => Some(Self::Module),
            Some("service") => Some(Self::Service),
            Some("server")
                if path.file_name().and_then(|name| name.to_str()) == Some("server.server") =>
            {
                Some(Self::Server)
            }
            _ => None,
        }
    }
}

impl fmt::Display for RelSourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable identity used for diagnostics and dependency metadata.
///
/// Physical sources use `<kind>:<logical-name>`. Embedded sources preserve
/// their containing source in the identity, for example
/// `server:Main#module:Auth`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(String);

impl SourceId {
    pub fn physical(
        kind: RelSourceKind,
        logical_name: impl AsRef<str>,
    ) -> Result<Self, SourceRegistryError> {
        let logical_name = validate_logical_name(kind, logical_name.as_ref())?;
        Ok(Self(format!("{}:{logical_name}", kind.as_str())))
    }

    pub fn embedded(
        container: &SourceId,
        kind: RelSourceKind,
        logical_name: impl AsRef<str>,
    ) -> Result<Self, SourceRegistryError> {
        let logical_name = validate_logical_name(kind, logical_name.as_ref())?;
        Ok(Self(format!(
            "{}#{}:{logical_name}",
            container.as_str(),
            kind.as_str()
        )))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn validate_logical_name(
    kind: RelSourceKind,
    logical_name: &str,
) -> Result<&str, SourceRegistryError> {
    let logical_name = logical_name.trim();
    if logical_name.is_empty()
        || logical_name.contains('#')
        || logical_name.chars().any(char::is_control)
    {
        return Err(SourceRegistryError::InvalidLogicalName {
            kind,
            logical_name: logical_name.to_string(),
        });
    }
    Ok(logical_name)
}

/// Where RELC discovered a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceOrigin {
    Physical {
        path: PathBuf,
    },
    Embedded {
        container: SourceId,
        /// Zero-based block index inside the containing Server REL source.
        block_index: usize,
        /// One-based line where the embedded REL payload begins.
        start_line: usize,
    },
}

/// A source registered for RELC compilation.
#[derive(Debug, Clone)]
pub struct RelSource {
    id: SourceId,
    kind: RelSourceKind,
    logical_name: String,
    origin: SourceOrigin,
    source: Arc<str>,
}

impl RelSource {
    pub fn id(&self) -> &SourceId {
        &self.id
    }

    pub fn kind(&self) -> RelSourceKind {
        self.kind
    }

    pub fn logical_name(&self) -> &str {
        &self.logical_name
    }

    pub fn origin(&self) -> &SourceOrigin {
        &self.origin
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRegistryError {
    InvalidLogicalName {
        kind: RelSourceKind,
        logical_name: String,
    },
    DuplicateSourceId {
        id: SourceId,
    },
    DuplicateLogicalSource {
        kind: RelSourceKind,
        logical_name: String,
        existing: SourceId,
        attempted: SourceId,
    },
    DuplicatePhysicalPath {
        path: PathBuf,
        existing: SourceId,
        attempted: SourceId,
    },
    UnknownEmbeddedContainer {
        container: SourceId,
    },
    EmbeddedContainerMustBeServer {
        container: SourceId,
        actual: RelSourceKind,
    },
    EmbeddedServerNotAllowed {
        container: SourceId,
    },
}

impl fmt::Display for SourceRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLogicalName { kind, logical_name } => write!(
                formatter,
                "invalid {kind} REL logical source name `{logical_name}`"
            ),
            Self::DuplicateSourceId { id } => {
                write!(formatter, "duplicate REL source id `{id}`")
            }
            Self::DuplicateLogicalSource {
                kind,
                logical_name,
                existing,
                attempted,
            } => write!(
                formatter,
                "duplicate logical {kind} REL source `{logical_name}`: `{existing}` already owns that target; attempted `{attempted}`"
            ),
            Self::DuplicatePhysicalPath {
                path,
                existing,
                attempted,
            } => write!(
                formatter,
                "REL source path `{}` is already registered as `{existing}`; attempted `{attempted}`",
                path.display()
            ),
            Self::UnknownEmbeddedContainer { container } => write!(
                formatter,
                "embedded REL source container `{container}` is not registered"
            ),
            Self::EmbeddedContainerMustBeServer { container, actual } => write!(
                formatter,
                "embedded REL source container `{container}` is {actual} REL; only Server REL may contain embedded files"
            ),
            Self::EmbeddedServerNotAllowed { container } => write!(
                formatter,
                "Server REL source `{container}` cannot embed another Server REL source"
            ),
        }
    }
}

impl std::error::Error for SourceRegistryError {}

/// Deterministic registry for all REL sources discovered before full compile.
///
/// `SourceId` preserves physical/embedded diagnostic identity while the
/// logical index guarantees that one import target cannot silently resolve to
/// two different sources.
#[derive(Debug, Default)]
pub struct RelSourceRegistry {
    sources: BTreeMap<SourceId, RelSource>,
    logical_sources: BTreeMap<(RelSourceKind, String), SourceId>,
    physical_paths: BTreeMap<PathBuf, SourceId>,
}

impl RelSourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_physical(
        &mut self,
        kind: RelSourceKind,
        logical_name: impl AsRef<str>,
        path: impl Into<PathBuf>,
        source: impl Into<Arc<str>>,
    ) -> Result<SourceId, SourceRegistryError> {
        let logical_name = validate_logical_name(kind, logical_name.as_ref())?.to_string();
        let id = SourceId::physical(kind, &logical_name)?;
        let registered = RelSource {
            id: id.clone(),
            kind,
            logical_name,
            origin: SourceOrigin::Physical { path: path.into() },
            source: source.into(),
        };
        self.insert(registered)?;
        Ok(id)
    }

    pub fn register_embedded(
        &mut self,
        container: &SourceId,
        kind: RelSourceKind,
        logical_name: impl AsRef<str>,
        block_index: usize,
        start_line: usize,
        source: impl Into<Arc<str>>,
    ) -> Result<SourceId, SourceRegistryError> {
        let container_kind = self
            .sources
            .get(container)
            .map(RelSource::kind)
            .ok_or_else(|| SourceRegistryError::UnknownEmbeddedContainer {
                container: container.clone(),
            })?;

        if container_kind != RelSourceKind::Server {
            return Err(SourceRegistryError::EmbeddedContainerMustBeServer {
                container: container.clone(),
                actual: container_kind,
            });
        }
        if kind == RelSourceKind::Server {
            return Err(SourceRegistryError::EmbeddedServerNotAllowed {
                container: container.clone(),
            });
        }

        let logical_name = validate_logical_name(kind, logical_name.as_ref())?.to_string();
        let id = SourceId::embedded(container, kind, &logical_name)?;
        let registered = RelSource {
            id: id.clone(),
            kind,
            logical_name,
            origin: SourceOrigin::Embedded {
                container: container.clone(),
                block_index,
                start_line,
            },
            source: source.into(),
        };
        self.insert(registered)?;
        Ok(id)
    }

    fn insert(&mut self, source: RelSource) -> Result<(), SourceRegistryError> {
        let id = source.id.clone();
        if self.sources.contains_key(&id) {
            return Err(SourceRegistryError::DuplicateSourceId { id });
        }

        let logical_key = (source.kind, source.logical_name.clone());
        if let Some(existing) = self.logical_sources.get(&logical_key) {
            return Err(SourceRegistryError::DuplicateLogicalSource {
                kind: source.kind,
                logical_name: source.logical_name.clone(),
                existing: existing.clone(),
                attempted: id,
            });
        }

        let physical_path = match &source.origin {
            SourceOrigin::Physical { path } => Some(path.clone()),
            SourceOrigin::Embedded { .. } => None,
        };
        if let Some(path) = physical_path.as_ref() {
            if let Some(existing) = self.physical_paths.get(path) {
                return Err(SourceRegistryError::DuplicatePhysicalPath {
                    path: path.clone(),
                    existing: existing.clone(),
                    attempted: id,
                });
            }
        }

        self.logical_sources.insert(logical_key, id.clone());
        if let Some(path) = physical_path {
            self.physical_paths.insert(path, id.clone());
        }
        self.sources.insert(id, source);
        Ok(())
    }

    pub fn get(&self, id: &SourceId) -> Option<&RelSource> {
        self.sources.get(id)
    }

    pub fn get_logical(&self, kind: RelSourceKind, logical_name: &str) -> Option<&RelSource> {
        let key = (kind, logical_name.trim().to_string());
        self.logical_sources
            .get(&key)
            .and_then(|id| self.sources.get(id))
    }

    pub fn contains(&self, id: &SourceId) -> bool {
        self.sources.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &RelSource> {
        self.sources.values()
    }

    pub fn iter_kind(&self, kind: RelSourceKind) -> impl Iterator<Item = &RelSource> {
        self.sources
            .values()
            .filter(move |source| source.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_rel_physical_paths() {
        assert_eq!(
            RelSourceKind::from_path(Path::new("api/account.route")),
            Some(RelSourceKind::Route)
        );
        assert_eq!(
            RelSourceKind::from_path(Path::new("module/Auth.module")),
            Some(RelSourceKind::Module)
        );
        assert_eq!(
            RelSourceKind::from_path(Path::new("service/Mail.service")),
            Some(RelSourceKind::Service)
        );
        assert_eq!(
            RelSourceKind::from_path(Path::new("server.server")),
            Some(RelSourceKind::Server)
        );
        assert_eq!(RelSourceKind::from_path(Path::new("nested.server")), None);
    }

    #[test]
    fn source_ids_preserve_embedded_origin() {
        let server = SourceId::physical(RelSourceKind::Server, "Main").unwrap();
        let module = SourceId::embedded(&server, RelSourceKind::Module, "Auth").unwrap();
        assert_eq!(server.as_str(), "server:Main");
        assert_eq!(module.as_str(), "server:Main#module:Auth");
    }

    #[test]
    fn registers_physical_and_embedded_sources_without_changing_source_role() {
        let mut registry = RelSourceRegistry::new();
        let server = registry
            .register_physical(
                RelSourceKind::Server,
                "Main",
                "server.server",
                "server source",
            )
            .unwrap();
        let module = registry
            .register_embedded(
                &server,
                RelSourceKind::Module,
                "Auth",
                0,
                12,
                "export function auth() { return true; }",
            )
            .unwrap();

        let registered = registry.get(&module).unwrap();
        assert_eq!(registered.kind(), RelSourceKind::Module);
        assert_eq!(registered.logical_name(), "Auth");
        assert_eq!(
            registry
                .get_logical(RelSourceKind::Module, "Auth")
                .map(RelSource::id),
            Some(&module)
        );
        assert!(matches!(
            registered.origin(),
            SourceOrigin::Embedded {
                block_index: 0,
                start_line: 12,
                ..
            }
        ));
    }

    #[test]
    fn rejects_duplicate_logical_target_across_physical_and_embedded_sources() {
        let mut registry = RelSourceRegistry::new();
        let server = registry
            .register_physical(RelSourceKind::Server, "Main", "server.server", "")
            .unwrap();
        registry
            .register_physical(RelSourceKind::Module, "Auth", "module/Auth.module", "")
            .unwrap();

        let error = registry
            .register_embedded(&server, RelSourceKind::Module, "Auth", 0, 5, "")
            .unwrap_err();
        assert!(matches!(
            error,
            SourceRegistryError::DuplicateLogicalSource { .. }
        ));
    }

    #[test]
    fn rejects_duplicate_physical_path() {
        let mut registry = RelSourceRegistry::new();
        registry
            .register_physical(RelSourceKind::Module, "Auth", "module/shared.module", "")
            .unwrap();

        let error = registry
            .register_physical(RelSourceKind::Module, "Users", "module/shared.module", "")
            .unwrap_err();
        assert!(matches!(
            error,
            SourceRegistryError::DuplicatePhysicalPath { .. }
        ));
    }

    #[test]
    fn embedded_sources_require_a_registered_server_container() {
        let mut registry = RelSourceRegistry::new();
        let missing_server = SourceId::physical(RelSourceKind::Server, "Main").unwrap();
        let error = registry
            .register_embedded(&missing_server, RelSourceKind::Module, "Auth", 0, 1, "")
            .unwrap_err();
        assert!(matches!(
            error,
            SourceRegistryError::UnknownEmbeddedContainer { .. }
        ));

        let module = registry
            .register_physical(
                RelSourceKind::Module,
                "Container",
                "module/Container.module",
                "",
            )
            .unwrap();
        let error = registry
            .register_embedded(&module, RelSourceKind::Route, "/embedded", 0, 1, "")
            .unwrap_err();
        assert!(matches!(
            error,
            SourceRegistryError::EmbeddedContainerMustBeServer { .. }
        ));
    }

    #[test]
    fn server_rel_cannot_be_embedded() {
        let mut registry = RelSourceRegistry::new();
        let server = registry
            .register_physical(RelSourceKind::Server, "Main", "server.server", "")
            .unwrap();
        let error = registry
            .register_embedded(&server, RelSourceKind::Server, "Nested", 0, 1, "")
            .unwrap_err();
        assert!(matches!(
            error,
            SourceRegistryError::EmbeddedServerNotAllowed { .. }
        ));
    }
}
