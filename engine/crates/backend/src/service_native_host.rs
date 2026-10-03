//! Native `.service` worker host for the Phase-5 OID cutover.
//!
//! The normal evaluator worker still owns source parsing and ModuleProgram
//! loading. A worker selected by RELC for the native path receives one immutable
//! host frame inside the already-authenticated Runtime ENV bootstrap pipe, strips
//! that internal frame before Runtime ENV is exposed, verifies the exact pinned
//! `.bin`, maps its executable payload, and serves the existing Service IPC ABI
//! without rereading `.service` source.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use service_runtime::{
    ServiceExecutionError, ServiceExecutor, ServiceLifecycle, ServiceMemory, ServiceMode,
    ServiceRequest, ServiceResponse,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use route_engine::service_native_worker::NativeServiceExecutor;
use route_engine::{
    verify_native_service_worker_bootstrap, NativeServiceWorkerBootstrap, RelSourceKind, SourceId,
};

pub const NATIVE_SERVICE_RUNTIME_ENV_KEY: &str = "__rbeNativeServiceWorkerV1";
pub const NATIVE_SERVICE_HOST_PROTOCOL: &str = "RBE-SERVICE-NATIVE-HOST/1";

const SERVICE_IPC_TIMEOUT: Duration = Duration::from_secs(5);
const SERVICE_IPC_REQUEST_MAX_BYTES: usize = 4 * 1024 * 1024;
const SERVICE_IPC_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;
const SERVICE_ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(50);
const SERVICE_ACCEPT_FAILURE_LIMIT: u32 = 8;

/// Process metadata that used to be recovered by reparsing the `.service` file.
/// It is compiler/catalog-owned and travels with the exact native bootstrap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeServiceHostFrame {
    pub protocol: String,
    pub service_name: String,
    pub mode: ServiceMode,
    pub memory_limit_mb: u64,
    pub runtime_image_id: String,
    pub bootstrap: NativeServiceWorkerBootstrap,
}

impl NativeServiceHostFrame {
    pub fn new(
        service_name: String,
        mode: ServiceMode,
        memory_limit_mb: u64,
        runtime_image_id: String,
        bootstrap: NativeServiceWorkerBootstrap,
    ) -> Self {
        Self {
            protocol: NATIVE_SERVICE_HOST_PROTOCOL.to_string(),
            service_name,
            mode,
            memory_limit_mb,
            runtime_image_id,
            bootstrap,
        }
    }

    fn validate(&self, expected_service_name: &str) -> anyhow::Result<SourceId> {
        if self.protocol != NATIVE_SERVICE_HOST_PROTOCOL {
            anyhow::bail!(
                "unsupported native Service host protocol {:?}; expected {:?}",
                self.protocol,
                NATIVE_SERVICE_HOST_PROTOCOL
            );
        }
        if self.service_name != expected_service_name {
            anyhow::bail!(
                "native Service host frame names {:?}, supervised worker names {:?}",
                self.service_name,
                expected_service_name
            );
        }
        if self.runtime_image_id != self.bootstrap.runtime_image_id {
            anyhow::bail!(
                "native Service host Runtime Image {:?} does not match worker bootstrap {:?}",
                self.runtime_image_id,
                self.bootstrap.runtime_image_id
            );
        }
        let source_id = SourceId::physical(RelSourceKind::Service, expected_service_name)
            .map_err(|error| anyhow::anyhow!("invalid native Service identity: {error}"))?;
        if self.bootstrap.source_id != source_id.as_str() {
            anyhow::bail!(
                "native Service bootstrap source {:?} does not match catalog Service {}",
                self.bootstrap.source_id,
                source_id
            );
        }
        if self.bootstrap.exports.is_empty() {
            anyhow::bail!("native Service host frame has no public exports");
        }
        let export_names = self
            .bootstrap
            .exports
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if export_names.len() != self.bootstrap.exports.len() {
            anyhow::bail!("native Service host frame contains duplicate export names");
        }
        Ok(source_id)
    }
}

