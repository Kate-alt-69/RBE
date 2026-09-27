use semver::{Version, VersionReq};

#[derive(Debug, thiserror::Error)]
pub enum VersionMatchError {
    #[error("invalid version requirement {value:?}: {source}")]
    InvalidRequirement {
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error("invalid resolved version {value:?}: {source}")]
    InvalidVersion {
        value: String,
        #[source]
        source: semver::Error,
    },
}

/// Check one exact resolved version against a package-authored semver requirement.
///
/// This keeps semver parsing inside the resolver crate so trusted consumers can
/// verify lock/manifest compatibility without duplicating resolver semantics.
pub fn version_satisfies_requirement(
    requirement: &str,
    resolved_version: &str,
) -> Result<bool, VersionMatchError> {
    let requirement = VersionReq::parse(requirement).map_err(|source| {
        VersionMatchError::InvalidRequirement {
            value: requirement.to_string(),
            source,
        }
    })?;
    let version = Version::parse(resolved_version).map_err(|source| VersionMatchError::InvalidVersion {
        value: resolved_version.to_string(),
        source,
    })?;
    Ok(requirement.matches(&version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_resolved_version_is_checked_against_requirement() {
        assert!(version_satisfies_requirement("^1.3", "1.9.4").unwrap());
        assert!(!version_satisfies_requirement("^1.3", "2.0.0").unwrap());
    }

    #[test]
    fn malformed_requirements_and_versions_fail_closed() {
        assert!(matches!(
            version_satisfies_requirement("not-semver", "1.0.0"),
            Err(VersionMatchError::InvalidRequirement { .. })
        ));
        assert!(matches!(
            version_satisfies_requirement("^1", "latest"),
            Err(VersionMatchError::InvalidVersion { .. })
        ));
    }
}
