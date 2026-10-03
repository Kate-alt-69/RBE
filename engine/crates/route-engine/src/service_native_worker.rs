//! Fail-closed verification and execution for native Service worker bootstrap frames.
//!
//! Raw parent metadata is never executable authority. The verifier first reopens
//! the exact content-addressed Service `.bin`, validates its integrity/identity,
//! checks the current host target and every dispatch offset, and produces a
//! `VerifiedNativeServiceWorkerBootstrap`. Only that verified type may be mapped
//! executable by `NativeServiceExecutor`.
//!
//! The executor is intentionally limited to the native ABI RELC emits today:
//! zero-argument Boolean leaf Service exports. Unsupported lifecycle/argument
//! semantics fail closed and remain on the evaluator path until native lowering
//! explicitly gains parity.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use serde_json::Value;
use service_runtime::{
    ServiceExecutionError, ServiceExecutionFuture, ServiceExecutor, ServiceLifecycle,
    ServiceLifecycleFuture,
};

use crate::oid_link::{REL_OID_END, REL_OID_START};
use crate::service_bin::decode_cached_service_bin;
use crate::service_native_build::{
    NativeServiceWorkerBootstrap, NativeServiceWorkerBootstrapEntry,
    NATIVE_SERVICE_WORKER_BOOTSTRAP_PROTOCOL,
};
use crate::service_oid::OidTarget;
use crate::source_registry::SourceId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedNativeServiceWorkerEntry {
    pub oid: u16,
    pub entry_offset: usize,
}

/// Bootstrap state safe for native loading/dispatch.
///
/// The original cross-process `u64` offsets have already been checked against
/// the local pointer width and the exact decoded payload. Downstream code should
/// use this type instead of the raw JSON frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedNativeServiceWorkerBootstrap {
    runtime_image_id: String,
    source_id: SourceId,
    oid_index_generation: u64,
    plan_hash: String,
    assembly_hash: String,
    target_fingerprint: String,
    bin_path: PathBuf,
    payload: Vec<u8>,
    exports: BTreeMap<String, VerifiedNativeServiceWorkerEntry>,
    lifecycle: BTreeMap<String, VerifiedNativeServiceWorkerEntry>,
}

impl VerifiedNativeServiceWorkerBootstrap {
    pub fn runtime_image_id(&self) -> &str {
        &self.runtime_image_id
    }

    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    pub fn oid_index_generation(&self) -> u64 {
        self.oid_index_generation
    }

    pub fn plan_hash(&self) -> &str {
        &self.plan_hash
    }

    pub fn assembly_hash(&self) -> &str {
        &self.assembly_hash
    }

    pub fn target_fingerprint(&self) -> &str {
        &self.target_fingerprint
    }