/// Remove the reserved native bootstrap bundle from a Runtime ENV snapshot and
/// return only the frame belonging to this worker. The internal key is removed
/// even when this Service has no native frame, so user code can never observe
/// compiler/worker metadata as an application environment variable.
pub fn take_native_service_frame(
    runtime_env: &mut Value,
    expected_service_name: &str,
) -> anyhow::Result<Option<NativeServiceHostFrame>> {
    let Some(object) = runtime_env.as_object_mut() else {
        anyhow::bail!("service Runtime ENV bootstrap must be a JSON object");
    };
    let Some(raw_bundle) = object.remove(NATIVE_SERVICE_RUNTIME_ENV_KEY) else {
        return Ok(None);
    };
    let mut bundle: BTreeMap<String, NativeServiceHostFrame> = serde_json::from_value(raw_bundle)
        .map_err(|error| anyhow::anyhow!("decode native Service bootstrap bundle: {error}"))?;
    let frame = bundle.remove(expected_service_name);
    if let Some(frame) = frame.as_ref() {
        frame.validate(expected_service_name)?;
    }
    Ok(frame)
}

/// Merge compiler-owned native frames into the authenticated Runtime ENV
/// transport. Existing user Runtime ENV is preserved, but the reserved key may
/// never be supplied by user configuration because it is overwritten here.
pub fn attach_native_service_frames(
    runtime_env: &mut Value,
    frames: BTreeMap<String, NativeServiceHostFrame>,
) -> anyhow::Result<()> {
    let Some(object) = runtime_env.as_object_mut() else {
        anyhow::bail!("service Runtime ENV snapshot must be a JSON object");
    };
    if frames.is_empty() {
        object.remove(NATIVE_SERVICE_RUNTIME_ENV_KEY);
        return Ok(());
    }
    object.insert(
        NATIVE_SERVICE_RUNTIME_ENV_KEY.to_string(),
        serde_json::to_value(frames)?,
    );
    Ok(())
}

pub async fn run_native_service_host(
    frame: NativeServiceHostFrame,
    expected_service_name: &str,
    token: String,
) -> anyhow::Result<()> {
    let source_id = frame.validate(expected_service_name)?;
    apply_native_memory_limit(frame.memory_limit_mb)?;
    let verified = verify_native_service_worker_bootstrap(
        frame.bootstrap,
        &frame.runtime_image_id,
        &source_id,
    )
    .map_err(|error| anyhow::anyhow!("verify native Service worker bootstrap: {error}"))?;
    let executor = Arc::new(
        NativeServiceExecutor::new(verified)
            .map_err(|error| anyhow::anyhow!("prepare native Service executor: {error}"))?,
    );
    let memory = ServiceMemory::default();
    let listener = TcpListener::bind("127.0.0.1:0").await?;

    executor
        .lifecycle(
            ServiceLifecycle::Start,
            lifecycle_context(expected_service_name, frame.mode),
        )
        .await
        .map_err(|error| lifecycle_start_error(expected_service_name, error))?;

    let ready = service_runtime::ServiceReady {
        service: expected_service_name.to_string(),
        pid: std::process::id(),
        address: listener.local_addr()?,
    };
    println!("{}", serde_json::to_string(&ready)?);
    use std::io::Write as _;
    std::io::stdout().flush()?;

    let mut parent_liveness = service_runtime::parent_liveness_signal_if_configured()?;
    let mut accept_failures = 0u32;
    loop {
        let accepted = match parent_liveness.as_mut() {
            Some(parent_liveness) => {
                tokio::select! {
                    accepted = listener.accept() => Some(accepted),
                    _ = parent_liveness => None,
                }
            }
            None => Some(listener.accept().await),
        };
        let Some(accepted) = accepted else {
            tracing::warn!(
                service = %expected_service_name,
                "native Service parent liveness closed; stopping orphaned worker"
            );
            if let Err(error) = executor
                .lifecycle(
                    ServiceLifecycle::Stop,
                    lifecycle_context(expected_service_name, frame.mode),
                )
                .await
            {
                tracing::warn!(
                    service = %expected_service_name,
                    code = %error.code,
                    message = %error.message,
                    "native Service stop lifecycle failed after parent loss"
                );
            }
            return Ok(());
        };

        let (stream, peer) = match accepted {
            Ok(accepted) => {
                accept_failures = 0;
                accepted
            }
            Err(error) => {
                accept_failures = accept_failures.saturating_add(1);
                if accept_failures >= SERVICE_ACCEPT_FAILURE_LIMIT {
                    anyhow::bail!(
                        "native Service {:?} listener failed {accept_failures} consecutive accepts: {error}",
                        expected_service_name
                    );
                }
                tracing::warn!(
                    service = %expected_service_name,
                    error = %error,
                    accept_failures,
                    retry_ms = SERVICE_ACCEPT_RETRY_DELAY.as_millis() as u64,
                    "native Service IPC listener accept failed; retrying"
                );
                tokio::time::sleep(SERVICE_ACCEPT_RETRY_DELAY).await;
                continue;
            }
        };
        if !peer.ip().is_loopback() {
            tracing::warn!(
                %peer,
                service = %expected_service_name,
                "native Service IPC rejected non-loopback peer"
            );
            continue;
        }

        let (read, mut write) = stream.into_split();
        let line = match tokio::time::timeout(
            SERVICE_IPC_TIMEOUT,
            read_bounded_line(read, SERVICE_IPC_REQUEST_MAX_BYTES, "native service request"),
        )
        .await
        {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => {
                tracing::warn!(
                    service = %expected_service_name,
                    error = %error,
                    "invalid native Service IPC frame"
                );
                continue;
            }
            Err(_) => {
                tracing::warn!(
                    service = %expected_service_name,
                    "native Service IPC request timed out"
                );
                continue;
            }
        };
        let request: ServiceRequest = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(
                    service = %expected_service_name,
                    %peer,
                    bytes = line.len(),
                    error = %error,
                    "invalid native Service IPC JSON"
                );
                continue;
            }
        };
        let (response, shutdown) = dispatch(
            request,
            expected_service_name,
            frame.mode,
            &token,
            &memory,
            executor.as_ref(),
        )
        .await;
        let mut payload = serde_json::to_vec(&response)?;
        if payload.len().saturating_add(1) > SERVICE_IPC_RESPONSE_MAX_BYTES {
            payload = serde_json::to_vec(&ServiceResponse::Error {
                code: "SVC4002".into(),
                message: "service IPC response exceeded frame limit".into(),
            })?;
        }
        payload.push(b'\n');
        if let Err(error) = tokio::time::timeout(SERVICE_IPC_TIMEOUT, async {
            write.write_all(&payload).await?;
            write.shutdown().await
        })
        .await
        .map_err(|_| anyhow::anyhow!("native Service IPC response write timed out"))
        .and_then(|result| result.map_err(anyhow::Error::from))
        {
            tracing::warn!(
                service = %expected_service_name,
                error = %error,
                "native Service IPC response write failed"
            );
        }
        if shutdown {
            return Ok(());
        }
    }
}

