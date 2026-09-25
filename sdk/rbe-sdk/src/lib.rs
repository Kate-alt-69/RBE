//! Stable SDK surface for external RBE native libraries.
//!
//! External libraries compile against this crate, but they do not link against
//! private `backend` Rust types. At runtime a library receives an implementation
//! of [`HostBridge`] from the RBE library host and all privileged work crosses
//! that versioned bridge.

#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;

/// First public RBE native-library ABI.
pub const LIBRARY_ABI_VERSION: u32 = 1;
/// Version of the SDK crate used to compile the external library.
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// ABI range accepted by a library package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbiRange {
    pub min: u32,
    pub max: u32,
}

impl AbiRange {
    pub const fn new(min: u32, max: u32) -> Self {
        Self { min, max }
    }

    pub const fn exact(version: u32) -> Self {
        Self::new(version, version)
    }

    pub const fn contains(self, version: u32) -> bool {
        version >= self.min && version <= self.max
    }

    pub const fn is_valid(self) -> bool {
        self.min > 0 && self.min <= self.max
    }
}

/// ABI range implemented by this SDK release.
pub const SUPPORTED_ABI: AbiRange = AbiRange::exact(LIBRARY_ABI_VERSION);

/// Canonical capability IDs understood by the RBE library host.
pub mod capability {
    pub const NET_HTTP: &str = "net:http";
    pub const NET_COOKIES: &str = "net:cookies";
    pub const NET_HEADERS: &str = "net:headers";
    pub const NET_URL: &str = "net:url";
    pub const NET_DNS: &str = "net:dns";
    pub const NET_IP: &str = "net:ip";
    pub const NET_TCP: &str = "net:tcp";
    pub const NET_UDP: &str = "net:udp";
    pub const NET_QUIC: &str = "net:quic";
    pub const NET_WEBSOCKET: &str = "net:websocket";
    pub const NET_WEBTRANSPORT: &str = "net:webtransport";
    pub const NET_P2P: &str = "net:p2p";
    pub const NET_MASK: &str = "net:mask";

    pub const ROUTER_READ: &str = "router:read";
    pub const ROUTER_REGISTER: &str = "router:register";
    pub const STORAGE: &str = "storage";
    pub const CRYPTO: &str = "crypto";
}

/// A single privileged request made by an external library to RBE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCall<'a> {
    pub capability: &'a str,
    pub target: &'a str,
    pub operation: &'a str,
    pub payload: &'a [u8],
}

impl<'a> HostCall<'a> {
    pub const fn new(
        capability: &'a str,
        target: &'a str,
        operation: &'a str,
        payload: &'a [u8],
    ) -> Self {
        Self {
            capability,
            target,
            operation,
            payload,
        }
    }
}

/// Owned host request for advanced SDK composition, queues, and batching.
///
/// The SDK deliberately does not keep a hardcoded allowlist here. Packages may
/// target future or package-specific host surfaces without waiting for a new
/// convenience wrapper release. RBE remains the authority and can reject any
/// request not granted to the package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRequest {
    pub capability: String,
    pub target: String,
    pub operation: String,
    pub payload: Vec<u8>,
}

impl HostRequest {
    pub fn new(
        capability: impl Into<String>,
        target: impl Into<String>,
        operation: impl Into<String>,
    ) -> Self {
        Self {
            capability: capability.into(),
            target: target.into(),
            operation: operation.into(),
            payload: Vec::new(),
        }
    }

    pub fn payload(mut self, payload: impl Into<Vec<u8>>) -> Self {
        self.payload = payload.into();
        self
    }

    pub fn as_call(&self) -> HostCall<'_> {
        HostCall::new(
            &self.capability,
            &self.target,
            &self.operation,
            &self.payload,
        )
    }
}

/// Opaque payload returned by the RBE host.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostReply {
    pub payload: Vec<u8>,
}

/// Error returned by the host bridge. Codes are owned by RBE and are kept as
/// strings so newer hosts can add errors without forcing an SDK enum bump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub code: String,
    pub message: String,
}

impl HostError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for HostError {}

