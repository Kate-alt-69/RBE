use std::collections::BTreeSet;

use anyhow::{bail, Context};
use core_lib::{
    call_public_dns, call_public_http, ExpectedWorkerIdentity, LibraryCapabilityGrant,
    LibraryHostCall, LibraryHostCallReply, LibraryPackageIdentity, LibraryRuntimeIdentity,
    LibrarySdkIdentity, LibrarySessionBinding, LIBRARY_ABI_VERSION, MAX_LIBRARY_PAYLOAD_BYTES,
};
use rand::RngCore;
use rbe_install_runtime::VerifiedRpxRootSnapshot;
use serde::Deserialize;
use serde_json::Value;

const SESSION_ID_BYTES: usize = 32;
const LIBRARY_LOG_CAPABILITY: &str = "log";
const LIBRARY_NET_HTTP_CAPABILITY: &str = "net:http";
const LIBRARY_NET_DNS_CAPABILITY: &str = "net:dns";
const LIBRARY_LOG_MAX_REQUEST_BYTES: usize = 64 * 1024;
const LIBRARY_LOG_MAX_RESPONSE_BYTES: usize = 1024;
const LIBRARY_LOG_MAX_SCOPE_DEPTH: usize = 16;
const LIBRARY_LOG_MAX_MESSAGE_BYTES: usize = 48 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LibraryLogRecord {
    #[serde(default)]
    scope: Vec<String>,
    message: String,
}

pub fn expected_worker(
    snapshot: &VerifiedRpxRootSnapshot,
) -> anyhow::Result<ExpectedWorkerIdentity> {
    let worker = &snapshot.worker;
    if worker.package != snapshot.package
        || worker.version != snapshot.version
        || !worker
            .artifact_sha256
            .eq_ignore_ascii_case(&snapshot.artifact_sha256)
    {
        bail!(
            "verified package worker identity drifted from RPX root snapshot for {:?}",
            snapshot.package
        );
    }
    if worker.rbe_abi_min == 0
        || worker.rbe_abi_min > worker.rbe_abi_max
        || LIBRARY_ABI_VERSION < worker.rbe_abi_min
        || LIBRARY_ABI_VERSION > worker.rbe_abi_max
    {
        bail!(
            "verified package {:?} supports RBE ABI {}..={} but Backend selected ABI {}",
            snapshot.package,
            worker.rbe_abi_min,
            worker.rbe_abi_max,
            LIBRARY_ABI_VERSION
        );
    }

    Ok(ExpectedWorkerIdentity {
        package: LibraryPackageIdentity {
            name: snapshot.package.clone(),
            version: snapshot.version.clone(),
            artifact_sha256: snapshot.artifact_sha256.to_ascii_lowercase(),
        },
        sdk: LibrarySdkIdentity {
            language: worker.sdk_language.clone(),
            name: worker.sdk_name.clone(),
            version: worker.sdk_version.clone(),
        },
        runtime: LibraryRuntimeIdentity {
            kind: worker.runtime_kind.clone(),
            version: worker.runtime_version.clone(),
        },
        abi: LIBRARY_ABI_VERSION,
    })
}

fn package_log_grant(package: &str) -> anyhow::Result<LibraryCapabilityGrant> {
    LibraryCapabilityGrant::new(
        LIBRARY_LOG_CAPABILITY,
        format!("lib/{package}"),
        [
            "debug".to_string(),
            "info".to_string(),
            "warn".to_string(),
            "error".to_string(),
            "fatal".to_string(),
        ],
        LIBRARY_LOG_MAX_REQUEST_BYTES,
        LIBRARY_LOG_MAX_RESPONSE_BYTES,
    )
    .context("build verified package logging capability grant")
}

fn package_http_grant() -> anyhow::Result<LibraryCapabilityGrant> {
    LibraryCapabilityGrant::new(
        LIBRARY_NET_HTTP_CAPABILITY,
        LIBRARY_NET_HTTP_CAPABILITY,
        ["get".to_string(), "post".to_string(), "request".to_string()],
        MAX_LIBRARY_PAYLOAD_BYTES,
        MAX_LIBRARY_PAYLOAD_BYTES,
    )
    .context("build verified package public HTTP capability grant")
}

