from pathlib import Path
import re


def one(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


def edit(path: str, transform) -> None:
    file = Path(path)
    before = file.read_text(encoding="utf-8")
    after = transform(before)
    if after == before:
        raise SystemExit(f"{path}: transform made no change")
    file.write_text(after, encoding="utf-8")


Path("container-runtime/crates/environments/src/id.rs").write_text(
    r'''//! Fixed Environment identities plus Controller-owned logical profiles.
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
        assert_eq!(
            EnvironmentId::Payment.profile(),
            EnvironmentProfile::Secure
        );
        assert_eq!(EnvironmentId::SECURE, [EnvironmentId::Payment]);
    }
}
''',
    encoding="utf-8",
)

edit(
    "container-runtime/crates/environments/src/lib.rs",
    lambda text: one(
        text,
        "pub use id::{EnvironmentId, EnvironmentKind};",
        "pub use id::{EnvironmentId, EnvironmentKind, EnvironmentProfile};",
        "environments profile export",
    ),
)


def edit_core_lib(text: str) -> str:
    text = one(
        text,
        "AbuseDimension, AbuseVerdict, EncryptedPayload, EnvironmentId, EnvironmentKind,\n    EnvironmentRegistry, HealthStatus, PaymentEnvironment,",
        "AbuseDimension, AbuseVerdict, EncryptedPayload, EnvironmentId, EnvironmentKind,\n    EnvironmentProfile, EnvironmentRegistry, HealthStatus, PaymentEnvironment,",
        "core profile export",
    )
    return text


edit("container-runtime/crates/container-runtime-core/src/lib.rs", edit_core_lib)


def edit_runtime(text: str) -> str:
    text = one(
        text,
        "use environments::EnvironmentId;",
        "use environments::{EnvironmentId, EnvironmentProfile};",
        "runtime profile import",
    )
    marker = "    pub fn rebalance_once(&self) {"
    if text.count(marker) != 1:
        raise SystemExit(f"runtime rebalance anchor count={text.count(marker)}")
    method = '''    /// Resolve a logical profile to one exact live Environment. Profiles
    /// never become provenance identities; callers receive the exact binding.
    pub fn select_environment_profile(
        &self,
        profile: EnvironmentProfile,
    ) -> Option<EnvironmentId> {
        match profile {
            EnvironmentProfile::General => Some(self.select_general_environment()),
            EnvironmentProfile::Secure => EnvironmentId::SECURE
                .into_iter()
                .find(|id| self.has_environment(*id)),
        }
    }

'''
    return text.replace(marker, method + marker, 1)


edit("container-runtime/crates/container-runtime-core/src/runtime.rs", edit_runtime)


def edit_controller(text: str) -> str:
    text = one(
        text,
        "artifact_sha256_matches, CapabilityBroker, EnvironmentId, EnvironmentRegistry,",
        "artifact_sha256_matches, CapabilityBroker, EnvironmentId, EnvironmentProfile, EnvironmentRegistry,",
        "Controller profile import",
    )
    pattern = re.compile(
        r'''                let requested_environment = request\.environment\.clone\(\);\n'''
        r'''                let environment = if requested_environment == "general" \{\n'''
        r'''                    Some\(runtime\.select_general_environment\(\)\)\n'''
        r'''                \} else \{\n'''
        r'''                    parse_environment\(&requested_environment\)\n'''
        r'''                        \.filter\(\|id\| runtime\.has_environment\(\*id\)\)\n'''
        r'''                \};'''
    )
    replacement = '''                let requested_environment = request.environment.clone();
                let environment = if let Some(profile) =
                    EnvironmentProfile::parse(&requested_environment)
                {
                    runtime.select_environment_profile(profile)
                } else {
                    parse_environment(&requested_environment)
                        .filter(|id| runtime.has_environment(*id))
                };'''
    text, count = pattern.subn(replacement, text, count=1)
    if count != 1:
        raise SystemExit(f"Controller profile admission: replacements={count}")
    return text


edit("container-runtime/crates/container-bin/src/main.rs", edit_controller)

edit(
    "engine/crates/core/src/container_client.rs",
    lambda text: one(
        text,
        "a Controller-owned logical profile such as `general`. Controller returns",
        "a Controller-owned logical profile such as `general` or `secure`. Controller returns",
        "client profile docs",
    ),
)
