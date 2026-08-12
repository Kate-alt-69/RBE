//! Six sandboxed execution environments: five general-purpose, one
//! dedicated to payment processing. Kept as a fixed, closed set (not
//! an arbitrary/dynamic pool) — the payment environment in particular
//! needs to be a known, specifically-configured thing, not "whichever
//! environment happened to be free."

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
        write!(f, "{}", self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    General,
    /// The one environment allowed to touch payment data. Everything
    /// it processes goes through encryption (see `payment` module) —
    /// never a "trusted because it's the payment one" exception to
    /// the abuse detection and health monitoring every environment
    /// gets; those apply here too, in addition to the encryption.
    Payment,
}