fn package_dns_grant() -> anyhow::Result<LibraryCapabilityGrant> {
    LibraryCapabilityGrant::new(
        LIBRARY_NET_DNS_CAPABILITY,
        LIBRARY_NET_DNS_CAPABILITY,
        ["lookup".to_string(), "ip".to_string(), "mx".to_string()],
        MAX_LIBRARY_PAYLOAD_BYTES,
        MAX_LIBRARY_PAYLOAD_BYTES,
    )
    .context("build verified package public DNS capability grant")
}

pub fn grants_for_verified_requests(
    requests: &[String],
) -> anyhow::Result<Vec<LibraryCapabilityGrant>> {
    let mut grants = Vec::new();
    let mut seen = BTreeSet::new();
    for request in requests {
        if !seen.insert(request.as_str()) {
            continue;
        }
        match request.as_str() {
            LIBRARY_LOG_CAPABILITY => {}
            LIBRARY_NET_HTTP_CAPABILITY => grants.push(package_http_grant()?),
            LIBRARY_NET_DNS_CAPABILITY => grants.push(package_dns_grant()?),
            _ => {}
        }
    }
    Ok(grants)
}

pub async fn dispatch_authorized_host_call(
    package: &str,
    binding: &LibrarySessionBinding,
    call: &LibraryHostCall,
) -> anyhow::Result<LibraryHostCallReply> {
    let grant = binding
        .authorize_host_call(call)
        .context("authorize package host call against accepted Library Host session")?;

    let payload = match call.capability.as_str() {
        LIBRARY_LOG_CAPABILITY => dispatch_package_log_call(package, call)?,
        LIBRARY_NET_HTTP_CAPABILITY => dispatch_package_http_call(call).await?,
        LIBRARY_NET_DNS_CAPABILITY => dispatch_package_dns_call(call).await?,
        capability => bail!(
            "trusted Backend dispatcher for package capability {capability:?} is not installed"
        ),
    };

    if payload.len() > grant.max_response_bytes {
        bail!(
            "package host call response exceeded admitted capability limit: capability={:?}, limit={}, observed={}",
            grant.capability,
            grant.max_response_bytes,
            payload.len()
        );
    }

    LibraryHostCallReply::success(call.call_id, payload)
        .context("encode successful package host-call reply")
}

async fn dispatch_package_http_call(call: &LibraryHostCall) -> anyhow::Result<Vec<u8>> {
    if call.capability != LIBRARY_NET_HTTP_CAPABILITY || call.target != LIBRARY_NET_HTTP_CAPABILITY
    {
        bail!("package HTTP call does not match admitted net:http authority");
    }
    if !matches!(call.operation.as_str(), "get" | "post" | "request") {
        bail!("unsupported package HTTP operation {:?}", call.operation);
    }
    let args: Vec<Value> = serde_json::from_slice(&call.payload)
        .context("package net:http payload must be a JSON argument array")?;
    let value = call_public_http(&call.operation, &args)
        .await
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
    serde_json::to_vec(&value).context("encode package net:http response")
}

async fn dispatch_package_dns_call(call: &LibraryHostCall) -> anyhow::Result<Vec<u8>> {
    if call.capability != LIBRARY_NET_DNS_CAPABILITY || call.target != LIBRARY_NET_DNS_CAPABILITY {
        bail!("package DNS call does not match admitted net:dns authority");
    }
    if !matches!(call.operation.as_str(), "lookup" | "ip" | "mx") {
        bail!("unsupported package DNS operation {:?}", call.operation);
    }
    let value = call_public_dns(&call.operation, &call.payload)
        .await
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
    serde_json::to_vec(&value).context("encode package net:dns response")
}

