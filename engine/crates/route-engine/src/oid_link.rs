//! Phase 3/4 Operation-ID ownership and linked-symbol planning.
//!
//! This module deliberately owns only the dynamic-linking policy. The Phase 1
//! OID cache/index implementation remains the storage authority, while the
//! Phase 2 code generator remains the machine-code authority. These types are
//! designed so the single `oid/index` can embed their ownership state without
//! introducing a second package/library index.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

pub const PACKAGE_OID_START: u16 = 20_086;
pub const PACKAGE_OID_END: u16 = 30_456;
pub const END_PACKAGE_OID: u16 = 30_457;
pub const REL_OID_START: u16 = 30_458;
pub const REL_OID_END: u16 = u16::MAX;

const PACKAGE_ID_PREFIX: &str = "lib_";
const REL_IDENTITY_DOMAIN: &[u8] = b"RBE_LINKED_REL_SYMBOL_V1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageExportSpec {
    /// Public name used by REL (`:import[send from mail]`). May contain `/` for
    /// future nested public surfaces; every segment is canonicalized separately.
    pub name: String,
    /// Stable ID published by the verified RPX package index.
    pub export_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageLinkSpec {
    pub package: String,
    pub version: String,
    pub artifact_sha256: String,
    pub exports: Vec<PackageExportSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageOidOwner {
    pub package: String,
    pub version: String,
    pub artifact_sha256: String,
    /// Stable RPX export ID -> project-local numeric OID.
    pub bindings: BTreeMap<String, u16>,
    /// Redundant by design: the single OID index must answer ownership queries
    /// without scanning every sparse `oid/<ID>` record.
    pub owned_oids: BTreeSet<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkedRelKind {
    Function,
    FirstClassFunction,
    ModuleExport,
    RouteExport,
    ServiceExport,
    Class,
    Constructor,
    Method,
}

impl LinkedRelKind {
    pub fn is_callable(self) -> bool {
        matches!(
            self,
            Self::Function
                | Self::FirstClassFunction
                | Self::ModuleExport
                | Self::RouteExport
                | Self::ServiceExport
                | Self::Constructor
                | Self::Method
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedRelSymbolSpec {
    /// Canonical project-local symbol identity, for example
    /// `module_users_findUser` or `class_UserCache`.
    pub canonical_id: String,
    pub kind: LinkedRelKind,
    pub source_sha256: String,
    #[serde(default)]
    pub required_symbols: BTreeSet<String>,
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
}

impl LinkedRelSymbolSpec {
    pub fn identity_sha256(&self) -> Result<String, OidLinkError> {
        validate_linked_symbol(self)?;
        let encoded = serde_json::to_vec(self)
            .map_err(|error| OidLinkError::Serialization(error.to_string()))?;
        let mut hash = Sha256::new();
        hash.update(REL_IDENTITY_DOMAIN);
        hash.update((encoded.len() as u64).to_be_bytes());
        hash.update(encoded);
        Ok(hex::encode(hash.finalize()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedRelBinding {
    pub canonical_id: String,
    pub kind: LinkedRelKind,
    pub oid: u16,
    pub source_sha256: String,
    pub identity_sha256: String,
    #[serde(default)]
    pub required_symbols: BTreeSet<String>,
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicOidOwnership {
    /// Package ownership stored inside the one project-local OID index.
    pub packages: BTreeMap<String, PackageOidOwner>,
    /// Canonical linked-REL identity -> binding in `30458..=65535`.
    pub rel: BTreeMap<String, LinkedRelBinding>,
}

/// Canonical RPX export ID. RPX publishes this stable identity; it never
/// publishes a project-local numeric OID.
pub fn canonical_package_export_id(
    package: &str,
    public_export: &str,
) -> Result<String, OidLinkError> {
    let mut out = String::from(PACKAGE_ID_PREFIX);
    out.push_str(&canonical_segment(package)?);
    if !public_export.is_empty() {
        for segment in public_export.split('/') {
            out.push('_');
            out.push_str(&canonical_segment(segment)?);
        }
    }
    if out.len() > 512 {
        return Err(OidLinkError::InvalidExportId(out));
    }
    Ok(out)
}

fn canonical_segment(value: &str) -> Result<String, OidLinkError> {
    if value.is_empty() || value.len() > 192 {
        return Err(OidLinkError::InvalidName(value.to_string()));
    }
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' | b'_' => out.push(byte as char),
            b'-' => out.push('_'),
            _ => return Err(OidLinkError::InvalidName(value.to_string())),
        }
    }
    if out.is_empty() {
        return Err(OidLinkError::InvalidName(value.to_string()));
    }
    Ok(out)
}

pub fn validate_package_spec(spec: &PackageLinkSpec) -> Result<(), OidLinkError> {
    canonical_segment(&spec.package)?;
    if spec.version.trim().is_empty()
        || spec.version.len() > 128
        || spec.version.chars().any(char::is_control)
    {
        return Err(OidLinkError::InvalidVersion(spec.version.clone()));
    }
    validate_sha256(&spec.artifact_sha256)?;
    if spec.exports.is_empty() {
        return Err(OidLinkError::NoPackageExports(spec.package.clone()));
    }

    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    for export in &spec.exports {
        if !names.insert(export.name.clone()) {
            return Err(OidLinkError::DuplicateExportName {
                package: spec.package.clone(),
                export: export.name.clone(),
            });
        }
        let expected = canonical_package_export_id(&spec.package, &export.name)?;
        if export.export_id != expected {
            return Err(OidLinkError::ExportIdMismatch {
                package: spec.package.clone(),
                export: export.name.clone(),
                expected,
                observed: export.export_id.clone(),
            });
        }
        if !ids.insert(export.export_id.clone()) {
            return Err(OidLinkError::ExportIdCollision {
                package: spec.package.clone(),
                export_id: export.export_id.clone(),
            });
        }
    }
    Ok(())
}

/// Reconcile verified RPX roots with the package-owned dynamic OID range.
///
/// Compatible exports in an unchanged verified artifact keep their OIDs. A
/// package update retires the old owner first and allocates the new artifact
/// from currently reusable holes. `pinned_oids` models Phase 5's old-image pins:
/// those slots are never reused even when their current owner disappeared.
pub fn reconcile_package_oids(
    current: &BTreeMap<String, PackageOidOwner>,
    desired: &[PackageLinkSpec],
    pinned_oids: &BTreeSet<u16>,
) -> Result<BTreeMap<String, PackageOidOwner>, OidLinkError> {
    let mut desired_map = BTreeMap::new();
    for spec in desired {
        validate_package_spec(spec)?;
        if desired_map.insert(spec.package.clone(), spec).is_some() {
            return Err(OidLinkError::DuplicatePackage(spec.package.clone()));
        }
    }
    validate_pins(pinned_oids, PACKAGE_OID_START, PACKAGE_OID_END)?;

    let mut occupied = pinned_oids.clone();
    let mut owners = BTreeMap::new();

    // Pass 1: preserve compatible assignments before allocating anything new.
    for (package, spec) in &desired_map {
        let old = current.get(package);
        let unchanged_artifact = old.is_some_and(|owner| {
            owner.version == spec.version && owner.artifact_sha256 == spec.artifact_sha256
        });
        let mut bindings = BTreeMap::new();
        if unchanged_artifact {
            let old = old.expect("checked above");
            for export in &spec.exports {
                if let Some(&oid) = old.bindings.get(&export.export_id) {
                    validate_oid_in_range(oid, PACKAGE_OID_START, PACKAGE_OID_END)?;
                    if !occupied.insert(oid) {
                        return Err(OidLinkError::OidCollision(oid));
                    }
                    bindings.insert(export.export_id.clone(), oid);
                }
            }
        }
        owners.insert(
            package.clone(),
            PackageOidOwner {
                package: package.clone(),
                version: spec.version.clone(),
                artifact_sha256: spec.artifact_sha256.clone(),
                bindings,
                owned_oids: BTreeSet::new(),
            },
        );
    }

    // Pass 2: deterministic hole-first allocation in package/export sort order.
    for (package, spec) in desired_map {
        let owner = owners
            .get_mut(&package)
            .expect("owner created during preservation pass");
        let mut exports = spec.exports.clone();
        exports.sort_by(|left, right| left.export_id.cmp(&right.export_id));
        for export in exports {
            if owner.bindings.contains_key(&export.export_id) {
                continue;
            }
            let oid = next_free_oid(PACKAGE_OID_START, PACKAGE_OID_END, &occupied)
                .ok_or(OidLinkError::PackageRangeExhausted)?;
            occupied.insert(oid);
            owner.bindings.insert(export.export_id, oid);
        }
        owner.owned_oids = owner.bindings.values().copied().collect();
    }

    Ok(owners)
}

pub fn reconcile_rel_oids(
    current: &BTreeMap<String, LinkedRelBinding>,
    desired: &[LinkedRelSymbolSpec],
    pinned_oids: &BTreeSet<u16>,
) -> Result<BTreeMap<String, LinkedRelBinding>, OidLinkError> {
    validate_pins(pinned_oids, REL_OID_START, REL_OID_END)?;
    let mut specs = BTreeMap::new();
    for spec in desired {
        validate_linked_symbol(spec)?;
        if specs.insert(spec.canonical_id.clone(), spec).is_some() {
            return Err(OidLinkError::DuplicateLinkedSymbol(
                spec.canonical_id.clone(),
            ));
        }
    }
    validate_rel_graph(&specs)?;

    let mut occupied = pinned_oids.clone();
    let mut bindings = BTreeMap::new();

    // Preserve identity slots when symbol identity/kind still agree. Source/hash
    // changes regenerate the sparse record but do not force an unrelated renumber.
    for (canonical_id, spec) in &specs {
        if let Some(old) = current.get(canonical_id) {
            if old.kind == spec.kind {
                validate_oid_in_range(old.oid, REL_OID_START, REL_OID_END)?;
                if !occupied.insert(old.oid) {
                    return Err(OidLinkError::OidCollision(old.oid));
                }
                bindings.insert(
                    canonical_id.clone(),
                    linked_binding(spec, old.oid)?,
                );
            }
        }
    }

    for (canonical_id, spec) in specs {
        if bindings.contains_key(&canonical_id) {
            continue;
        }
        let oid = next_free_oid(REL_OID_START, REL_OID_END, &occupied)
            .ok_or(OidLinkError::RelRangeExhausted)?;
        occupied.insert(oid);
        bindings.insert(canonical_id, linked_binding(spec, oid)?);
    }
    Ok(bindings)
}

fn linked_binding(spec: &LinkedRelSymbolSpec, oid: u16) -> Result<LinkedRelBinding, OidLinkError> {
    Ok(LinkedRelBinding {
        canonical_id: spec.canonical_id.clone(),
        kind: spec.kind,
        oid,
        source_sha256: spec.source_sha256.clone(),
        identity_sha256: spec.identity_sha256()?,
        required_symbols: spec.required_symbols.clone(),
        capabilities: spec.capabilities.clone(),
    })
}

fn validate_linked_symbol(spec: &LinkedRelSymbolSpec) -> Result<(), OidLinkError> {
    validate_canonical_rel_id(&spec.canonical_id)?;
    validate_sha256(&spec.source_sha256)?;
    for dependency in &spec.required_symbols {
        validate_canonical_rel_id(dependency)?;
    }
    for capability in &spec.capabilities {
        if capability.is_empty()
            || capability.len() > 192
            || capability.chars().any(char::is_control)
        {
            return Err(OidLinkError::InvalidCapability(capability.clone()));
        }
    }
    Ok(())
}

fn validate_rel_graph(
    specs: &BTreeMap<String, &LinkedRelSymbolSpec>,
) -> Result<(), OidLinkError> {
    for (id, spec) in specs {
        for dependency in &spec.required_symbols {
            let target = specs
                .get(dependency)
                .ok_or_else(|| OidLinkError::MissingLinkedDependency {
                    symbol: id.clone(),
                    dependency: dependency.clone(),
                })?;
            if spec.kind == LinkedRelKind::Class
                && !matches!(target.kind, LinkedRelKind::Constructor | LinkedRelKind::Method)
            {
                return Err(OidLinkError::InvalidClassMemberKind {
                    class: id.clone(),
                    member: dependency.clone(),
                    kind: target.kind,
                });
            }
        }
    }
    Ok(())
}

/// Compute the exact OID closure required by a service. The graph may contain
/// stable core OIDs, package OIDs and linked REL OIDs; unused nodes never enter
/// the returned dependency set.
pub fn reachable_oids(
    entries: impl IntoIterator<Item = u16>,
    edges: &BTreeMap<u16, BTreeSet<u16>>,
) -> BTreeSet<u16> {
    let mut reachable = BTreeSet::new();
    let mut queue = VecDeque::new();
    for oid in entries {
        if reachable.insert(oid) {
            queue.push_back(oid);
        }
    }
    while let Some(oid) = queue.pop_front() {
        if let Some(dependencies) = edges.get(&oid) {
            for &dependency in dependencies {
                if reachable.insert(dependency) {
                    queue.push_back(dependency);
                }
            }
        }
    }
    reachable
}

pub fn rel_edges_by_oid(
    bindings: &BTreeMap<String, LinkedRelBinding>,
) -> Result<BTreeMap<u16, BTreeSet<u16>>, OidLinkError> {
    let mut edges = BTreeMap::new();
    for binding in bindings.values() {
        let mut required = BTreeSet::new();
        for dependency in &binding.required_symbols {
            let target = bindings
                .get(dependency)
                .ok_or_else(|| OidLinkError::MissingLinkedDependency {
                    symbol: binding.canonical_id.clone(),
                    dependency: dependency.clone(),
                })?;
            required.insert(target.oid);
        }
        edges.insert(binding.oid, required);
    }
    Ok(edges)
}

fn next_free_oid(start: u16, end: u16, occupied: &BTreeSet<u16>) -> Option<u16> {
    let mut oid = start;
    loop {
        if !occupied.contains(&oid) {
            return Some(oid);
        }
        if oid == end {
            return None;
        }
        oid = oid.checked_add(1)?;
    }
}

fn validate_pins(pins: &BTreeSet<u16>, start: u16, end: u16) -> Result<(), OidLinkError> {
    for &oid in pins {
        validate_oid_in_range(oid, start, end)?;
    }
    Ok(())
}

fn validate_oid_in_range(oid: u16, start: u16, end: u16) -> Result<(), OidLinkError> {
    if (start..=end).contains(&oid) {
        Ok(())
    } else {
        Err(OidLinkError::OidOutsideDynamicRange { oid, start, end })
    }
}

fn validate_sha256(value: &str) -> Result<(), OidLinkError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(OidLinkError::InvalidSha256(value.to_string()))
    }
}

fn validate_canonical_rel_id(value: &str) -> Result<(), OidLinkError> {
    if value.is_empty()
        || value.len() > 512
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        Err(OidLinkError::InvalidLinkedSymbol(value.to_string()))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OidLinkError {
    InvalidName(String),
    InvalidVersion(String),
    InvalidSha256(String),
    InvalidExportId(String),
    DuplicatePackage(String),
    NoPackageExports(String),
    DuplicateExportName {
        package: String,
        export: String,
    },
    ExportIdMismatch {
        package: String,
        export: String,
        expected: String,
        observed: String,
    },
    ExportIdCollision {
        package: String,
        export_id: String,
    },
    InvalidLinkedSymbol(String),
    InvalidCapability(String),
    DuplicateLinkedSymbol(String),
    MissingLinkedDependency {
        symbol: String,
        dependency: String,
    },
    InvalidClassMemberKind {
        class: String,
        member: String,
        kind: LinkedRelKind,
    },
    OidOutsideDynamicRange {
        oid: u16,
        start: u16,
        end: u16,
    },
    OidCollision(u16),
    PackageRangeExhausted,
    RelRangeExhausted,
    Serialization(String),
}

impl fmt::Display for OidLinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(value) => write!(formatter, "invalid package/export segment {value:?}"),
            Self::InvalidVersion(value) => write!(formatter, "invalid package version {value:?}"),
            Self::InvalidSha256(value) => write!(formatter, "invalid SHA-256 {value:?}"),
            Self::InvalidExportId(value) => write!(formatter, "invalid RPX export ID {value:?}"),
            Self::DuplicatePackage(package) => write!(formatter, "duplicate package {package:?}"),
            Self::NoPackageExports(package) => write!(formatter, "package {package:?} has no public exports"),
            Self::DuplicateExportName { package, export } => write!(formatter, "duplicate public export {export:?} in package {package:?}"),
            Self::ExportIdMismatch { package, export, expected, observed } => write!(formatter, "RPX export ID mismatch for {export:?} from {package:?}: expected {expected:?}, observed {observed:?}"),
            Self::ExportIdCollision { package, export_id } => write!(formatter, "canonical RPX export ID collision in package {package:?}: {export_id:?}"),
            Self::InvalidLinkedSymbol(value) => write!(formatter, "invalid canonical linked REL symbol {value:?}"),
            Self::InvalidCapability(value) => write!(formatter, "invalid linked REL capability {value:?}"),
            Self::DuplicateLinkedSymbol(value) => write!(formatter, "duplicate linked REL symbol {value:?}"),
            Self::MissingLinkedDependency { symbol, dependency } => write!(formatter, "linked REL symbol {symbol:?} requires missing symbol {dependency:?}"),
            Self::InvalidClassMemberKind { class, member, kind } => write!(formatter, "class {class:?} references {member:?} with invalid member kind {kind:?}"),
            Self::OidOutsideDynamicRange { oid, start, end } => write!(formatter, "OID {oid} is outside dynamic range {start}..={end}"),
            Self::OidCollision(oid) => write!(formatter, "dynamic OID {oid} is assigned/pinned more than once"),
            Self::PackageRangeExhausted => write!(formatter, "package OID range 20086..=30456 is exhausted"),
            Self::RelRangeExhausted => write!(formatter, "linked REL OID range 30458..=65535 is exhausted"),
            Self::Serialization(message) => write!(formatter, "failed to serialize OID-link identity: {message}"),
        }
    }
}

impl std::error::Error for OidLinkError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(ch: char) -> String {
        std::iter::repeat(ch).take(64).collect()
    }

    fn package(version: &str, artifact: char, exports: &[&str]) -> PackageLinkSpec {
        let package = "mail".to_string();
        PackageLinkSpec {
            package: package.clone(),
            version: version.to_string(),
            artifact_sha256: sha(artifact),
            exports: exports
                .iter()
                .map(|name| PackageExportSpec {
                    name: (*name).to_string(),
                    export_id: canonical_package_export_id(&package, name).unwrap(),
                })
                .collect(),
        }
    }

    #[test]
    fn canonical_rpx_ids_follow_library_contract() {
        assert_eq!(canonical_package_export_id("mail", "").unwrap(), "lib_mail");
        assert_eq!(canonical_package_export_id("mail", "send").unwrap(), "lib_mail_send");
        assert_eq!(
            canonical_package_export_id("my-mail", "client/send-fast").unwrap(),
            "lib_my_mail_client_send_fast"
        );
    }

    #[test]
    fn canonicalization_collision_is_rejected() {
        let spec = package("1.0.0", 'a', &["send-fast", "send_fast"]);
        assert!(matches!(
            validate_package_spec(&spec),
            Err(OidLinkError::ExportIdCollision { .. })
        ));
    }

    #[test]
    fn incremental_package_rebuild_preserves_compatible_oids() {
        let first = reconcile_package_oids(
            &BTreeMap::new(),
            &[package("1.0.0", 'a', &["send", "receive"])],
            &BTreeSet::new(),
        )
        .unwrap();
        let second = reconcile_package_oids(
            &first,
            &[package("1.0.0", 'a', &["send", "receive", "status"])],
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            first["mail"].bindings["lib_mail_send"],
            second["mail"].bindings["lib_mail_send"]
        );
        assert_eq!(second["mail"].bindings["lib_mail_status"], 20_088);
    }

    #[test]
    fn package_update_retires_owner_and_respects_pinned_slots() {
        let first = reconcile_package_oids(
            &BTreeMap::new(),
            &[package("1.0.0", 'a', &["send", "receive"])],
            &BTreeSet::new(),
        )
        .unwrap();
        let pinned_oid = first["mail"].bindings["lib_mail_send"];
        let pinned = BTreeSet::from([pinned_oid]);
        let second = reconcile_package_oids(
            &first,
            &[package("2.0.0", 'b', &["send"])],
            &pinned,
        )
        .unwrap();
        assert_ne!(second["mail"].bindings["lib_mail_send"], pinned_oid);
        assert_eq!(second["mail"].bindings["lib_mail_send"], 20_086);
    }

    #[test]
    fn removed_unpinned_package_slot_is_reused_without_renumbering_other_owner() {
        let mail = package("1.0.0", 'a', &["send"]);
        let mut auth = package("1.0.0", 'b', &["login"]);
        auth.package = "auth".into();
        auth.exports[0].export_id = canonical_package_export_id("auth", "login").unwrap();
        let first = reconcile_package_oids(&BTreeMap::new(), &[mail, auth.clone()], &BTreeSet::new()).unwrap();
        let auth_oid = first["auth"].bindings["lib_auth_login"];

        let mut files = package("1.0.0", 'c', &["read"]);
        files.package = "files".into();
        files.exports[0].export_id = canonical_package_export_id("files", "read").unwrap();
        let second = reconcile_package_oids(&first, &[auth, files], &BTreeSet::new()).unwrap();
        assert_eq!(second["auth"].bindings["lib_auth_login"], auth_oid);
        assert_eq!(second["files"].bindings["lib_files_read"], 20_087);
    }

    fn rel(id: &str, kind: LinkedRelKind, deps: &[&str]) -> LinkedRelSymbolSpec {
        LinkedRelSymbolSpec {
            canonical_id: id.into(),
            kind,
            source_sha256: sha('d'),
            required_symbols: deps.iter().map(|value| (*value).to_string()).collect(),
            capabilities: BTreeSet::new(),
        }
    }

    #[test]
    fn class_descriptor_stacks_only_constructor_and_method_oids() {
        let desired = vec![
            rel("class_UserCache", LinkedRelKind::Class, &["ctor_UserCache", "method_UserCache_get"]),
            rel("ctor_UserCache", LinkedRelKind::Constructor, &[]),
            rel("method_UserCache_get", LinkedRelKind::Method, &[]),
        ];
        let bindings = reconcile_rel_oids(&BTreeMap::new(), &desired, &BTreeSet::new()).unwrap();
        let edges = rel_edges_by_oid(&bindings).unwrap();
        let class = bindings["class_UserCache"].oid;
        assert_eq!(edges[&class].len(), 2);
        assert!(edges[&class].contains(&bindings["ctor_UserCache"].oid));
        assert!(edges[&class].contains(&bindings["method_UserCache_get"].oid));
    }

    #[test]
    fn reachability_excludes_unused_linked_symbols() {
        let edges = BTreeMap::from([
            (30_458, BTreeSet::from([30_459, 20_086])),
            (30_459, BTreeSet::from([96])),
            (30_500, BTreeSet::from([97])),
        ]);
        let reachable = reachable_oids([30_458], &edges);
        assert_eq!(
            reachable,
            BTreeSet::from([96, 20_086, 30_458, 30_459])
        );
        assert!(!reachable.contains(&30_500));
    }
}