    pub fn bin_path(&self) -> &std::path::Path {
        &self.bin_path
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn export(&self, name: &str) -> Option<&VerifiedNativeServiceWorkerEntry> {
        self.exports.get(name)
    }

    pub fn lifecycle(&self, verb: &str) -> Option<&VerifiedNativeServiceWorkerEntry> {
        self.lifecycle.get(verb)
    }

    pub fn exports(&self) -> &BTreeMap<String, VerifiedNativeServiceWorkerEntry> {
        &self.exports
    }

    pub fn lifecycle_entries(&self) -> &BTreeMap<String, VerifiedNativeServiceWorkerEntry> {
        &self.lifecycle
    }
}

/// Verify one parent-provided native Service bootstrap against identities the
/// worker already knows from its supervised launch record.
///
/// This deliberately does not map executable memory. A successful result proves
/// that the frame names the expected Runtime Image/Service, targets this exact
/// host ABI, the cached `.bin` is canonical/content-valid, and every advertised
/// export/lifecycle entry is a unique REL OID with an in-bounds local offset.
pub fn verify_native_service_worker_bootstrap(
    bootstrap: NativeServiceWorkerBootstrap,
    expected_runtime_image_id: &str,
    expected_source_id: &SourceId,
) -> Result<VerifiedNativeServiceWorkerBootstrap, NativeServiceWorkerBootstrapError> {
    validate_identity("expected Runtime Image", expected_runtime_image_id)?;

    if bootstrap.protocol != NATIVE_SERVICE_WORKER_BOOTSTRAP_PROTOCOL {
        return Err(invalid(format!(
            "unsupported native Service worker bootstrap protocol {:?}; expected {:?}",
            bootstrap.protocol, NATIVE_SERVICE_WORKER_BOOTSTRAP_PROTOCOL
        )));
    }
    if bootstrap.runtime_image_id != expected_runtime_image_id {
        return Err(invalid(format!(
            "native Service worker bootstrap Runtime Image {:?} does not match supervised Runtime Image {:?}",
            bootstrap.runtime_image_id, expected_runtime_image_id
        )));
    }
    if bootstrap.source_id != expected_source_id.as_str() {
        return Err(invalid(format!(
            "native Service worker bootstrap source {:?} does not match supervised Service {}",
            bootstrap.source_id, expected_source_id
        )));
    }

    validate_sha256("service plan", &bootstrap.plan_hash)?;
    validate_sha256("service assembly", &bootstrap.assembly_hash)?;
    validate_identity("native target fingerprint", &bootstrap.target_fingerprint)?;
    let current_target = OidTarget::current().label();
    if bootstrap.target_fingerprint != current_target {
        return Err(invalid(format!(
            "native Service worker bootstrap target {:?} does not match this worker host {:?}",
            bootstrap.target_fingerprint, current_target
        )));
    }

    if !bootstrap.bin_path.is_absolute() {
        return Err(invalid(format!(
            "native Service worker bootstrap bin path {} is not absolute",
            bootstrap.bin_path.display()
        )));
    }
    let canonical_bin_path = std::fs::canonicalize(&bootstrap.bin_path).map_err(|error| {
        invalid(format!(
            "native Service worker bootstrap bin {} cannot be canonicalized: {error}",
            bootstrap.bin_path.display()
        ))
    })?;
    if canonical_bin_path != bootstrap.bin_path {
        return Err(invalid(format!(
            "native Service worker bootstrap bin {} is not the canonical path {}",
            bootstrap.bin_path.display(),
            canonical_bin_path.display()
        )));
    }
    if !canonical_bin_path.is_file() {
        return Err(invalid(format!(
            "native Service worker bootstrap bin {} is not a file",
            canonical_bin_path.display()
        )));
    }

    let expected_file_name = format!("{}.bin", bootstrap.assembly_hash);
    if canonical_bin_path
        .file_name()
        .and_then(|name| name.to_str())
        != Some(expected_file_name.as_str())
    {
        return Err(invalid(format!(
            "native Service worker bootstrap bin {} is not content-addressed by assembly {}",
            canonical_bin_path.display(),
            bootstrap.assembly_hash
        )));
    }

    let encoded = std::fs::read(&canonical_bin_path).map_err(|error| {
        invalid(format!(
            "native Service worker bootstrap bin {} cannot be read: {error}",
            canonical_bin_path.display()
        ))
    })?;
    let decoded = decode_cached_service_bin(&encoded).map_err(|error| {
        invalid(format!(
            "native Service worker bootstrap bin {} failed integrity decoding: {error}",
            canonical_bin_path.display()
        ))
    })?;

    if decoded.plan_hash != bootstrap.plan_hash {
        return Err(invalid(format!(
            "native Service worker bootstrap plan hash {} does not match decoded bin {}",
            bootstrap.plan_hash, decoded.plan_hash
        )));
    }
    if decoded.assembly_hash != bootstrap.assembly_hash {
        return Err(invalid(format!(
            "native Service worker bootstrap assembly hash {} does not match decoded bin {}",
            bootstrap.assembly_hash, decoded.assembly_hash
        )));
    }
    if decoded.target_fingerprint != bootstrap.target_fingerprint {
        return Err(invalid(format!(
            "native Service worker bootstrap target {:?} does not match decoded bin {:?}",
            bootstrap.target_fingerprint, decoded.target_fingerprint
        )));
    }

    let mut seen_oids = BTreeSet::new();
    let mut seen_offsets = BTreeSet::new();
    let exports = verify_entries(
        expected_source_id,
        "export",
        &bootstrap.exports,
        decoded.payload.len(),
        &mut seen_oids,
        &mut seen_offsets,
    )?;
    let lifecycle = verify_entries(
        expected_source_id,
        "lifecycle",
        &bootstrap.lifecycle,
        decoded.payload.len(),
        &mut seen_oids,
        &mut seen_offsets,
    )?;
    if exports.is_empty() && lifecycle.is_empty() {
        return Err(invalid(format!(
            "native Service worker bootstrap for {expected_source_id} has no executable entries"
        )));
    }

    Ok(VerifiedNativeServiceWorkerBootstrap {
        runtime_image_id: bootstrap.runtime_image_id,
        source_id: expected_source_id.clone(),
        oid_index_generation: bootstrap.oid_index_generation,
        plan_hash: bootstrap.plan_hash,
        assembly_hash: bootstrap.assembly_hash,
        target_fingerprint: bootstrap.target_fingerprint,
        bin_path: canonical_bin_path,
        payload: decoded.payload,
        exports,
        lifecycle,
    })
}

fn verify_entries(
    source_id: &SourceId,
    label: &str,
    entries: &BTreeMap<String, NativeServiceWorkerBootstrapEntry>,
    payload_len: usize,
    seen_oids: &mut BTreeSet<u16>,
    seen_offsets: &mut BTreeSet<usize>,
) -> Result<BTreeMap<String, VerifiedNativeServiceWorkerEntry>, NativeServiceWorkerBootstrapError> {
    let mut verified = BTreeMap::new();
    for (name, entry) in entries {
        validate_entry_name(label, name)?;
        if !(REL_OID_START..=REL_OID_END).contains(&entry.oid) {
            return Err(invalid(format!(
                "native Service {source_id} {label} {name:?} uses non-REL OID {}",
                entry.oid
            )));
        }
        if !seen_oids.insert(entry.oid) {
            return Err(invalid(format!(
                "native Service {source_id} bootstrap reuses OID {} across dispatch entries",
                entry.oid
            )));
        }

        let entry_offset = usize::try_from(entry.entry_offset).map_err(|_| {
            invalid(format!(
                "native Service {source_id} {label} {name:?} offset {} does not fit this worker address width",
                entry.entry_offset
            ))
        })?;
        if entry_offset >= payload_len {
            return Err(invalid(format!(
                "native Service {source_id} {label} {name:?} offset {entry_offset} is outside decoded payload length {payload_len}"
            )));
        }
        if !seen_offsets.insert(entry_offset) {
            return Err(invalid(format!(
                "native Service {source_id} bootstrap reuses payload offset {entry_offset} across dispatch entries"
            )));
        }

        verified.insert(
            name.clone(),
            VerifiedNativeServiceWorkerEntry {
                oid: entry.oid,
                entry_offset,
            },
        );
    }
    Ok(verified)
}

fn validate_entry_name(label: &str, name: &str) -> Result<(), NativeServiceWorkerBootstrapError> {
    if name.is_empty()
        || name.trim() != name
        || name.len() > 256
        || name.chars().any(char::is_control)
    {
        Err(invalid(format!(
            "invalid native Service worker {label} name {name:?}"
        )))
    } else {
        Ok(())
    }
}

fn validate_identity(label: &str, value: &str) -> Result<(), NativeServiceWorkerBootstrapError> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        Err(invalid(format!("invalid {label} {value:?}")))
    } else {
        Ok(())
    }
}