fn dispatch_package_log_call(package: &str, call: &LibraryHostCall) -> anyhow::Result<Vec<u8>> {
    let expected_target = format!("lib/{package}");
    if call.capability != LIBRARY_LOG_CAPABILITY || call.target != expected_target {
        bail!("package log call does not match verified library authority");
    }
    if !matches!(
        call.operation.as_str(),
        "debug" | "info" | "warn" | "error" | "fatal"
    ) {
        bail!("unsupported package log operation {:?}", call.operation);
    }

    let record: LibraryLogRecord =
        serde_json::from_slice(&call.payload).context("decode structured package log record")?;
    if record.scope.len() > LIBRARY_LOG_MAX_SCOPE_DEPTH {
        bail!(
            "package log scope exceeds maximum depth {}",
            LIBRARY_LOG_MAX_SCOPE_DEPTH
        );
    }
    if record.message.len() > LIBRARY_LOG_MAX_MESSAGE_BYTES {
        bail!(
            "package log message exceeds maximum size {} bytes",
            LIBRARY_LOG_MAX_MESSAGE_BYTES
        );
    }
    if record.message.chars().any(char::is_control) {
        bail!("package log message cannot contain control characters");
    }

    let mut module = expected_target;
    for scope in &record.scope {
        if !valid_log_component(scope) {
            bail!("package log scope contains an invalid component");
        }
        module.push('/');
        module.push_str(scope);
    }

    match call.operation.as_str() {
        "debug" => tracing::debug!(module = %module, "{}", record.message),
        "info" => tracing::info!(module = %module, "{}", record.message),
        "warn" => tracing::warn!(module = %module, "{}", record.message),
        "error" => tracing::error!(module = %module, "{}", record.message),
        "fatal" => tracing::error!(module = %module, fatal = true, "{}", record.message),
        _ => unreachable!("log operation validated above"),
    }

    Ok(Vec::new())
}

