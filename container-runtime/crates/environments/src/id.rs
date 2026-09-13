//! Fixed Environment identities plus Controller-owned logical profiles.
//!
//! Exact Environment IDs remain the execution/provenance identity. Profiles are
//! admission-time selectors only: Controller resolves a profile to one exact ID
//! and generation before registering capabilities or executing code.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentId {
    General1,
    General2,
    General3,
    General4,
    General5,
    Payment,
}

impl EnvironmentId {
    pub const ALL: [EnvironmentId; 6] = [
        EnvironmentId::General1,
        EnvironmentId::General2,
        EnvironmentId::General3,
        EnvironmentId::General4,
        EnvironmentId::General5,
        EnvironmentId::Payment,
    ];

    pub const GENERAL: [EnvironmentId; 5] = [
        EnvironmentId::General1,
        EnvironmentId::General2,
        EnvironmentId::General3,
        EnvironmentId::General4,
        EnvironmentId::General5,
    ];

    /// Current members of the logical `secure` profile. Payment remains the
    /// compatibility identity for the first secure Environment; future secure
    /// instances can join this set without changing profile callers.
    pub const SECURE: [EnvironmentId; 1] = [EnvironmentId::Payment];

    pub fn profile(self) -> EnvironmentProfile {
        match self {
            EnvironmentId::Payment => EnvironmentProfile::Secure,
            _ => EnvironmentProfile::General,
        }
    }

    pub fn kind(self) -> EnvironmentKind {
        match self {
            EnvironmentId::Payment => EnvironmentKind::Payment,
            _ => EnvironmentKind::General,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EnvironmentId::General1 => "general-1",
            EnvironmentId::General2 => "general-2",
            EnvironmentId::General3 => "general-3",
            EnvironmentId::General4 => "general-4",
            EnvironmentId::General5 => "general-5",
            EnvironmentId::Payment => "payment",
        }
    }
}

impl std::fmt::Display for EnvironmentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Logical scheduling/trust class owned by Container Controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentProfile {
    General,
    Secure,
}

impl EnvironmentProfile {
    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Secure => "secure",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "general" => Some(Self::General),
            "secure" => Some(Self::Secure),
            _ => None,
        }
    }
}

impl std::fmt::Display for EnvironmentProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    General,
    /// Compatibility kind for the first secure-profile Environment. Payment's
    /// encryption boundary remains intact while scheduling authority moves to
    /// the reusable `secure` profile abstraction.
    Payment,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_distinct_from_exact_environment_ids() {
        assert_eq!(
            EnvironmentProfile::parse("general"),
            Some(EnvironmentProfile::General)
        );
        assert_eq!(
            EnvironmentProfile::parse("secure"),
            Some(EnvironmentProfile::Secure)
        );
        assert_eq!(EnvironmentProfile::parse("payment"), None);
        assert_eq!(
            EnvironmentId::General3.profile(),
            EnvironmentProfile::General
        );
        assert_eq!(EnvironmentId::Payment.profile(), EnvironmentProfile::Secure);
        assert_eq!(EnvironmentId::SECURE, [EnvironmentId::Payment]);
    }
}