/// Runtime bridge supplied by RBE to a native library worker.
///
/// Libraries never receive raw router pointers, sockets, database handles, or
/// private backend structures. All privileged operations are explicit calls
/// through this boundary and are capability checked by the host.
pub trait HostBridge: Send + Sync {
    fn call(&self, call: HostCall<'_>) -> Result<HostReply, HostError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdkError {
    InvalidLibraryName(String),
    InvalidNetSublibrary(String),
    InvalidAbiRange(AbiRange),
    Host(HostError),
}

impl fmt::Display for SdkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLibraryName(name) => {
                write!(formatter, "invalid RBE library name {name:?}")
            }
            Self::InvalidNetSublibrary(name) => {
                write!(formatter, "invalid net sub-library name {name:?}")
            }
            Self::InvalidAbiRange(range) => {
                write!(
                    formatter,
                    "invalid RBE ABI range {}..={}",
                    range.min, range.max
                )
            }
            Self::Host(error) => error.fmt(formatter),
        }
    }
}

impl Error for SdkError {}

impl From<HostError> for SdkError {
    fn from(value: HostError) -> Self {
        Self::Host(value)
    }
}

/// Descriptor embedded by an external library package/worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryDescriptor<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub abi: AbiRange,
}

impl<'a> LibraryDescriptor<'a> {
    pub fn validate(self) -> Result<Self, SdkError> {
        if !valid_component(self.name) {
            return Err(SdkError::InvalidLibraryName(self.name.to_string()));
        }
        if !self.abi.is_valid() {
            return Err(SdkError::InvalidAbiRange(self.abi));
        }
        Ok(self)
    }

    pub const fn supports_host_abi(self, host_abi: u32) -> bool {
        self.abi.contains(host_abi)
    }
}

/// Entry point exposed to library code.
///
/// Common helpers (`net`, `router`, `storage`, `crypto`) are the easy path. The
/// generic `capability` and `advanced` surfaces intentionally remain open-ended
/// so advanced packages can compose RBE features without the SDK policing their
/// application design.
#[derive(Clone, Copy)]
pub struct RbeSdk<'a> {
    bridge: &'a dyn HostBridge,
}

impl<'a> RbeSdk<'a> {
    pub const fn new(bridge: &'a dyn HostBridge) -> Self {
        Self { bridge }
    }

    pub const fn net(self) -> Net<'a> {
        Net {
            bridge: self.bridge,
        }
    }

    pub const fn router(self) -> Router<'a> {
        Router {
            bridge: self.bridge,
        }
    }

    pub const fn storage(self) -> Storage<'a> {
        Storage {
            bridge: self.bridge,
        }
    }

    pub const fn crypto(self) -> Crypto<'a> {
        Crypto {
            bridge: self.bridge,
        }
    }

    /// Create a generic capability client. The capability is also used as the
    /// default target, which matches RBE's built-in namespace convention.
    pub fn capability(self, capability: impl Into<String>) -> CapabilityClient<'a> {
        let capability = capability.into();
        CapabilityClient {
            bridge: self.bridge,
            target: capability.clone(),
            capability,
        }
    }

    /// Create a generic capability client with an independent target identity.
    pub fn capability_target(
        self,
        capability: impl Into<String>,
        target: impl Into<String>,
    ) -> CapabilityClient<'a> {
        CapabilityClient {
            bridge: self.bridge,
            capability: capability.into(),
            target: target.into(),
        }
    }

    pub const fn advanced(self) -> AdvancedSdk<'a> {
        AdvancedSdk {
            bridge: self.bridge,
        }
    }

    /// Escape hatch for packages that need to build their own SDK abstraction.
    /// The bridge is still capability checked by RBE.
    pub const fn host_bridge(self) -> &'a dyn HostBridge {
        self.bridge
    }

    pub fn call(self, call: HostCall<'_>) -> Result<HostReply, SdkError> {
        self.bridge.call(call).map_err(Into::into)
    }
}

/// Open-ended advanced SDK surface.
///
/// This intentionally supplies mechanism rather than policy: package authors
/// choose how to compose calls, retries, queues, caches, and higher-level APIs.
/// Host authority, ABI validation, and capability grants remain RBE-owned.
#[derive(Clone, Copy)]
pub struct AdvancedSdk<'a> {
    bridge: &'a dyn HostBridge,
}