fn valid_log_component(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub fn bind_session(
    snapshot: &VerifiedRpxRootSnapshot,
    grants: impl IntoIterator<Item = LibraryCapabilityGrant>,
) -> anyhow::Result<LibrarySessionBinding> {
    let expected = expected_worker(snapshot)?;
    let mut admitted = Vec::new();
    for grant in grants {
        if grant.capability == LIBRARY_LOG_CAPABILITY {
            bail!("package log capability is host-owned and cannot be supplied by callers");
        }
        admitted.push(grant);
    }
    admitted.push(package_log_grant(&expected.package.name)?);

    let mut nonce = [0u8; SESSION_ID_BYTES];
    let mut rng = rand::rngs::OsRng;
    rng.fill_bytes(&mut nonce);
    let capability_identity = format!("session:{}", hex::encode(nonce));

    LibrarySessionBinding::new(expected, admitted, capability_identity)
        .context("bind verified package worker to Library Host session")
}

#[cfg(test)]
mod tests {
    use core_lib::{LibraryWorkerHello, LIBRARY_PROTOCOL_VERSION};
    use rbe_install_runtime::VerifiedPackageWorkerIdentity;

    use super::*;

    fn snapshot() -> VerifiedRpxRootSnapshot {
        VerifiedRpxRootSnapshot {
            package: "advancenet".into(),
            version: "2.0.0".into(),
            artifact_sha256: "a".repeat(64),
            index_json: "{}".into(),
            worker: VerifiedPackageWorkerIdentity {
                package: "advancenet".into(),
                version: "2.0.0".into(),
                artifact_sha256: "a".repeat(64),
                rbe_abi_min: LIBRARY_ABI_VERSION,
                rbe_abi_max: LIBRARY_ABI_VERSION,
                sdk_language: "bun".into(),
                sdk_name: "@rbe/sdk".into(),
                sdk_version: "0.1.9".into(),
                runtime_kind: "bun".into(),
                runtime_version: "1.3.7".into(),
                runtime_entry: "src/index.js".into(),
                runtime_managed: true,
            },
        }
    }

    fn hello(snapshot: &VerifiedRpxRootSnapshot) -> LibraryWorkerHello {
        let expected = expected_worker(snapshot).unwrap();
        LibraryWorkerHello {
            protocol: LIBRARY_PROTOCOL_VERSION,
            package: expected.package,
            sdk: expected.sdk,
            runtime: expected.runtime,
            abi_min: LIBRARY_ABI_VERSION,
            abi_max: LIBRARY_ABI_VERSION,
        }
    }

    fn log_call(target: &str, operation: &str, payload: &[u8]) -> LibraryHostCall {
        LibraryHostCall {
            call_id: 1,
            capability: "log".into(),
            target: target.into(),
            operation: operation.into(),
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn verified_snapshot_becomes_exact_expected_worker_identity() {
        let snapshot = snapshot();
        let expected = expected_worker(&snapshot).unwrap();
        assert_eq!(expected.package.name, "advancenet");
        assert_eq!(expected.package.version, "2.0.0");
        assert_eq!(expected.package.artifact_sha256, "a".repeat(64));
        assert_eq!(expected.sdk.language, "bun");
        assert_eq!(expected.sdk.name, "@rbe/sdk");
        assert_eq!(expected.sdk.version, "0.1.9");
        assert_eq!(expected.runtime.kind, "bun");
        assert_eq!(expected.runtime.version, "1.3.7");
        assert_eq!(expected.abi, LIBRARY_ABI_VERSION);
    }

    #[test]
    fn verified_requests_admit_only_implemented_rbe_privileges() {
        let grants = grants_for_verified_requests(&[
            "log".into(),
            "mail:queue".into(),
            "net:http".into(),
            "net:dns".into(),
            "mail:smtp".into(),
        ])
        .unwrap();
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[0].capability, "net:http");
        assert_eq!(grants[1].capability, "net:dns");
    }

    #[test]
    fn package_specific_and_unavailable_requests_do_not_block_worker_start() {
        let grants = grants_for_verified_requests(&[
            "mail:smtp".into(),
            "mail:provider:resend".into(),
            "net:tcp".into(),
        ])
        .unwrap();
        assert!(grants.is_empty());
    }

    #[test]
    fn session_uses_opaque_identity_supplied_grants_and_verified_log_scope() {
        let snapshot = snapshot();
        let mut session = bind_session(
            &snapshot,
            [LibraryCapabilityGrant::new(
                "net:http",
                "net:http",
                ["request".to_string()],
                1024,
                4096,
            )
            .unwrap()],
        )
        .unwrap();
        let info = session.accept_hello(&hello(&snapshot)).unwrap();
        assert!(info.capability_identity.starts_with("session:"));
        assert_eq!(info.capability_identity.len(), "session:".len() + 64);
        assert_eq!(
            info.granted_capabilities,
            std::collections::BTreeSet::from(["log".to_string(), "net:http".to_string()])
        );

        assert!(session
            .authorize_host_call(&log_call(
                "lib/advancenet",
                "info",
                br#"{"scope":[],"message":"ready"}"#,
            ))
            .is_ok());
        assert!(session
            .authorize_host_call(&log_call(
                "lib/other-package",
                "info",
                br#"{"scope":[],"message":"spoof"}"#,
            ))
            .is_err());
    }

    #[test]
    fn host_owned_log_capability_cannot_be_replaced_or_widened() {
        let snapshot = snapshot();
        let error = bind_session(
            &snapshot,
            [LibraryCapabilityGrant::new(
                "log",
                "lib/other-package",
                ["info".to_string()],
                1024,
                1024,
            )
            .unwrap()],
        )
        .err()
        .expect("host-owned log grant must be rejected");
        assert!(error.to_string().contains("host-owned"));
    }

    #[tokio::test]
    async fn authorized_log_call_dispatches_and_returns_typed_reply() {
        let snapshot = snapshot();
        let mut session = bind_session(&snapshot, std::iter::empty()).unwrap();
        session.accept_hello(&hello(&snapshot)).unwrap();
        let call = log_call(
            "lib/advancenet",
            "info",
            br#"{"scope":["smtp"],"message":"ready"}"#,
        );
        let reply = dispatch_authorized_host_call("advancenet", &session, &call)
            .await
            .unwrap();
        assert_eq!(reply.call_id, call.call_id);
        assert!(reply.ok);
        assert!(reply.payload.is_empty());
        assert!(reply.error.is_none());
    }

    #[tokio::test]
    async fn ungranted_custom_host_call_fails_at_authority_boundary() {
        let snapshot = snapshot();
        let mut session = bind_session(&snapshot, std::iter::empty()).unwrap();
        session.accept_hello(&hello(&snapshot)).unwrap();
        let call = LibraryHostCall {
            call_id: 9,
            capability: "mail:smtp".into(),
            target: "mail:smtp".into(),
            operation: "send".into(),
            payload: b"[]".to_vec(),
        };
        let error = dispatch_authorized_host_call("advancenet", &session, &call)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("authorize package host call"));
    }

    #[tokio::test]
    async fn package_http_dispatch_requires_json_argument_array_before_network() {
        let snapshot = snapshot();
        let mut session = bind_session(&snapshot, [package_http_grant().unwrap()]).unwrap();
        session.accept_hello(&hello(&snapshot)).unwrap();
        let call = LibraryHostCall {
            call_id: 10,
            capability: "net:http".into(),
            target: "net:http".into(),
            operation: "request".into(),
            payload: b"{}".to_vec(),
        };
        let error = dispatch_authorized_host_call("advancenet", &session, &call)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("JSON argument array"));
    }

    #[tokio::test]
    async fn package_dns_dispatch_rejects_local_name_before_network() {
        let snapshot = snapshot();
        let mut session = bind_session(&snapshot, [package_dns_grant().unwrap()]).unwrap();
        session.accept_hello(&hello(&snapshot)).unwrap();
        let call = LibraryHostCall {
            call_id: 11,
            capability: "net:dns".into(),
            target: "net:dns".into(),
            operation: "mx".into(),
            payload: b"localhost".to_vec(),
        };
        let error = dispatch_authorized_host_call("advancenet", &session, &call)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("DNS3000"));
    }

    #[test]
    fn package_log_dispatch_keeps_child_scope_below_verified_library() {
        let call = log_call(
            "lib/mail",
            "info",
            br#"{"scope":["smtp","delivery"],"message":"queued"}"#,
        );
        assert!(dispatch_package_log_call("mail", &call).is_ok());
        assert!(dispatch_package_log_call("other", &call).is_err());
    }

    #[test]
    fn package_log_dispatch_rejects_control_and_scope_injection() {
        assert!(dispatch_package_log_call(
            "mail",
            &log_call(
                "lib/mail",
                "warn",
                b"{\"scope\":[\"../backend\"],\"message\":\"nope\"}",
            ),
        )
        .is_err());
        assert!(dispatch_package_log_call(
            "mail",
            &log_call(
                "lib/mail",
                "warn",
                b"{\"scope\":[],\"message\":\"fake\\nline\"}",
            ),
        )
        .is_err());
    }

    #[test]
    fn unsupported_worker_abi_is_rejected_before_session_creation() {
        let mut snapshot = snapshot();
        snapshot.worker.rbe_abi_min = LIBRARY_ABI_VERSION + 1;
        snapshot.worker.rbe_abi_max = LIBRARY_ABI_VERSION + 1;
        let error = expected_worker(&snapshot).unwrap_err();
        assert!(error.to_string().contains("supports RBE ABI"));
    }

    #[test]
    fn snapshot_identity_drift_is_rejected() {
        let mut snapshot = snapshot();
        snapshot.worker.artifact_sha256 = "b".repeat(64);
        let error = expected_worker(&snapshot).unwrap_err();
        assert!(error.to_string().contains("identity drifted"));
    }
}