fn validate_sha256(label: &str, value: &str) -> Result<(), NativeServiceWorkerBootstrapError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(invalid(format!("invalid {label} SHA-256 {value:?}")))
    }
}

fn invalid(message: String) -> NativeServiceWorkerBootstrapError {
    NativeServiceWorkerBootstrapError { message }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeServiceWorkerBootstrapError {
    message: String,
}

impl NativeServiceWorkerBootstrapError {
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for NativeServiceWorkerBootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for NativeServiceWorkerBootstrapError {}

/// Native Service executor for the exact Phase-2 ABI currently emitted by RELC.
///
/// Construction consumes only a verified bootstrap and maps its immutable payload
/// W->X. Public calls are zero-argument Boolean leaves; any other ABI shape fails
/// closed so unsupported Services stay on the evaluator fallback path.
#[derive(Debug)]
pub struct NativeServiceExecutor {
    bootstrap: VerifiedNativeServiceWorkerBootstrap,
    image: NativeExecutableImage,
}

impl NativeServiceExecutor {
    pub fn new(
        bootstrap: VerifiedNativeServiceWorkerBootstrap,
    ) -> Result<Self, NativeServiceExecutorError> {
        if bootstrap.exports().is_empty() {
            return Err(executor_invalid(
                "native Service executor requires at least one public export".into(),
            ));
        }
        if !bootstrap.lifecycle_entries().is_empty() {
            return Err(executor_invalid(
                "native Service lifecycle ABI is not enabled yet; keep this Service on evaluator fallback"
                    .into(),
            ));
        }
        let image = NativeExecutableImage::new(bootstrap.payload())?;
        Ok(Self { bootstrap, image })
    }

    pub fn bootstrap(&self) -> &VerifiedNativeServiceWorkerBootstrap {
        &self.bootstrap
    }

    pub const fn platform_supported() -> bool {
        cfg!(all(
            target_arch = "x86_64",
            any(unix, windows)
        ))
    }

    fn call_export(&self, function: &str) -> Result<Value, ServiceExecutionError> {
        let entry = self.bootstrap.export(function).ok_or_else(|| {
            ServiceExecutionError::new("SVC4101", format!("unknown native export {function:?}"))
        })?;
        self.image
            .call_bool(entry.entry_offset)
            .map(Value::Bool)
            .map_err(|error| ServiceExecutionError::new("SVC4100", error.to_string()))
    }
}

impl ServiceExecutor for NativeServiceExecutor {
    fn call<'a>(&'a self, function: &'a str, args: Vec<Value>) -> ServiceExecutionFuture<'a> {
        Box::pin(async move {
            if !args.is_empty() {
                return Err(ServiceExecutionError::new(
                    "SVC4100",
                    format!(
                        "native export {function:?} uses the zero-argument ABI, received {} argument(s)",
                        args.len()
                    ),
                ));
            }
            self.call_export(function)
        })
    }

    fn lifecycle<'a>(
        &'a self,
        phase: ServiceLifecycle,
        _argument: Value,
    ) -> ServiceLifecycleFuture<'a> {
        Box::pin(async move {
            if self.bootstrap.lifecycle(phase.as_str()).is_some() {
                return Err(ServiceExecutionError::new(
                    "SVC4100",
                    format!(
                        "native lifecycle {:?} is present but the lifecycle native ABI is not enabled",
                        phase.as_str()
                    ),
                ));
            }
            Ok(None)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeServiceExecutorError {
    message: String,
}

impl NativeServiceExecutorError {
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for NativeServiceExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for NativeServiceExecutorError {}

fn executor_invalid(message: String) -> NativeServiceExecutorError {
    NativeServiceExecutorError { message }
}

#[derive(Debug)]
struct NativeExecutableImage {
    ptr: *mut u8,
    len: usize,
}

// The allocation becomes immutable RX before construction returns. Dispatch only
// reads it and invokes stable offsets, so sharing the mapping between worker RPC
// tasks is safe.
unsafe impl Send for NativeExecutableImage {}
unsafe impl Sync for NativeExecutableImage {}

impl NativeExecutableImage {
    fn new(payload: &[u8]) -> Result<Self, NativeServiceExecutorError> {
        if payload.is_empty() {
            return Err(executor_invalid(
                "native Service executable payload is empty".into(),
            ));
        }
        if !NativeServiceExecutor::platform_supported() {
            return Err(executor_invalid(format!(
                "native Service execution is not enabled for host {}/{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            )));
        }
        platform_allocate_executable(payload)
    }

    fn call_bool(&self, offset: usize) -> Result<bool, NativeServiceExecutorError> {
        if offset >= self.len {
            return Err(executor_invalid(format!(
                "native Service entry offset {offset} is outside executable image length {}",
                self.len
            )));
        }
        let address = unsafe { self.ptr.add(offset) }.cast_const();
        let function: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(address) };
        let value = unsafe { function() };
        match value {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(executor_invalid(format!(
                "native Boolean ABI returned non-canonical value {other}"
            ))),
        }
    }
}

impl Drop for NativeExecutableImage {
    fn drop(&mut self) {
        platform_free_executable(self.ptr, self.len);
    }
}

#[cfg(all(unix, target_arch = "x86_64"))]
fn platform_allocate_executable(
    payload: &[u8],
) -> Result<NativeExecutableImage, NativeServiceExecutorError> {
    use std::ffi::c_void;

    const PROT_READ: i32 = 0x1;
    const PROT_WRITE: i32 = 0x2;
    const PROT_EXEC: i32 = 0x4;
    const MAP_PRIVATE: i32 = 0x2;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const MAP_ANONYMOUS: i32 = 0x20;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const MAP_ANONYMOUS: i32 = 0x1000;

    unsafe extern "C" {
        fn mmap(
            address: *mut c_void,
            length: usize,
            protection: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut c_void;
        fn mprotect(address: *mut c_void, length: usize, protection: i32) -> i32;
        fn munmap(address: *mut c_void, length: usize) -> i32;
    }

    let mapped = unsafe {
        mmap(
            std::ptr::null_mut(),
            payload.len(),
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mapped as isize == -1 {
        return Err(executor_invalid(format!(
            "map native Service executable image: {}",
            std::io::Error::last_os_error()
        )));
    }

    unsafe {
        std::ptr::copy_nonoverlapping(payload.as_ptr(), mapped.cast::<u8>(), payload.len());
    }
    if unsafe { mprotect(mapped, payload.len(), PROT_READ | PROT_EXEC) } != 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            let _ = munmap(mapped, payload.len());
        }
        return Err(executor_invalid(format!(
            "protect native Service executable image RX: {error}"
        )));
    }

    Ok(NativeExecutableImage {
        ptr: mapped.cast(),
        len: payload.len(),
    })
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn platform_allocate_executable(
    payload: &[u8],
) -> Result<NativeExecutableImage, NativeServiceExecutorError> {
    use std::ffi::c_void;

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const PAGE_READWRITE: u32 = 0x04;
    const PAGE_EXECUTE_READ: u32 = 0x20;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualAlloc(
            address: *mut c_void,
            size: usize,
            allocation_type: u32,
            protection: u32,
        ) -> *mut c_void;
        fn VirtualProtect(
            address: *mut c_void,
            size: usize,
            new_protection: u32,
            old_protection: *mut u32,
        ) -> i32;
        fn FlushInstructionCache(
            process: *mut c_void,
            base_address: *const c_void,
            size: usize,
        ) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
    }

    let mapped = unsafe {
        VirtualAlloc(
            std::ptr::null_mut(),
            payload.len(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if mapped.is_null() {
        return Err(executor_invalid(format!(
            "allocate native Service executable image: {}",
            std::io::Error::last_os_error()
        )));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(payload.as_ptr(), mapped.cast::<u8>(), payload.len());
    }
    let mut old_protection = 0u32;
    if unsafe {
        VirtualProtect(
            mapped,
            payload.len(),
            PAGE_EXECUTE_READ,
            &mut old_protection,
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        platform_free_executable(mapped.cast(), payload.len());
        return Err(executor_invalid(format!(
            "protect native Service executable image RX: {error}"
        )));
    }
    if unsafe { FlushInstructionCache(GetCurrentProcess(), mapped, payload.len()) } == 0 {
        let error = std::io::Error::last_os_error();
        platform_free_executable(mapped.cast(), payload.len());
        return Err(executor_invalid(format!(
            "flush native Service instruction cache: {error}"
        )));
    }

    Ok(NativeExecutableImage {
        ptr: mapped.cast(),
        len: payload.len(),
    })
}

#[cfg(not(all(target_arch = "x86_64", any(unix, windows))))]
fn platform_allocate_executable(
    _payload: &[u8],
) -> Result<NativeExecutableImage, NativeServiceExecutorError> {
    Err(executor_invalid(format!(
        "native Service execution is not enabled for host {}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )))
}

#[cfg(all(unix, target_arch = "x86_64"))]
fn platform_free_executable(ptr: *mut u8, len: usize) {
    use std::ffi::c_void;

    unsafe extern "C" {
        fn munmap(address: *mut c_void, length: usize) -> i32;
    }
    unsafe {
        let _ = munmap(ptr.cast(), len);
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn platform_free_executable(ptr: *mut u8, _len: usize) {
    use std::ffi::c_void;

    const MEM_RELEASE: u32 = 0x8000;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualFree(address: *mut c_void, size: usize, free_type: u32) -> i32;
    }
    unsafe {
        let _ = VirtualFree(ptr.cast(), 0, MEM_RELEASE);
    }
}

#[cfg(not(all(target_arch = "x86_64", any(unix, windows))))]
fn platform_free_executable(_ptr: *mut u8, _len: usize) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_bin::{
        assemble_service_bin, encode_cached_service_bin, AssemblyRecordKind, RequiredOid,
        ServiceAssemblyPlan, VerifiedAssemblyOidRecord, DONE_OID, SERVICE_PLAN_FORMAT,
    };
    use crate::source_registry::RelSourceKind;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
        source_id: SourceId,
        bootstrap: NativeServiceWorkerBootstrap,
        export_offset: u64,
        lifecycle_offset: u64,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture() -> Fixture {
        let source_id = SourceId::physical(RelSourceKind::Service, "worker").unwrap();
        let target = OidTarget::current().label();
        let export_oid = REL_OID_START;
        let lifecycle_oid = REL_OID_START + 1;
        let export_hash = "c".repeat(64);
        let lifecycle_hash = "d".repeat(64);
        let plan = ServiceAssemblyPlan {
            format: SERVICE_PLAN_FORMAT,
            service_identity: source_id.as_str().to_string(),
            service_source_sha256: "a".repeat(64),
            index_identity_sha256: "b".repeat(64),
            target_fingerprint: target.clone(),
            entry_oids: vec![export_oid, lifecycle_oid],
            required_oids: vec![
                RequiredOid {
                    oid: export_oid,
                    record_hash: export_hash.clone(),
                    kind: AssemblyRecordKind::ServiceExport,
                },
                RequiredOid {
                    oid: lifecycle_oid,
                    record_hash: lifecycle_hash.clone(),
                    kind: AssemblyRecordKind::Function,
                },
            ],
            placement_order: vec![export_oid, lifecycle_oid],
            call_graph: BTreeMap::new(),
            service_data: Vec::new(),
            data_alignment: 8,
            dependency_hashes: BTreeMap::new(),
            compile_options: BTreeMap::new(),
        };
        let records = BTreeMap::from([
            (
                export_oid,
                VerifiedAssemblyOidRecord {
                    oid: export_oid,
                    record_hash: export_hash,
                    kind: AssemblyRecordKind::ServiceExport,
                    target_fingerprint: target.clone(),
                    alignment: 1,
                    entry_offset: 0,
                    frame_terminator: Some(DONE_OID),
                    diagnostics: Vec::new(),
                    machine_code: vec![0xC3],
                    relocations: Vec::new(),
                },
            ),
            (
                lifecycle_oid,
                VerifiedAssemblyOidRecord {
                    oid: lifecycle_oid,
                    record_hash: lifecycle_hash,
                    kind: AssemblyRecordKind::Function,
                    target_fingerprint: target.clone(),
                    alignment: 1,
                    entry_offset: 0,
                    frame_terminator: Some(DONE_OID),
                    diagnostics: Vec::new(),
                    machine_code: vec![0xC3],
                    relocations: Vec::new(),
                },
            ),
        ]);
        let bin = assemble_service_bin(&plan, &records).unwrap();
        let encoded = encode_cached_service_bin(&bin).unwrap();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-native-worker-bootstrap-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("{}.bin", bin.assembly_hash));
        std::fs::write(&path, encoded).unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        let export_offset = u64::try_from(bin.placements[&export_oid].entry).unwrap();
        let lifecycle_offset = u64::try_from(bin.placements[&lifecycle_oid].entry).unwrap();
        let bootstrap = NativeServiceWorkerBootstrap {
            protocol: NATIVE_SERVICE_WORKER_BOOTSTRAP_PROTOCOL.to_string(),
            runtime_image_id: "image-b".into(),
            source_id: source_id.as_str().to_string(),
            oid_index_generation: 42,
            plan_hash: bin.plan_hash,
            assembly_hash: bin.assembly_hash,
            target_fingerprint: target,
            bin_path: path,
            exports: BTreeMap::from([(
                "run".into(),
                NativeServiceWorkerBootstrapEntry {
                    oid: export_oid,
                    entry_offset: export_offset,
                },
            )]),
            lifecycle: BTreeMap::from([(
                "start".into(),
                NativeServiceWorkerBootstrapEntry {
                    oid: lifecycle_oid,
                    entry_offset: lifecycle_offset,
                },
            )]),
        };
        Fixture {
            root,
            source_id,
            bootstrap,
            export_offset,
            lifecycle_offset,
        }
    }

    #[test]
    fn verifies_hash_bound_export_and_lifecycle_entries() {
        let fixture = fixture();
        let verified = verify_native_service_worker_bootstrap(
            fixture.bootstrap.clone(),
            "image-b",
            &fixture.source_id,
        )
        .unwrap();
        assert_eq!(verified.runtime_image_id(), "image-b");
        assert_eq!(verified.source_id(), &fixture.source_id);
        assert_eq!(verified.payload(), &[0xC3, 0xC3]);
        assert_eq!(
            verified.export("run").unwrap().entry_offset,
            usize::try_from(fixture.export_offset).unwrap()
        );
        assert_eq!(
            verified.lifecycle("start").unwrap().entry_offset,
            usize::try_from(fixture.lifecycle_offset).unwrap()
        );
    }

    #[test]
    fn rejects_bootstrap_identity_drift_before_execution() {
        let fixture = fixture();
        let error = verify_native_service_worker_bootstrap(
            fixture.bootstrap.clone(),
            "image-a",
            &fixture.source_id,
        )
        .unwrap_err();
        assert!(error
            .message()
            .contains("does not match supervised Runtime Image"));
    }

    #[test]
    fn rejects_foreign_target_before_execution() {
        let mut fixture = fixture();
        fixture.bootstrap.target_fingerprint = "foreign-target".into();
        let error = verify_native_service_worker_bootstrap(
            fixture.bootstrap.clone(),
            "image-b",
            &fixture.source_id,
        )
        .unwrap_err();
        assert!(error.message().contains("does not match this worker host"));
    }

    #[test]
    fn rejects_out_of_bounds_dispatch_offset() {
        let mut fixture = fixture();
        fixture
            .bootstrap
            .lifecycle
            .get_mut("start")
            .unwrap()
            .entry_offset = u64::try_from(fixture.bootstrap.exports.len() + 2).unwrap();
        let error = verify_native_service_worker_bootstrap(
            fixture.bootstrap.clone(),
            "image-b",
            &fixture.source_id,
        )
        .unwrap_err();
        assert!(error.message().contains("outside decoded payload length"));
    }

    #[test]
    fn rejects_cross_table_offset_aliasing() {
        let mut fixture = fixture();
        fixture
            .bootstrap
            .lifecycle
            .get_mut("start")
            .unwrap()
            .entry_offset = fixture.export_offset;
        let error = verify_native_service_worker_bootstrap(
            fixture.bootstrap.clone(),
            "image-b",
            &fixture.source_id,
        )
        .unwrap_err();
        assert!(error.message().contains("reuses payload offset"));
    }

    #[cfg(all(target_arch = "x86_64", any(unix, windows)))]
    #[test]
    fn executable_image_runs_canonical_boolean_leafs() {
        let true_image = NativeExecutableImage::new(&[0xB8, 1, 0, 0, 0, 0xC3]).unwrap();
        assert!(true_image.call_bool(0).unwrap());

        let false_image = NativeExecutableImage::new(&[0x31, 0xC0, 0xC3]).unwrap();
        assert!(!false_image.call_bool(0).unwrap());
    }

    #[cfg(all(target_arch = "x86_64", any(unix, windows)))]
    #[test]
    fn executable_image_rejects_noncanonical_boolean_result() {
        let image = NativeExecutableImage::new(&[0xB8, 2, 0, 0, 0, 0xC3]).unwrap();
        let error = image.call_bool(0).unwrap_err();
        assert!(error.message().contains("non-canonical value 2"));
    }
}