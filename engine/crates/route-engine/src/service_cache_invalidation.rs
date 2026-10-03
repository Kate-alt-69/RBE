//! Selective native Service cache invalidation for Phase 3/4 OID changes.
//!
//! OID changes must not delete the whole compiler cache. Service plans already
//! pin exact OID identities/hashes and cached `.bin` headers already pin the
//! exact plan hash, so those existing artifacts are sufficient to invalidate
//! only the dependency closure that actually changed.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::service_bin::decode_cached_service_bin;
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
                protection
                    .assembly_hashes
                    .insert(pin.assembly_hash.clone());
            }
        }
        protection
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceCacheInvalidationReport {
    pub affected_oids: BTreeSet<u16>,
    pub removed_plan_hashes: BTreeSet<String>,
    pub removed_assembly_hashes: BTreeSet<String>,
    /// Corrupt/unrecognized cache entries are not treated as dependencies and
    /// are left for normal cache-corruption cleanup instead of broad deletion.
    pub skipped_unreadable_plans: usize,
    pub skipped_unreadable_bins: usize,
}

/// Remove only Service plans that require one of `affected_oids`, followed by
/// only bytecode bins whose embedded plan hash points at one of those plans.
///
/// This is deliberately a preflight-then-delete operation. If any affected
/// artifact is still pinned by a live Runtime Image, no files are removed and
/// the caller gets a hard liveness error instead of silently breaking overlap.
pub fn invalidate_service_cache_for_oids(
    project_root: &Path,
    affected_oids: impl IntoIterator<Item = u16>,
    protection: &ServiceCacheProtection,
) -> Result<ServiceCacheInvalidationReport, ServiceCacheInvalidationError> {
    let affected_oids = affected_oids.into_iter().collect::<BTreeSet<_>>();
    let mut report = ServiceCacheInvalidationReport {
        affected_oids: affected_oids.clone(),
        ..ServiceCacheInvalidationReport::default()
    };
    if affected_oids.is_empty() {
        return Ok(report);
    }

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
        let intersects = plan
            .required_oids
            .iter()
            .any(|required| affected_oids.contains(&required.oid));
        if !intersects {
            continue;
        }
        if protection.plan_hashes.contains(&plan_hash) {
            return Err(ServiceCacheInvalidationError::PinnedPlanAffected {
                plan_hash,
                affected_oids: plan
                    .required_oids
                    .iter()
                    .filter_map(|required| {
                        affected_oids
                            .contains(&required.oid)
                            .then_some(required.oid)
                    })
                    .collect(),
            });
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
            return Err(ServiceCacheInvalidationError::PinnedBinAffected {
                assembly_hash,
                plan_hash: decoded.plan_hash,
            });
        }
        bin_deletes.push((assembly_hash, path));
    }

    // No liveness conflict exists. Apply the previously computed exact delete
    // set; unrelated plans/bins are never touched.
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
    Io {
        path: PathBuf,
        error: String,
    },
    PinnedPlanAffected {
        plan_hash: String,
        affected_oids: BTreeSet<u16>,
    },
    PinnedBinAffected {
        assembly_hash: String,
        plan_hash: String,
    },
}

impl fmt::Display for ServiceCacheInvalidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, error } => write!(
                formatter,
                "service cache invalidation failed at {}: {error}",
                path.display()
            ),
            Self::PinnedPlanAffected {
                plan_hash,
                affected_oids,
            } => write!(
                formatter,
                "service plan {plan_hash} is still pinned but depends on changing OIDs {affected_oids:?}"
            ),
            Self::PinnedBinAffected {
                assembly_hash,
                plan_hash,
            } => write!(
                formatter,
                "service bin {assembly_hash} (plan {plan_hash}) is still pinned and cannot be invalidated"
            ),
        }
    }
}

impl std::error::Error for ServiceCacheInvalidationError {}

#[cfg(test)]
mod tests {
    use super::*;

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
}
