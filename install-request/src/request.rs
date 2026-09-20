use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{ExternalLocator, InstallRequestError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallCommand {
    pub target: InstallTarget,
    pub flags: InstallFlags,
}

impl InstallCommand {
    pub fn parse(args: &[&str]) -> Result<Self, InstallRequestError> {
        let Some(raw_target) = args.first().copied() else {
            return Err(InstallRequestError::MissingTarget);
        };
        let mut flags = InstallFlags::parse(&args[1..])?;
        let mut target = InstallTarget::parse(raw_target)?;

        if let Some(flag_version) = flags.version.clone() {
            match target.version() {
                Some(inline) if inline != &flag_version => {
                    return Err(InstallRequestError::ConflictingVersion {
                        inline: inline.raw.clone(),
                        flag: flag_version.raw,
                    });
                }
                Some(_) => {}
                None => target.set_version(flag_version),
            }
        }
        if flags.version.is_none() {
            flags.version = target.version().cloned();
        }
        Ok(Self { target, flags })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallTarget {
    Named {
        key: String,
        version: Option<VersionSelector>,
    },
    External {
        locator: Box<ExternalLocator>,
        version: Option<VersionSelector>,
    },
    LocalArchive(PathBuf),
}

impl InstallTarget {
    pub fn parse(value: &str) -> Result<Self, InstallRequestError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(InstallRequestError::MissingTarget);
        }
        if looks_like_local_path(value) {
            return Ok(Self::LocalArchive(PathBuf::from(value)));
        }
        if let Some((key, version)) = split_named_version(value)? {
            return Ok(Self::Named {
                key,
                version: Some(version),
            });
        }
        if is_reserved_install_namespace(value) {
            validate_install_key(value)?;
            return Ok(Self::Named {
                key: value.to_string(),
                version: None,
            });
        }
        if looks_like_external(value) {
            return Ok(Self::External {
                locator: Box::new(ExternalLocator::parse(value)?),
                version: None,
            });
        }
        validate_install_key(value)?;
        Ok(Self::Named {
            key: value.to_string(),
            version: None,
        })
    }

    pub fn version(&self) -> Option<&VersionSelector> {
        match self {
            Self::Named { version, .. } | Self::External { version, .. } => version.as_ref(),
            Self::LocalArchive(_) => None,
        }
    }

    fn set_version(&mut self, version: VersionSelector) {
        match self {
            Self::Named { version: slot, .. } | Self::External { version: slot, .. } => {
                *slot = Some(version)
            }
            Self::LocalArchive(_) => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionSelector {
    pub raw: String,
    pub requirement: String,
}

impl VersionSelector {
    pub fn parse(value: &str) -> Result<Self, InstallRequestError> {
        let value = value.trim();
        if value.is_empty() || value.len() > 64 {
            return Err(InstallRequestError::InvalidVersion(value.to_string()));
        }
        let parts: Vec<&str> = value.split('.').collect();
        if parts.is_empty()
            || parts.len() > 3
            || parts
                .iter()
                .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return Err(InstallRequestError::InvalidVersion(value.to_string()));
        }
        let numbers = parts
            .iter()
            .map(|part| {
                part.parse::<u32>()
                    .map_err(|_| InstallRequestError::InvalidVersion(value.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let requirement = match numbers.as_slice() {
            [major] => format!(">={major}.0.0,<{}.0.0", major.saturating_add(1)),
            [major, minor] => format!(">={major}.{minor}.0,<{major}.{}.0", minor.saturating_add(1)),
            [major, minor, patch] => format!("={major}.{minor}.{patch}"),
            _ => unreachable!("version parts are bounded to 1..=3"),
        };
        Ok(Self {
            raw: value.to_string(),
            requirement,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallFlags {
    pub version: Option<VersionSelector>,
    pub shared: bool,
    pub force: bool,
    pub no_cache: bool,
    pub refresh_index: bool,
    pub json: bool,
    pub quiet: bool,
}

impl InstallFlags {
    pub fn parse(args: &[&str]) -> Result<Self, InstallRequestError> {
        let mut flags = Self::default();
        let mut index = 0usize;
        while index < args.len() {
            let flag = args[index];
            if let Some(value) = flag
                .strip_prefix("-version=")
                .or_else(|| flag.strip_prefix("--version="))
            {
                set_version_flag(&mut flags, value)?;
            } else if matches!(flag, "-version" | "--version") {
                index += 1;
                let value = args
                    .get(index)
                    .copied()
                    .ok_or(InstallRequestError::MissingFlagValue("version"))?;
                set_version_flag(&mut flags, value)?;
            } else {
                match flag {
                    "-shared" | "--shared" => flags.shared = true,
                    "-force" | "--force" => flags.force = true,
                    "-no-cache" | "--no-cache" => flags.no_cache = true,
                    "-refresh-index" | "--refresh-index" => flags.refresh_index = true,
                    "-json" | "--json" => flags.json = true,
                    "-quiet" | "--quiet" => flags.quiet = true,
                    _ => return Err(InstallRequestError::UnknownFlag(flag.to_string())),
                }
            }
            index += 1;
        }
        Ok(flags)
    }
}

fn set_version_flag(flags: &mut InstallFlags, value: &str) -> Result<(), InstallRequestError> {
    if flags.version.is_some() {
        return Err(InstallRequestError::DuplicateVersionFlag);
    }
    flags.version = Some(VersionSelector::parse(value)?);
    Ok(())
}

fn split_named_version(
    value: &str,
) -> Result<Option<(String, VersionSelector)>, InstallRequestError> {
    if value.contains('/') || value.contains(':') || value.contains('\\') {
        return Ok(None);
    }
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() < 2 {
        return Ok(None);
    }
    for suffix_len in (1..=3.min(parts.len() - 1)).rev() {
        let split = parts.len() - suffix_len;
        let suffix = &parts[split..];
        if suffix
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        {
            let key = parts[..split].join(".");
            validate_install_key(&key)?;
            return Ok(Some((key, VersionSelector::parse(&suffix.join("."))?)));
        }
    }
    Ok(None)
}

fn is_reserved_install_namespace(value: &str) -> bool {
    value == "sdk" || value.starts_with("sdk.") || value.starts_with("runtime.")
}

fn validate_install_key(value: &str) -> Result<(), InstallRequestError> {
    if value.is_empty() || value.len() > 192 {
        return Err(InstallRequestError::InvalidInstallKey(value.to_string()));
    }
    for segment in value.split('.') {
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            return Err(InstallRequestError::InvalidInstallKey(value.to_string()));
        };
        if !(first.is_ascii_lowercase() || first.is_ascii_digit())
            || !chars
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
        {
            return Err(InstallRequestError::InvalidInstallKey(value.to_string()));
        }
    }
    Ok(())
}

fn looks_like_external(value: &str) -> bool {
    if value.contains("://") || value.contains('/') {
        return true;
    }
    let host = value.split(['?', '#']).next().unwrap_or(value);
    host.contains('.') && !host.starts_with('.') && !host.ends_with('.')
}

fn looks_like_local_path(value: &str) -> bool {
    value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with(".\\")
        || value.starts_with("..\\")
        || value.starts_with('/')
        || value.ends_with(".zip")
        || value.ends_with(".rbe-pkg")
        || (value.len() >= 3
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'\\' | b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_named_and_namespaced_versions_parse() {
        let package = InstallCommand::parse(&["advancenet.4.0.1"]).unwrap();
        assert!(matches!(
            package.target,
            InstallTarget::Named { ref key, ref version }
                if key == "advancenet" && version.as_ref().unwrap().requirement == "=4.0.1"
        ));
        let runtime = InstallCommand::parse(&["runtime.python.3.10"]).unwrap();
        assert!(matches!(
            runtime.target,
            InstallTarget::Named { ref key, ref version }
                if key == "runtime.python" && version.as_ref().unwrap().requirement == ">=3.10.0,<3.11.0"
        ));
        let runtime_latest = InstallCommand::parse(&["runtime.python"]).unwrap();
        assert!(matches!(
            runtime_latest.target,
            InstallTarget::Named { ref key, version: None } if key == "runtime.python"
        ));
        let sdk = InstallCommand::parse(&["sdk.0.1.0", "-shared"]).unwrap();
        assert!(matches!(sdk.target, InstallTarget::Named { ref key, .. } if key == "sdk"));
        assert!(sdk.flags.shared);
    }

    #[test]
    fn bare_domain_stays_an_external_target() {
        let command = InstallCommand::parse(&["python.org"]).unwrap();
        assert!(matches!(command.target, InstallTarget::External { .. }));
    }

    #[test]
    fn version_flag_applies_to_external_url() {
        let command = InstallCommand::parse(&["python.org/downloads/", "-version=3.10"]).unwrap();
        assert!(matches!(
            command.target,
            InstallTarget::External { ref version, .. }
                if version.as_ref().unwrap().requirement == ">=3.10.0,<3.11.0"
        ));
    }

    #[test]
    fn inline_and_flag_versions_must_agree() {
        let error = InstallCommand::parse(&["advancenet.4.0.1", "-version=4.1.0"]).unwrap_err();
        assert!(matches!(
            error,
            InstallRequestError::ConflictingVersion { .. }
        ));
    }
}
