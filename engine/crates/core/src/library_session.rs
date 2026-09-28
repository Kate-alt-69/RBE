use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::library_host::{
    CapabilityGrant, ExpectedWorkerIdentity, HostCall, LibraryHostError, LibrarySession,
    PackageReply, SessionState, WorkerHello, LIBRARY_PROTOCOL_VERSION,
};

pub const LIBRARY_HOST_CALL_FEATURE: &str = "host.call:v1";
const MAX_CAPABILITY_IDENTITY_BYTES: usize = 256;

/// Read-only metadata returned after a package worker completes the verified
/// Library Protocol handshake.
///
/// `granted_capabilities` is derived from the exact [`CapabilityGrant`] values
/// installed into the same [`LibrarySession`]. It is therefore descriptive of
/// already-admitted authority and can never widen that authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedLibrarySessionInfo {
    pub protocol: u32,
    pub abi: u32,
    pub capability_identity: String,
    pub granted_capabilities: BTreeSet<String>,
    pub features: BTreeSet<String>,
}

impl AcceptedLibrarySessionInfo {
    pub fn to_value(&self) -> Value {
        json!({
            "type": "library.accept",
            "protocol": self.protocol,
            "abi": self.abi,
            "capabilityIdentity": self.capability_identity,
            "grantedCapabilities": self.granted_capabilities,
            "features": self.features,
        })
    }
}

/// Couples one enforcement session with the exact metadata that may be exposed
/// to the accepted worker.
///
/// Callers provide the opaque, session-scoped capability identity. The binding
/// derives capability names from the exact grant vector before moving those
/// grants into [`LibrarySession`], preventing metadata/enforcement drift.
pub struct LibrarySessionBinding {
    session: LibrarySession,
    accepted: AcceptedLibrarySessionInfo,
}

impl LibrarySessionBinding {
    pub fn new(
        expected: ExpectedWorkerIdentity,
        grants: impl IntoIterator<Item = CapabilityGrant>,
        capability_identity: impl Into<String>,
    ) -> Result<Self, LibraryHostError> {
        let capability_identity = capability_identity.into();
        validate_capability_identity(&capability_identity)?;

        let grants = grants.into_iter().collect::<Vec<_>>();
        let granted_capabilities = grants
            .iter()
            .map(|grant| grant.capability.clone())
            .collect::<BTreeSet<_>>();
        let abi = expected.abi;
        let session = LibrarySession::new(expected, grants)?;

        Ok(Self {
            session,
            accepted: AcceptedLibrarySessionInfo {
                protocol: LIBRARY_PROTOCOL_VERSION,
                abi,
                capability_identity,
                granted_capabilities,
                features: BTreeSet::from([LIBRARY_HOST_CALL_FEATURE.to_string()]),
            },
        })
    }

    pub const fn state(&self) -> SessionState {
        self.session.state()
    }

    pub fn accept_hello(
        &mut self,
        hello: &WorkerHello,
    ) -> Result<&AcceptedLibrarySessionInfo, LibraryHostError> {
        self.session.accept_hello(hello)?;
        Ok(&self.accepted)
    }

    pub fn accepted_info(&self) -> Option<&AcceptedLibrarySessionInfo> {
        (self.session.state() == SessionState::Accepted).then_some(&self.accepted)
    }

    pub fn authorize_host_call(
        &self,
        call: &HostCall,
    ) -> Result<&CapabilityGrant, LibraryHostError> {
        self.session.authorize_host_call(call)
    }

    pub fn validate_reply(
        &self,
        reply: &PackageReply,
        maximum_response_bytes: usize,
    ) -> Result<(), LibraryHostError> {
        self.session.validate_reply(reply, maximum_response_bytes)
    }

    pub fn close(&mut self) {
        self.session.close();
    }
}

fn validate_capability_identity(value: &str) -> Result<(), LibraryHostError> {
    let suffix = value.strip_prefix("session:").unwrap_or_default();
    if suffix.is_empty()
        || value.len() > MAX_CAPABILITY_IDENTITY_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(LibraryHostError::InvalidIdentity(
            "session capability identity must be a bounded opaque session:* value".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library_host::{PackageIdentity, RuntimeIdentity, SdkIdentity, LIBRARY_ABI_VERSION};

    fn expected() -> ExpectedWorkerIdentity {
        ExpectedWorkerIdentity {
            package: PackageIdentity {
                name: "advancenet".into(),
                version: "2.0.0".into(),
                artifact_sha256: "a".repeat(64),
            },
            sdk: SdkIdentity {
                language: "bun".into(),
                name: "@rbe/sdk".into(),
                version: "0.1.9".into(),
            },
            runtime: RuntimeIdentity {
                kind: "bun".into(),
                version: "1.3.7".into(),
            },
            abi: LIBRARY_ABI_VERSION,
        }
    }

    fn hello() -> WorkerHello {
        let expected = expected();
        WorkerHello {
            protocol: LIBRARY_PROTOCOL_VERSION,
            package: expected.package,
            sdk: expected.sdk,
            runtime: expected.runtime,
            abi_min: LIBRARY_ABI_VERSION,
            abi_max: LIBRARY_ABI_VERSION,
        }
    }

    #[test]
    fn accepted_metadata_is_derived_from_the_enforced_grants() {
        let grants = [
            CapabilityGrant::new("net:http", "net:http", ["request".to_string()], 1024, 4096)
                .unwrap(),
            CapabilityGrant::new(
                "storage",
                "storage:project",
                ["read".to_string()],
                1024,
                4096,
            )
            .unwrap(),
        ];
        let mut binding =
            LibrarySessionBinding::new(expected(), grants, "session:test-opaque").unwrap();
        assert!(binding.accepted_info().is_none());

        let info = binding.accept_hello(&hello()).unwrap();
        assert_eq!(info.protocol, LIBRARY_PROTOCOL_VERSION);
        assert_eq!(info.abi, LIBRARY_ABI_VERSION);
        assert_eq!(
            info.granted_capabilities,
            BTreeSet::from(["net:http".to_string(), "storage".to_string()])
        );
        assert_eq!(
            info.features,
            BTreeSet::from([LIBRARY_HOST_CALL_FEATURE.to_string()])
        );

        let wire = info.to_value();
        assert_eq!(wire["type"], "library.accept");
        assert_eq!(wire["capabilityIdentity"], "session:test-opaque");
    }

    #[test]
    fn empty_grant_set_stays_empty_and_does_not_gain_authority() {
        let mut binding = LibrarySessionBinding::new(
            expected(),
            std::iter::empty::<CapabilityGrant>(),
            "session:no-grants",
        )
        .unwrap();
        let info = binding.accept_hello(&hello()).unwrap();
        assert!(info.granted_capabilities.is_empty());

        assert!(matches!(
            binding.authorize_host_call(&HostCall {
                call_id: 1,
                capability: "net:http".into(),
                target: "net:http".into(),
                operation: "request".into(),
                payload: Vec::new(),
            }),
            Err(LibraryHostError::CapabilityDenied { .. })
        ));
    }

    #[test]
    fn malformed_session_identity_is_rejected_before_handshake() {
        assert!(matches!(
            LibrarySessionBinding::new(
                expected(),
                std::iter::empty::<CapabilityGrant>(),
                "not-session-scoped"
            ),
            Err(LibraryHostError::InvalidIdentity(_))
        ));
    }
}
