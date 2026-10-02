//! Recovery boundary for the disposable project-local OID compiler cache.
//!
//! `service_oid` owns the binary index/record formats. This module owns one
//! higher-level compiler rule: corrupted or stale cache state must never make
//! source compilation permanently fail. Recoverable cache-format/identity
//! failures delete only `.cache/compiler/oid` and retry once. Real compiler
//! failures (unsupported host target, exhausted OID space, I/O permissions,
//! invariants) remain visible and are never disguised as cache corruption.

use std::fs;
use std::path::{Path, PathBuf};

use crate::service_oid::{
    prepare_service_oid_cache as prepare_service_oid_cache_once, CoreMaterializationReport,
    OidCache, OidError,
};

pub fn oid_cache_root(project_root: &Path) -> PathBuf {
    project_root.join(".cache/compiler/oid")
}

pub fn prepare_service_oid_cache(
    project_root: &Path,
) -> Result<CoreMaterializationReport, OidError> {
    match prepare_service_oid_cache_once(project_root) {
        Ok(report) => Ok(report),
        Err(error) if recoverable_cache_error(&error) => {
            clear_service_oid_cache(project_root)?;
            prepare_service_oid_cache_once(project_root)
        }
        Err(error) => Err(error),
    }
}

pub fn open_service_oid_cache(project_root: &Path) -> Result<OidCache, OidError> {
    match OidCache::open_or_rebuild(project_root) {
        Ok(cache) => Ok(cache),
        Err(error) if recoverable_cache_error(&error) => {
            clear_service_oid_cache(project_root)?;
            OidCache::open_or_rebuild(project_root)
        }
        Err(error) => Err(error),
    }
}

pub fn clear_service_oid_cache(project_root: &Path) -> Result<(), OidError> {
    let root = oid_cache_root(project_root);
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            fs::remove_file(root)?;
        }
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(root)?;
        }
        Ok(_) => {
            fs::remove_file(root)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn recoverable_cache_error(error: &OidError) -> bool {
    matches!(
        error,
        OidError::InvalidIndex(_)
            | OidError::InvalidRecord(_)
            | OidError::TargetMismatch { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_cache_identity_and_format_errors_are_recoverable() {
        assert!(recoverable_cache_error(&OidError::InvalidIndex(
            "bad checksum".into()
        )));
        assert!(recoverable_cache_error(&OidError::InvalidRecord(
            "bad record".into()
        )));
        assert!(recoverable_cache_error(&OidError::TargetMismatch {
            expected: "a".into(),
            observed: "b".into(),
        }));
        assert!(!recoverable_cache_error(&OidError::UnsupportedTarget(
            "mips64".into()
        )));
        assert!(!recoverable_cache_error(&OidError::Exhausted(
            "package"
        )));
        assert!(!recoverable_cache_error(&OidError::Invariant(
            "compiler bug".into()
        )));
    }
}
