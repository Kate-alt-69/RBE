//! Selective native Service cache invalidation for Phase 3/4 OID changes.
//!
//! OID changes must not delete the whole compiler cache. Service plans already
//! pin exact OID identities/hashes and cached `.bin` headers already pin the
//! exact plan hash, so those existing artifacts are sufficient to invalidate
//! only the dependency closure that actually changed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::service_bin::{decode_cached_service_bin, ServiceAssemblyPlan};
use crate::service_native::NativeRuntimeImagePins;
use crate::service_oid_adapter::read_service_plan;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceCacheProtection {
    pub plan_hashes: BTreeSet<String>,
    pub assembly_hashes: BTreeSet<String>,
}

impl ServiceCacheProtection {
    /// Build a protection set from every Runtime Image that is still live.
    /// Callers should include the active image plus any draining/overlapping
    /// image whose workers may still execute a pinned Service artifact.
    pub fn from_runtime_images<'a>(
        images: impl IntoIterator<Item = &'a NativeRuntimeImagePins>,
    ) -> Self {
        let mut protection = Self::default();
        for image in images {
            for pin in image.services.values() {
                protection.plan_hashes.insert(pin.plan_hash.clone());
                protection.assembly_hashes.insert(pin.assembly_hash.clone());
            }
        }
        protection
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceCacheInvalidationReport {
    pub affected_oids: BTreeSet<u16>,
    /// Semantic dependency keys whose expected hash changed or disappeared.
    /// OID-only invalidation leaves this empty.
    pub affected_dependency_keys: BTreeSet<String>,
    pub removed_plan_hashes: BTreeSet<String>,
    pub removed_assembly_hashes: BTreeSet<String>,
    /// A live/draining Runtime Image still owns these exact cache artifacts.
    /// They intentionally survive until that image/worker releases its pin.
    pub retained_plan_hashes: BTreeSet<String>,
    pub retained_assembly_hashes: BTreeSet<String>,
    /// Corrupt/unrecognized cache entries are not treated as dependencies and
    /// are left for normal cache-corruption cleanup instead of broad deletion.
    pub skipped_unreadable_plans: usize,
    pub skipped_unreadable_bins: usize,
}

/// Remove only unprotected Service plans that require one of `affected_oids`,
/// followed by only bytecode bins whose embedded plan hash points at one of
/// those removed plans.
///
/// Protected artifacts are deliberately retained. A pinned old Runtime Image
/// may keep executing its old OID record while the next image receives a fresh
/// OID; deleting that old plan/bin would break the overlap guarantee Phase 5
/// exists to provide.
pub fn invalidate_service_cache_for_oids(
    project_root: &Path,
    affected_oids: impl IntoIterator<Item = u16>,
    protection: &ServiceCacheProtection,
) -> Result<ServiceCacheInvalidationReport, ServiceCacheInvalidationError> {
    let affected_oids = affected_oids.into_iter().collect::<BTreeSet<_>>();
    if affected_oids.is_empty() {
        return Ok(ServiceCacheInvalidationReport {
            affected_oids,
            ..ServiceCacheInvalidationReport::default()
        });
    }

    let mut report = invalidate_service_cache_matching(project_root, protection, |plan| {
        plan.required_oids
            .iter()
            .any(|required| affected_oids.contains(&required.oid))
    })?;
    report.affected_oids = affected_oids;
    Ok(report)
}

/// Invalidate unprotected plans whose stored dependency hash under one compiler
/// namespace no longer matches the current semantic identity.
///
/// This covers the case an OID keeps the same number and even emits identical
/// machine bytes while its source/capability/call-graph meaning changes. The
/// plan's `rel-semantic/...` binding is compiler truth for that higher-level
/// meaning; stale plans must not remain eligible for the next Runtime Image.
pub fn invalidate_service_cache_for_dependency_hashes(
    project_root: &Path,
    namespace_prefix: &str,
    expected_hashes: &BTreeMap<String, String>,
    protection: &ServiceCacheProtection,
) -> Result<ServiceCacheInvalidationReport, ServiceCacheInvalidationError> {
    if namespace_prefix.is_empty() {
        return Err(ServiceCacheInvalidationError::InvalidDependencyNamespace);
    }

    let mut affected_dependency_keys = BTreeSet::new();
    let mut report = invalidate_service_cache_matching(project_root, protection, |plan| {
        semantic_plan_is_stale(
            plan,
            namespace_prefix,
            expected_hashes,
            &mut affected_dependency_keys,
        )
    })?;
    report.affected_dependency_keys = affected_dependency_keys;
    Ok(report)
}

fn semantic_plan_is_stale(
    plan: &ServiceAssemblyPlan,
    namespace_prefix: &str,
    expected_hashes: &BTreeMap<String, String>,
    affected: &mut BTreeSet<String>,
) -> bool {
    let mut stale = false;
    for (key, observed_hash) in &plan.dependency_hashes {
        if !key.starts_with(namespace_prefix) {
            continue;
        }
        if expected_hashes.get(key) == Some(observed_hash) {
            continue;
        }
        affected.insert(key.clone());
        stale = true;
    }
    stale
}

fn invalidate_service_cache_matching(
    project_root: &Path,
    protection: &ServiceCacheProtection,
    mut invalid_plan: impl FnMut(&ServiceAssemblyPlan) -> bool,
) -> Result<ServiceCacheInvalidationReport, ServiceCacheInvalidationError> {
    let mut report = ServiceCacheInvalidationReport::default();
    let plan_root = project_root.join(".cache/compiler/service/plan");
    let bytecode_root = project_root.join(".cache/compiler/service/bytecode");

    let mut plan_deletes = Vec::<(String, PathBuf)>::new();
    for path in cache_files(&plan_root)? {
        let Some(plan_hash) = hash_filename(&path, false) else {
            continue;
        };
        let plan = match read_service_plan(project_root, &plan_hash) {
            Ok(plan) => plan,
            Err(_) => {
                report.skipped_unreadable_plans += 1;
                continue;
            }
        };
        if !invalid_plan(&plan) {
            continue;
        }
        if protection.plan_hashes.contains(&plan_hash) {
            report.retained_plan_hashes.insert(plan_hash);
            continue;
        }
        plan_deletes.push((plan_hash, path));
    }

    let invalid_plan_hashes = plan_deletes
        .iter()
        .map(|(hash, _)| hash.clone())
        .collect::<BTreeSet<_>>();
    let mut bin_deletes = Vec::<(String, PathBuf)>::new();
    for path in cache_files(&bytecode_root)? {
        let Some(assembly_hash) = hash_filename(&path, true) else {
            continue;
        };
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err(ServiceCacheInvalidationError::Io {
                    path,
                    error: error.to_string(),
                })
            }
        };
        let decoded = match decode_cached_service_bin(&bytes) {
            Ok(decoded) => decoded,
            Err(_) => {
                report.skipped_unreadable_bins += 1;
                continue;
            }
        };
        if !invalid_plan_hashes.contains(&decoded.plan_hash) {
            continue;
        }
        if protection.assembly_hashes.contains(&assembly_hash) {
            report.retained_assembly_hashes.insert(assembly_hash);
            continue;
        }
        bin_deletes.push((assembly_hash, path));
    }

    // Apply the exact unprotected delete set; unrelated and live-pinned cache
    // artifacts are never touched.
    for (plan_hash, path) in plan_deletes {
        remove_cache_file(&path)?;
        report.removed_plan_hashes.insert(plan_hash);
    }
    for (assembly_hash, path) in bin_deletes {
        remove_cache_file(&path)?;
        report.removed_assembly_hashes.insert(assembly_hash);
    }

    Ok(report)
}