async fn dispatch(
    request: ServiceRequest,
    service_name: &str,
    mode: ServiceMode,
    token: &str,
    memory: &ServiceMemory,
    executor: &dyn ServiceExecutor,
) -> (ServiceResponse, bool) {
    if !constant_time_eq(request_token(&request).as_bytes(), token.as_bytes()) {
        return (
            ServiceResponse::Error {
                code: "SVC4001".into(),
                message: "service IPC authentication failed".into(),
            },
            false,
        );
    }

    let ok = |value| (ServiceResponse::Ok { value }, false);
    match request {
        ServiceRequest::Health { .. } => {
            match executor
                .lifecycle(
                    ServiceLifecycle::Health,
                    lifecycle_context(service_name, mode),
                )
                .await
            {
                Ok(lifecycle) => {
                    let healthy = lifecycle_health_ok(lifecycle.as_ref());
                    ok(serde_json::json!({
                        "ok": healthy,
                        "service": service_name,
                        "pid": std::process::id(),
                        "memoryEntries": memory.len(),
                        "lifecycle": lifecycle.unwrap_or(Value::Null)
                    }))
                }
                Err(error) => execution_error_response(error, false),
            }
        }
        ServiceRequest::Event { event, .. } => {
            match executor.lifecycle(ServiceLifecycle::Event, event).await {
                Ok(Some(value)) => ok(value),
                Ok(None) => (
                    ServiceResponse::Error {
                        code: "SVC4304".into(),
                        message: "service does not define Service.event()".into(),
                    },
                    false,
                ),
                Err(error) => execution_error_response(error, false),
            }
        }
        ServiceRequest::MemoryGet { key, .. } => ok(memory.get(&key).unwrap_or(Value::Null)),
        ServiceRequest::MemorySet { key, value, .. } => {
            memory.set(key, value);
            ok(Value::Bool(true))
        }
        ServiceRequest::MemoryDelete { key, .. } => ok(Value::Bool(memory.delete(&key))),
        ServiceRequest::MemoryClear { .. } => {
            memory.clear();
            ok(Value::Bool(true))
        }
        ServiceRequest::Call { function, args, .. } => match executor.call(&function, args).await {
            Ok(value) => ok(value),
            Err(error) => execution_error_response(error, false),
        },
        ServiceRequest::Shutdown { .. } => {
            match executor
                .lifecycle(
                    ServiceLifecycle::Stop,
                    lifecycle_context(service_name, mode),
                )
                .await
            {
                Ok(value) => (
                    ServiceResponse::Ok {
                        value: value.unwrap_or(Value::Bool(true)),
                    },
                    true,
                ),
                Err(error) => execution_error_response(error, true),
            }
        }
    }
}

