//! Safe extraction planning and deterministic source-tree identity.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rbe_library_package::{ArchiveEntry, InspectedPackage};
use sha2::{Digest, Sha256};

pub const SOURCE_TREE_HASH_ALGORITHM: &str = "rbe-source-tree-sha256-v1";
pub const DEFAULT_MAX_SOURCE_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractionHardening {
    pub require_fresh_root: bool,
    pub reject_existing_destinations: bool,
    pub follow_symlinks: bool,
    pub preserve_archive_permissions: bool,
    pub preserve_archive_timestamps: bool,
}

impl Default for ExtractionHardening {
    fn default() -> Self {
        Self {
            require_fresh_root: true,
            reject_existing_destinations: true,
            follow_symlinks: false,
            preserve_archive_permissions: false,
            preserve_archive_timestamps: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionEntry {
    pub archive_path: String,
    pub destination: PathBuf,
    pub size: u64,
    pub directory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionPlan {
    pub root: PathBuf,
    pub entries: Vec<ExtractionEntry>,
    pub total_uncompressed_bytes: u64,
    pub hardening: ExtractionHardening,
}

impl ExtractionPlan {
    /// Builds a plan only from an already-inspected package. The executor must
    /// create `root` itself and enforce the hardening flags while materializing
    /// each regular file.
    pub fn from_inspected(
        package: &InspectedPackage,
        root: impl AsRef<Path>,
    ) -> Result<Self, SourceStageError> {
        let root = root.as_ref();
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(SourceStageError::ExtractionRootMustBeAbsolute);
        }

        let mut entries = Vec::with_capacity(package.entries.len());
        let mut seen = BTreeSet::new();
        let mut total = 0_u64;
        for entry in &package.entries {
            let canonical = validate_archive_entry(entry)?;
            let collision_key = canonical.to_ascii_lowercase();
            if !seen.insert(collision_key) {
                return Err(SourceStageError::DuplicatePath(canonical));
            }
            total = total
                .checked_add(entry.size)
                .ok_or(SourceStageError::SourceSizeOverflow)?;
            let destination = join_canonical(root, &canonical);
            if !destination.starts_with(root) {
                return Err(SourceStageError::UnsafePath(canonical));
            }
            entries.push(ExtractionEntry {
                archive_path: canonical,
                destination,
                size: entry.size,
                directory: entry.directory,
            });
        }
        if total != package.total_uncompressed_bytes {
            return Err(SourceStageError::InspectedSizeMismatch {
                expected: package.total_uncompressed_bytes,
                actual: total,
            });
        }

        Ok(Self {
            root: root.to_path_buf(),
            entries,
            total_uncompressed_bytes: total,
            hardening: ExtractionHardening::default(),
        })
    }
}

fn validate_archive_entry(entry: &ArchiveEntry) -> Result<String, SourceStageError> {
    let raw = entry.path.trim_end_matches('/');
    if raw.is_empty()
        || entry.path.contains('\\')
        || entry.path.starts_with('/')
        || entry.path.contains(':')
    {
        return Err(SourceStageError::UnsafePath(entry.path.clone()));
    }
    for part in raw.split('/') {
        if part.is_empty() || matches!(part, "." | "..") {
            return Err(SourceStageError::UnsafePath(entry.path.clone()));
        }
    }
    let mut canonical = raw.to_string();
    if entry.directory {
        canonical.push('/');
    }
    Ok(canonical)
}

fn join_canonical(root: &Path, canonical: &str) -> PathBuf {
    let mut destination = root.to_path_buf();
    for part in canonical.trim_end_matches('/').split('/') {
        destination.push(part);
    }
    destination
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSelection {
    roots: Vec<String>,
}

impl SourceSelection {
    pub fn new<I, S>(roots: I) -> Result<Self, SourceStageError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut normalized = BTreeSet::new();
        for root in roots {
            let root = normalize_source_root(&root.into())?;
            normalized.insert(root);
        }
        if normalized.is_empty() {
            return Err(SourceStageError::EmptySourceSelection);
        }
        Ok(Self {
            roots: normalized.into_iter().collect(),
        })
    }

    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    pub fn includes(&self, path: &str) -> bool {
        self.roots.iter().any(|root| {
            path == root
                || path
                    .strip_prefix(root)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    }
}

fn normalize_source_root(value: &str) -> Result<String, SourceStageError> {
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty()
        || value.contains('\\')
        || value.starts_with('/')
        || value.contains(':')
        || trimmed
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(SourceStageError::UnsafeSourceRoot(value.to_string()));
    }
    Ok(trimmed.to_string())
}

#[derive(Debug, Clone)]
pub struct SourceFileHasher {
    path: String,
    expected_size: u64,
    observed_size: u64,
    maximum_size: u64,
    hasher: Sha256,
}

impl SourceFileHasher {
    pub fn new(path: impl Into<String>, expected_size: u64) -> Result<Self, SourceStageError> {
        Self::with_limit(path, expected_size, DEFAULT_MAX_SOURCE_FILE_BYTES)
    }

    pub fn with_limit(
        path: impl Into<String>,
        expected_size: u64,
        maximum_size: u64,
    ) -> Result<Self, SourceStageError> {
        let path = normalize_source_file_path(&path.into())?;
        if maximum_size == 0 || expected_size > maximum_size {
            return Err(SourceStageError::SourceFileTooLarge {
                path,
                size: expected_size,
                limit: maximum_size,
            });
        }
        Ok(Self {
            path,
            expected_size,
            observed_size: 0,
            maximum_size,
            hasher: Sha256::new(),
        })
    }

    pub fn update(&mut self, bytes: &[u8]) -> Result<(), SourceStageError> {
        let next = self
            .observed_size
            .checked_add(bytes.len() as u64)
            .ok_or(SourceStageError::SourceSizeOverflow)?;
        if next > self.maximum_size || next > self.expected_size {
            return Err(SourceStageError::SourceFileTooLarge {
                path: self.path.clone(),
                size: next,
                limit: self.expected_size.min(self.maximum_size),
            });
        }
        self.hasher.update(bytes);
        self.observed_size = next;
        Ok(())
    }

    pub fn finish(self) -> Result<SourceFileDigest, SourceStageError> {
        if self.observed_size != self.expected_size {
            return Err(SourceStageError::SourceFileSizeMismatch {
                path: self.path,
                expected: self.expected_size,
                actual: self.observed_size,
            });
        }
        Ok(SourceFileDigest {
            path: self.path,
            size: self.observed_size,
            sha256: format!("{:x}", self.hasher.finalize()),
        })
    }
}

fn normalize_source_file_path(value: &str) -> Result<String, SourceStageError> {
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty()
        || value.ends_with('/')
        || value.contains('\\')
        || value.starts_with('/')
        || value.contains(':')
        || trimmed
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(SourceStageError::UnsafePath(value.to_string()));
    }
    Ok(trimmed.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFileDigest {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTreeDigest {
    pub algorithm: &'static str,
    pub sha256: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

impl SourceTreeDigest {
    pub fn from_files(
        selection: &SourceSelection,
        files: impl IntoIterator<Item = SourceFileDigest>,
    ) -> Result<Self, SourceStageError> {
        let mut selected = files
            .into_iter()
            .filter(|file| selection.includes(&file.path))
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Err(SourceStageError::NoSelectedSourceFiles);
        }
        selected.sort_by(|left, right| left.path.cmp(&right.path));

        let mut seen = BTreeSet::new();
        let mut total_bytes = 0_u64;
        let mut hasher = Sha256::new();
        hasher.update(b"RBE-SOURCE-TREE-SHA256-V1\0");
        for file in &selected {
            normalize_source_file_path(&file.path)?;
            validate_sha256(&file.sha256)?;
            if !seen.insert(file.path.to_ascii_lowercase()) {
                return Err(SourceStageError::DuplicatePath(file.path.clone()));
            }
            total_bytes = total_bytes
                .checked_add(file.size)
                .ok_or(SourceStageError::SourceSizeOverflow)?;
            hasher.update((file.path.len() as u64).to_be_bytes());
            hasher.update(file.path.as_bytes());
            hasher.update(file.size.to_be_bytes());
            hasher.update(file.sha256.to_ascii_lowercase().as_bytes());
        }

        Ok(Self {
            algorithm: SOURCE_TREE_HASH_ALGORITHM,
            sha256: format!("{:x}", hasher.finalize()),
            file_count: selected.len(),
            total_bytes,
        })
    }
}

fn validate_sha256(value: &str) -> Result<(), SourceStageError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SourceStageError::InvalidSha256(value.to_string()));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum SourceStageError {
    #[error("extraction root must be an absolute path")]
    ExtractionRootMustBeAbsolute,
    #[error("unsafe source/archive path {0:?}")]
    UnsafePath(String),
    #[error("unsafe source root {0:?}")]
    UnsafeSourceRoot(String),
    #[error("duplicate or case-colliding source path {0:?}")]
    DuplicatePath(String),
    #[error("source size accounting overflow")]
    SourceSizeOverflow,
    #[error("inspected package size mismatch: expected {expected}, got {actual}")]
    InspectedSizeMismatch { expected: u64, actual: u64 },
    #[error("source selection must contain at least one root")]
    EmptySourceSelection,
    #[error("source selection matched no regular files")]
    NoSelectedSourceFiles,
    #[error("source file {path:?} is {size} bytes, above limit {limit}")]
    SourceFileTooLarge { path: String, size: u64, limit: u64 },
    #[error("source file {path:?} size mismatch: expected {expected}, got {actual}")]
    SourceFileSizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbe_library_package::{LibraryManifest, RuntimeSpec, SdkSpec};

    fn package(entries: Vec<ArchiveEntry>) -> InspectedPackage {
        let total_uncompressed_bytes = entries.iter().map(|entry| entry.size).sum();
        InspectedPackage {
            manifest: LibraryManifest {
                name: "demo".into(),
                version: "1.0.0".into(),
                language: rbe_library_package::Language::Rust,
                rbe_abi_min: 1,
                rbe_abi_max: 1,
                sdk: SdkSpec {
                    family: "rust".into(),
                    package: "rbe-sdk".into(),
                    version: "0.1".into(),
                },
                runtime: RuntimeSpec {
                    kind: "rust".into(),
                    version: "1.98".into(),
                    managed: true,
                    entry: "src/main.rs".into(),
                },
                exports: Default::default(),
                capabilities: Default::default(),
                build_capabilities: Default::default(),
                dependencies: Default::default(),
                build: Default::default(),
            },
            entries,
            total_uncompressed_bytes,
        }
    }

    #[test]
    fn extraction_plan_keeps_every_destination_under_fresh_root() {
        let inspected = package(vec![
            ArchiveEntry {
                path: "src/".into(),
                size: 0,
                directory: true,
            },
            ArchiveEntry {
                path: "src/main.rs".into(),
                size: 12,
                directory: false,
            },
        ]);
        let plan = ExtractionPlan::from_inspected(&inspected, "/cache/staging/pkg").unwrap();
        assert_eq!(plan.entries.len(), 2);
        assert!(plan
            .entries
            .iter()
            .all(|entry| entry.destination.starts_with(&plan.root)));
        assert!(plan.hardening.require_fresh_root);
        assert!(!plan.hardening.follow_symlinks);
        assert!(!plan.hardening.preserve_archive_permissions);
    }

    #[test]
    fn defense_in_depth_rejects_manually_constructed_traversal() {
        let inspected = package(vec![ArchiveEntry {
            path: "../evil".into(),
            size: 1,
            directory: false,
        }]);
        assert!(matches!(
            ExtractionPlan::from_inspected(&inspected, "/cache/staging/pkg"),
            Err(SourceStageError::UnsafePath(_))
        ));
    }

    #[test]
    fn source_tree_digest_is_stable_across_file_order() {
        let selection = SourceSelection::new(["src", "Cargo.toml"]).unwrap();
        let a = SourceFileDigest {
            path: "src/main.rs".into(),
            size: 3,
            sha256: "a".repeat(64),
        };
        let b = SourceFileDigest {
            path: "Cargo.toml".into(),
            size: 4,
            sha256: "b".repeat(64),
        };
        let first = SourceTreeDigest::from_files(&selection, [a.clone(), b.clone()]).unwrap();
        let second = SourceTreeDigest::from_files(&selection, [b, a]).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.algorithm, SOURCE_TREE_HASH_ALGORITHM);
    }

    #[test]
    fn source_selection_excludes_shipped_binary() {
        let selection = SourceSelection::new(["src"]).unwrap();
        let digest = SourceTreeDigest::from_files(
            &selection,
            [
                SourceFileDigest {
                    path: "src/main.rs".into(),
                    size: 3,
                    sha256: "a".repeat(64),
                },
                SourceFileDigest {
                    path: "dist/mycoolpackage.exe".into(),
                    size: 999,
                    sha256: "b".repeat(64),
                },
            ],
        )
        .unwrap();
        assert_eq!(digest.file_count, 1);
        assert_eq!(digest.total_bytes, 3);
    }

    #[test]
    fn source_file_hasher_is_streaming_and_size_bounded() {
        let mut hasher = SourceFileHasher::new("src/main.rs", 3).unwrap();
        hasher.update(b"a").unwrap();
        hasher.update(b"bc").unwrap();
        let digest = hasher.finish().unwrap();
        assert_eq!(
            digest.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