impl<'a> AdvancedSdk<'a> {
    pub fn capability(self, capability: impl Into<String>) -> CapabilityClient<'a> {
        let capability = capability.into();
        CapabilityClient {
            bridge: self.bridge,
            target: capability.clone(),
            capability,
        }
    }

    pub fn target(
        self,
        capability: impl Into<String>,
        target: impl Into<String>,
    ) -> CapabilityClient<'a> {
        CapabilityClient {
            bridge: self.bridge,
            capability: capability.into(),
            target: target.into(),
        }
    }

    pub fn request(
        self,
        capability: impl Into<String>,
        target: impl Into<String>,
        operation: impl Into<String>,
    ) -> HostRequest {
        HostRequest::new(capability, target, operation)
    }

    pub fn send(self, request: &HostRequest) -> Result<HostReply, SdkError> {
        self.bridge.call(request.as_call()).map_err(Into::into)
    }

    /// Execute every request independently and preserve each result. One failed
    /// operation does not hide the results of the others; package code decides
    /// whether that means retry, fallback, rollback, or ignore.
    pub fn batch(self, requests: &[HostRequest]) -> Vec<Result<HostReply, SdkError>> {
        requests.iter().map(|request| self.send(request)).collect()
    }

    pub const fn host_bridge(self) -> &'a dyn HostBridge {
        self.bridge
    }
}

#[derive(Clone, Copy)]
pub struct Net<'a> {
    bridge: &'a dyn HostBridge,
}

impl<'a> Net<'a> {
    pub fn sublibrary(self, name: &str) -> Result<NetLibrary<'a>, SdkError> {
        if !valid_component(name) {
            return Err(SdkError::InvalidNetSublibrary(name.to_string()));
        }
        Ok(CapabilityClient {
            bridge: self.bridge,
            capability: format!("net:{name}"),
            target: format!("net:{name}"),
        })
    }

    pub fn http(self) -> NetLibrary<'a> {
        self.known(capability::NET_HTTP)
    }

    pub fn p2p(self) -> NetLibrary<'a> {
        self.known(capability::NET_P2P)
    }

    pub fn mask(self) -> NetLibrary<'a> {
        self.known(capability::NET_MASK)
    }

    fn known(self, target: &'static str) -> NetLibrary<'a> {
        CapabilityClient {
            bridge: self.bridge,
            capability: target.to_string(),
            target: target.to_string(),
        }
    }
}

/// Generic reusable client for one capability/target pair.
pub struct CapabilityClient<'a> {
    bridge: &'a dyn HostBridge,
    capability: String,
    target: String,
}

impl CapabilityClient<'_> {
    pub fn capability_id(&self) -> &str {
        &self.capability
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn retarget(mut self, target: impl Into<String>) -> Self {
        self.target = target.into();
        self
    }

    pub fn request(
        &self,
        operation: impl Into<String>,
        payload: impl Into<Vec<u8>>,
    ) -> HostRequest {
        HostRequest {
            capability: self.capability.clone(),
            target: self.target.clone(),
            operation: operation.into(),
            payload: payload.into(),
        }
    }

    pub fn call(&self, operation: &str, payload: &[u8]) -> Result<HostReply, SdkError> {
        self.bridge
            .call(HostCall::new(
                &self.capability,
                &self.target,
                operation,
                payload,
            ))
            .map_err(Into::into)
    }
}

pub type NetLibrary<'a> = CapabilityClient<'a>;

#[derive(Clone, Copy)]
pub struct Router<'a> {
    bridge: &'a dyn HostBridge,
}

impl Router<'_> {
    pub fn inspect(&self, operation: &str, payload: &[u8]) -> Result<HostReply, SdkError> {
        self.bridge
            .call(HostCall::new(
                capability::ROUTER_READ,
                "router",
                operation,
                payload,
            ))
            .map_err(Into::into)
    }

    pub fn register(&self, operation: &str, payload: &[u8]) -> Result<HostReply, SdkError> {
        self.bridge
            .call(HostCall::new(
                capability::ROUTER_REGISTER,
                "router",
                operation,
                payload,
            ))
            .map_err(Into::into)
    }
}

#[derive(Clone, Copy)]
pub struct Storage<'a> {
    bridge: &'a dyn HostBridge,
}