fn request_token(request: &ServiceRequest) -> &str {
    match request {
        ServiceRequest::Health { token }
        | ServiceRequest::Call { token, .. }
        | ServiceRequest::Event { token, .. }
        | ServiceRequest::MemoryGet { token, .. }
        | ServiceRequest::MemorySet { token, .. }
        | ServiceRequest::MemoryDelete { token, .. }
        | ServiceRequest::MemoryClear { token }
        | ServiceRequest::Shutdown { token } => token,
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&left, &right) in left.iter().zip(right.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

fn execution_error_response(
    error: ServiceExecutionError,
    shutdown: bool,
) -> (ServiceResponse, bool) {
    (
        ServiceResponse::Error {
            code: error.code,
            message: error.message,
        },
        shutdown,
    )
}

fn lifecycle_context(service_name: &str, mode: ServiceMode) -> Value {
    serde_json::json!({
        "service": service_name,
        "pid": std::process::id(),
        "mode": mode,
    })
}

fn lifecycle_health_ok(value: Option<&Value>) -> bool {
    match value {
        None => true,
        Some(Value::Bool(value)) => *value,
        Some(Value::Object(fields)) => fields.get("ok").and_then(Value::as_bool).unwrap_or(true),
        _ => true,
    }
}

fn lifecycle_start_error(service_name: &str, error: ServiceExecutionError) -> anyhow::Error {
    anyhow::anyhow!(
        "native service {service_name:?} start lifecycle failed with {}: {}",
        error.code,
        error.message
    )
}

async fn read_bounded_line<R>(reader: R, max_bytes: usize, label: &str) -> anyhow::Result<String>
where
    R: AsyncRead + Unpin,
{
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut reader = BufReader::new(reader).take(limit);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 {
        anyhow::bail!("{label} is empty");
    }
    if bytes > max_bytes {
        anyhow::bail!("{label} exceeded {max_bytes} bytes");
    }
    if !line.ends_with('\n') {
        anyhow::bail!("{label} is not newline terminated");
    }
    Ok(line)
}

#[cfg(unix)]
fn apply_native_memory_limit(memory_limit_mb: u64) -> anyhow::Result<()> {
    if memory_limit_mb == 0 {
        return Ok(());
    }

    #[repr(C)]
    struct RLimit {
        current: usize,
        maximum: usize,
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    const RLIMIT_AS: i32 = 9;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    const RLIMIT_AS: i32 = 5;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    const RLIMIT_AS: i32 = -1;

    if RLIMIT_AS < 0 {
        anyhow::bail!("native Service memory limit is unsupported on this Unix target");
    }
    unsafe extern "C" {
        fn setrlimit(resource: i32, limit: *const RLimit) -> i32;
    }
    let bytes = memory_limit_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow::anyhow!("native Service memoryLimitMb is too large"))?;
    let bytes = usize::try_from(bytes)
        .map_err(|_| anyhow::anyhow!("native Service memoryLimitMb exceeds addressable memory"))?;
    let limit = RLimit {
        current: bytes,
        maximum: bytes,
    };
    if unsafe { setrlimit(RLIMIT_AS, &limit) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(windows)]
fn apply_native_memory_limit(memory_limit_mb: u64) -> anyhow::Result<()> {
    use std::ffi::c_void;

    if memory_limit_mb == 0 {
        return Ok(());
    }

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimitInformation {
        basic_limit_information: BasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_LIMIT_PROCESS_MEMORY: u32 = 0x0000_0100;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            class: i32,
            information: *const c_void,
            length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    let bytes = memory_limit_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow::anyhow!("native Service memoryLimitMb is too large"))?;
    let bytes = usize::try_from(bytes)
        .map_err(|_| anyhow::anyhow!("native Service memoryLimitMb exceeds addressable memory"))?;
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(anyhow::anyhow!(
            "create native Service memory Job Object: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut limits = ExtendedLimitInformation::default();
    limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    limits.process_memory_limit = bytes;
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
            (&limits as *const ExtendedLimitInformation).cast(),
            std::mem::size_of::<ExtendedLimitInformation>() as u32,
        )
    };
    if configured == 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            let _ = CloseHandle(job);
        }
        return Err(anyhow::anyhow!(
            "configure native Service memory Job Object: {error}"
        ));
    }
    if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            let _ = CloseHandle(job);
        }
        return Err(anyhow::anyhow!(
            "assign native Service memory Job Object: {error}"
        ));
    }
    unsafe {
        let _ = CloseHandle(job);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn apply_native_memory_limit(memory_limit_mb: u64) -> anyhow::Result<()> {
    if memory_limit_mb == 0 {
        Ok(())
    } else {
        anyhow::bail!("native Service memoryLimitMb is unsupported on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_transport_is_removed_before_runtime_env_exposure() {
        let source_id = SourceId::physical(RelSourceKind::Service, "demo").unwrap();
        let bootstrap = NativeServiceWorkerBootstrap {
            protocol: route_engine::NATIVE_SERVICE_WORKER_BOOTSTRAP_PROTOCOL.to_string(),
            runtime_image_id: "image-a".into(),
            source_id: source_id.as_str().to_string(),
            oid_index_generation: 7,
            plan_hash: "a".repeat(64),
            assembly_hash: "b".repeat(64),
            target_fingerprint: route_engine::OidTarget::current().label(),
            bin_path: std::env::temp_dir().join(format!("{}.bin", "b".repeat(64))),
            exports: BTreeMap::from([(
                "run".into(),
                route_engine::NativeServiceWorkerBootstrapEntry {
                    oid: route_engine::OID_REL_START,
                    entry_offset: 0,
                },
            )]),
            lifecycle: BTreeMap::new(),
        };
        let frame = NativeServiceHostFrame::new(
            "demo".into(),
            ServiceMode::Resident,
            64,
            "image-a".into(),
            bootstrap,
        );
        let mut runtime_env = serde_json::json!({"VISIBLE": "yes"});
        attach_native_service_frames(
            &mut runtime_env,
            BTreeMap::from([("demo".into(), frame)]),
        )
        .unwrap();
        let decoded = take_native_service_frame(&mut runtime_env, "demo")
            .unwrap()
            .unwrap();
        assert_eq!(decoded.service_name, "demo");
        assert_eq!(runtime_env, serde_json::json!({"VISIBLE": "yes"}));
    }

    #[test]
    fn native_frame_rejects_supervised_name_drift() {
        let source_id = SourceId::physical(RelSourceKind::Service, "demo").unwrap();
        let bootstrap = NativeServiceWorkerBootstrap {
            protocol: route_engine::NATIVE_SERVICE_WORKER_BOOTSTRAP_PROTOCOL.to_string(),
            runtime_image_id: "image-a".into(),
            source_id: source_id.as_str().to_string(),
            oid_index_generation: 7,
            plan_hash: "a".repeat(64),
            assembly_hash: "b".repeat(64),
            target_fingerprint: route_engine::OidTarget::current().label(),
            bin_path: std::env::temp_dir().join(format!("{}.bin", "b".repeat(64))),
            exports: BTreeMap::from([(
                "run".into(),
                route_engine::NativeServiceWorkerBootstrapEntry {
                    oid: route_engine::OID_REL_START,
                    entry_offset: 0,
                },
            )]),
            lifecycle: BTreeMap::new(),
        };
        let frame = NativeServiceHostFrame::new(
            "demo".into(),
            ServiceMode::Resident,
            64,
            "image-a".into(),
            bootstrap,
        );
        assert!(frame.validate("other").is_err());
    }
}