fn cache_files(root: &Path) -> Result<Vec<PathBuf>, ServiceCacheInvalidationError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(ServiceCacheInvalidationError::Io {
                path: root.to_path_buf(),
                error: error.to_string(),
            })
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| ServiceCacheInvalidationError::Io {
            path: root.to_path_buf(),
            error: error.to_string(),
        })?;
        let path = entry.path();
        if path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn hash_filename(path: &Path, bin_suffix: bool) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let value = if bin_suffix {
        name.strip_suffix(".bin")?
    } else {
        name
    };
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(value.to_ascii_lowercase())
    } else {
        None
    }
}

fn remove_cache_file(path: &Path) -> Result<(), ServiceCacheInvalidationError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServiceCacheInvalidationError::Io {
            path: path.to_path_buf(),
            error: error.to_string(),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceCacheInvalidationError {
    Io { path: PathBuf, error: String },
    InvalidDependencyNamespace,
}

impl fmt::Display for ServiceCacheInvalidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, error } => write!(
                formatter,
                "service cache invalidation failed at {}: {error}",
                path.display()
            ),
            Self::InvalidDependencyNamespace => write!(
                formatter,
                "service cache semantic invalidation requires a non-empty dependency namespace"
            ),
        }
    }
}

impl std::error::Error for ServiceCacheInvalidationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_bin::{AssemblyRecordKind, RequiredOid, SERVICE_PLAN_FORMAT};

    fn sha(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn plan(dependencies: BTreeMap<String, String>) -> ServiceAssemblyPlan {
        ServiceAssemblyPlan {
            format: SERVICE_PLAN_FORMAT,
            service_identity: "worker".into(),
            service_source_sha256: sha('a'),
            index_identity_sha256: sha('b'),
            target_fingerprint: "test-target".into(),
            entry_oids: vec![30_458],
            required_oids: vec![RequiredOid {
                oid: 30_458,
                record_hash: sha('c'),
                kind: AssemblyRecordKind::ServiceExport,
            }],
            placement_order: vec![30_458],
            call_graph: BTreeMap::new(),
            service_data: Vec::new(),
            data_alignment: 8,
            dependency_hashes: dependencies,
            compile_options: BTreeMap::new(),
        }
    }

    #[test]
    fn accepts_only_content_addressed_cache_names() {
        let hash = "a".repeat(64);
        assert_eq!(
            hash_filename(Path::new(&hash), false).as_deref(),
            Some(hash.as_str())
        );
        let bin = format!("{hash}.bin");
        assert_eq!(
            hash_filename(Path::new(&bin), true).as_deref(),
            Some(hash.as_str())
        );
        assert!(hash_filename(Path::new("latest"), false).is_none());
        assert!(hash_filename(Path::new("abc.bin"), true).is_none());
    }

    #[test]
    fn affected_oid_set_is_deduplicated() {
        let root = std::env::temp_dir().join(format!(
            "rbe-service-invalidation-empty-{}",
            std::process::id()
        ));
        let report = invalidate_service_cache_for_oids(
            &root,
            [30_458, 30_458, 30_459],
            &ServiceCacheProtection::default(),
        )
        .unwrap();
        assert_eq!(report.affected_oids, BTreeSet::from([30_458, 30_459]));
    }

    #[test]
    fn semantic_hash_change_marks_plan_stale() {
        let key = "rel-semantic/service_worker_run".to_string();
        let old_hash = sha('d');
        let current_hash = sha('e');
        let plan = plan(BTreeMap::from([(key.clone(), old_hash)]));
        let mut affected = BTreeSet::new();
        assert!(semantic_plan_is_stale(
            &plan,
            "rel-semantic/",
            &BTreeMap::from([(key.clone(), current_hash)]),
            &mut affected,
        ));
        assert_eq!(affected, BTreeSet::from([key]));
    }

    #[test]
    fn removed_semantic_target_marks_plan_stale_but_other_namespace_does_not() {
        let rel_key = "rel-semantic/module_math_add".to_string();
        let pkg_key = "package/mail".to_string();
        let plan = plan(BTreeMap::from([
            (rel_key.clone(), sha('d')),
            (pkg_key, sha('e')),
        ]));
        let mut affected = BTreeSet::new();
        assert!(semantic_plan_is_stale(
            &plan,
            "rel-semantic/",
            &BTreeMap::new(),
            &mut affected,
        ));
        assert_eq!(affected, BTreeSet::from([rel_key]));
    }

    #[test]
    fn matching_semantic_hash_keeps_plan_valid() {
        let key = "rel-semantic/service_worker_run".to_string();
        let hash = sha('d');
        let plan = plan(BTreeMap::from([(key.clone(), hash.clone())]));
        let mut affected = BTreeSet::new();
        assert!(!semantic_plan_is_stale(
            &plan,
            "rel-semantic/",
            &BTreeMap::from([(key, hash)]),
            &mut affected,
        ));
        assert!(affected.is_empty());
    }
}