impl Storage<'_> {
    pub fn call(&self, operation: &str, payload: &[u8]) -> Result<HostReply, SdkError> {
        self.bridge
            .call(HostCall::new(
                capability::STORAGE,
                "storage",
                operation,
                payload,
            ))
            .map_err(Into::into)
    }
}

#[derive(Clone, Copy)]
pub struct Crypto<'a> {
    bridge: &'a dyn HostBridge,
}

impl Crypto<'_> {
    pub fn call(&self, operation: &str, payload: &[u8]) -> Result<HostReply, SdkError> {
        self.bridge
            .call(HostCall::new(
                capability::CRYPTO,
                "crypto",
                operation,
                payload,
            ))
            .map_err(Into::into)
    }
}

fn valid_component(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type RecordedCall = (String, String, String, Vec<u8>);

    #[derive(Default)]
    struct RecordingBridge {
        calls: Mutex<Vec<RecordedCall>>,
    }

    impl HostBridge for RecordingBridge {
        fn call(&self, call: HostCall<'_>) -> Result<HostReply, HostError> {
            self.calls.lock().unwrap().push((
                call.capability.to_string(),
                call.target.to_string(),
                call.operation.to_string(),
                call.payload.to_vec(),
            ));
            Ok(HostReply {
                payload: b"ok".to_vec(),
            })
        }
    }

    #[test]
    fn descriptor_rejects_invalid_names_and_abi_ranges() {
        assert!(LibraryDescriptor {
            name: "advancenet",
            version: "1.0.0",
            abi: AbiRange::exact(1),
        }
        .validate()
        .is_ok());
        assert!(LibraryDescriptor {
            name: "../evil",
            version: "1.0.0",
            abi: AbiRange::exact(1),
        }
        .validate()
        .is_err());
        assert!(LibraryDescriptor {
            name: "good",
            version: "1.0.0",
            abi: AbiRange::new(2, 1),
        }
        .validate()
        .is_err());
    }

    #[test]
    fn higher_level_library_can_call_builtin_net_through_host_bridge() {
        let bridge = RecordingBridge::default();
        let sdk = RbeSdk::new(&bridge);
        let reply = sdk
            .net()
            .http()
            .call("request", br#"{\"url\":\"https://example.com\"}"#)
            .unwrap();
        assert_eq!(reply.payload, b"ok");

        let calls = bridge.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, capability::NET_HTTP);
        assert_eq!(calls[0].1, capability::NET_HTTP);
        assert_eq!(calls[0].2, "request");
    }

    #[test]
    fn router_registration_is_a_separate_capability_from_router_read() {
        let bridge = RecordingBridge::default();
        let sdk = RbeSdk::new(&bridge);
        sdk.router().inspect("snapshot", b"{}").unwrap();
        sdk.router().register("middleware", b"{}").unwrap();

        let calls = bridge.calls.lock().unwrap();
        assert_eq!(calls[0].0, capability::ROUTER_READ);
        assert_eq!(calls[1].0, capability::ROUTER_REGISTER);
    }

    #[test]
    fn advanced_surface_supports_custom_targets_and_independent_batch_results() {
        let bridge = RecordingBridge::default();
        let sdk = RbeSdk::new(&bridge);

        let custom = sdk
            .capability_target("video:encode", "encoder:primary")
            .retarget("encoder:gpu0");
        assert_eq!(custom.capability_id(), "video:encode");
        assert_eq!(custom.target(), "encoder:gpu0");
        custom.call("submit", b"frame").unwrap();

        let advanced = sdk.advanced();
        let requests = [
            advanced
                .request("net:quic", "peer:alpha", "connect")
                .payload(b"one".to_vec()),
            advanced
                .request("custom:future", "thing:42", "do-work")
                .payload(b"two".to_vec()),
        ];
        let results = advanced.batch(&requests);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(Result::is_ok));

        let calls = bridge.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].0, "video:encode");
        assert_eq!(calls[0].1, "encoder:gpu0");
        assert_eq!(calls[1].0, "net:quic");
        assert_eq!(calls[1].1, "peer:alpha");
        assert_eq!(calls[2].0, "custom:future");
        assert_eq!(calls[2].1, "thing:42");
    }
}
