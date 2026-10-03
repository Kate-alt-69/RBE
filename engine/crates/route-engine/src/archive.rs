//! Archive capability planning for REL.
//!
//! Archive bytes remain host-owned. REL receives symbolic workspace paths and
//! bounded operations; the trusted host performs parsing/extraction/editing.

use std::fmt;

use crate::workspace::{WorkspacePath, WorkspacePlanError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    Tar,
    TarGz,
    TarXz,
}

impl ArchiveFormat {
    pub fn infer(path: &str) -> Result<Self, ArchivePlanError> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".zip") {
            Ok(Self::Zip)
        } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Ok(Self::TarGz)
        } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
            Ok(Self::TarXz)
        } else if lower.ends_with(".tar") {
            Ok(Self::Tar)
        } else {
            Err(ArchivePlanError::UnsupportedFormat(path.to_string()))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArchivePath(String);

impl ArchivePath {
    pub fn parse(value: &str) -> Result<Self, ArchivePlanError> {
        let normalized = value.replace('\\', "/");
        if normalized.is_empty()
            || normalized.starts_with('/')
            || normalized.contains('\0')
            || normalized.split('/').any(|segment| segment == "..")
        {
            return Err(ArchivePlanError::InvalidEntryPath(value.to_string()));
        }
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchivePlan {
    List {
        archive: WorkspacePath,
        format: ArchiveFormat,
    },
    Read {
        archive: WorkspacePath,
        format: ArchiveFormat,
        entry: ArchivePath,
    },
    Extract {
        archive: WorkspacePath,
        format: ArchiveFormat,
        entries: Vec<ArchivePath>,
        destination: WorkspacePath,
    },
    Create {
        source: WorkspacePath,
        destination: WorkspacePath,
        format: ArchiveFormat,
    },
    Replace {
        archive: WorkspacePath,
        format: ArchiveFormat,
        entry: ArchivePath,
        source: WorkspacePath,
    },
    Remove {
        archive: WorkspacePath,
        format: ArchiveFormat,
        entry: ArchivePath,
    },
}

impl ArchivePlan {
    pub fn list(path: &str) -> Result<Self, ArchivePlanError> {
        let archive = WorkspacePath::parse(path).map_err(ArchivePlanError::Workspace)?;
        let format = ArchiveFormat::infer(archive.relative())?;
        Ok(Self::List { archive, format })
    }

    pub fn read(path: &str, entry: &str) -> Result<Self, ArchivePlanError> {
        let archive = WorkspacePath::parse(path).map_err(ArchivePlanError::Workspace)?;
        let format = ArchiveFormat::infer(archive.relative())?;
        Ok(Self::Read {
            archive,
            format,
            entry: ArchivePath::parse(entry)?,
        })
    }

    pub fn extract(
        path: &str,
        entries: Vec<String>,
        destination: &str,
    ) -> Result<Self, ArchivePlanError> {
        let archive = WorkspacePath::parse(path).map_err(ArchivePlanError::Workspace)?;
        let destination = WorkspacePath::parse(destination).map_err(ArchivePlanError::Workspace)?;
        let format = ArchiveFormat::infer(archive.relative())?;
        let entries = entries
            .into_iter()
            .map(|entry| ArchivePath::parse(&entry))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::Extract {
            archive,
            format,
            entries,
            destination,
        })
    }

    pub fn create(source: &str, destination: &str) -> Result<Self, ArchivePlanError> {
        let source = WorkspacePath::parse(source).map_err(ArchivePlanError::Workspace)?;
        let destination = WorkspacePath::parse(destination).map_err(ArchivePlanError::Workspace)?;
        let format = ArchiveFormat::infer(destination.relative())?;
        Ok(Self::Create {
            source,
            destination,
            format,
        })
    }

    pub fn replace(path: &str, entry: &str, source: &str) -> Result<Self, ArchivePlanError> {
        let archive = WorkspacePath::parse(path).map_err(ArchivePlanError::Workspace)?;
        let source = WorkspacePath::parse(source).map_err(ArchivePlanError::Workspace)?;
        let format = ArchiveFormat::infer(archive.relative())?;
        Ok(Self::Replace {
            archive,
            format,
            entry: ArchivePath::parse(entry)?,
            source,
        })
    }

    pub fn remove(path: &str, entry: &str) -> Result<Self, ArchivePlanError> {
        let archive = WorkspacePath::parse(path).map_err(ArchivePlanError::Workspace)?;
        let format = ArchiveFormat::infer(archive.relative())?;
        Ok(Self::Remove {
            archive,
            format,
            entry: ArchivePath::parse(entry)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveWarmingKey {
    pub archive: WorkspacePath,
    pub format: ArchiveFormat,
}

impl ArchiveWarmingKey {
    pub fn from_path(path: &str) -> Result<Self, ArchivePlanError> {
        let archive = WorkspacePath::parse(path).map_err(ArchivePlanError::Workspace)?;
        let format = ArchiveFormat::infer(archive.relative())?;
        Ok(Self { archive, format })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchivePlanError {
    Workspace(WorkspacePlanError),
    UnsupportedFormat(String),
    InvalidEntryPath(String),
}

impl fmt::Display for ArchivePlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace(error) => write!(f, "{error}"),
            Self::UnsupportedFormat(path) => write!(
                f,
                "unsupported archive format for {path:?}; expected .zip, .tar, .tar.gz/.tgz, or .tar.xz/.txz"
            ),
            Self::InvalidEntryPath(path) => write!(f, "invalid archive entry path {path:?}"),
        }
    }
}

impl std::error::Error for ArchivePlanError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_archive_traversal() {
        assert!(ArchivePath::parse("../secret").is_err());
        assert!(ArchivePath::parse("folder/../../secret").is_err());
        assert!(ArchivePath::parse("folder/file.txt").is_ok());
    }

    #[test]
    fn infers_supported_formats() {
        assert_eq!(
            ArchiveFormat::infer("package.zip").unwrap(),
            ArchiveFormat::Zip
        );
        assert_eq!(
            ArchiveFormat::infer("package.tar.gz").unwrap(),
            ArchiveFormat::TarGz
        );
        assert_eq!(
            ArchiveFormat::infer("package.tgz").unwrap(),
            ArchiveFormat::TarGz
        );
        assert_eq!(
            ArchiveFormat::infer("package.tar.xz").unwrap(),
            ArchiveFormat::TarXz
        );
    }
}
