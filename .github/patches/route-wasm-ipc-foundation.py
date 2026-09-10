from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"missing anchor in {path}: {old[:120]!r}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# ---------------------------------------------------------------------------
# IPC v3: an execution input is no longer overloaded as a WASM artifact.
# Artifacts have an explicit authenticated registration operation.
# ---------------------------------------------------------------------------
protocol = Path("container-runtime/crates/ipc-protocol/src/lib.rs")
text = protocol.read_text(encoding="utf-8")
text = text.replace("pub const PROTOCOL_VERSION: u16 = 2;", "pub const PROTOCOL_VERSION: u16 = 3;", 1)
text = text.replace(
    "const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;",
    "const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;\n"
    "pub const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;\n"
    "pub const MAX_EXECUTION_INPUT_BYTES: usize = 2 * 1024 * 1024;",
    1,
)
old = """#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub request_id: String,
    pub auth_token: String,
    pub environment: String,
    pub artifact_hash: String,
    pub declared_cost: WorkCost,
    pub payload: Vec<u8>,
}
"""
new = """#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterArtifactRequest {
    pub request_id: String,
    pub auth_token: String,
    pub artifact_hash: String,
    pub wasm: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub request_id: String,
    pub auth_token: String,
    pub environment: String,
    pub artifact_hash: String,
    pub declared_cost: WorkCost,
    /// Invocation data for an already-registered artifact. This field must
    /// never be interpreted as executable bytes.
    pub input: Vec<u8>,
}
"""
if old not in text:
    raise SystemExit("ipc ExecuteRequest anchor missing")
text = text.replace(old, new, 1)
text = text.replace(
    "    Hello(Hello),\n    Execute(ExecuteRequest),",
    "    Hello(Hello),\n    RegisterArtifact(RegisterArtifactRequest),\n    Execute(ExecuteRequest),",
    1,
)
text = text.replace(
    """    HelloAccepted {
        version: u16,
    },
    Accepted {
""",
    """    HelloAccepted {
        version: u16,
    },
    ArtifactRegistered {
        request_id: String,
        artifact_hash: String,
        already_present: bool,
    },
    Accepted {
""",
    1,
)
insert = r'''

    #[test]
    fn artifact_registration_round_trip_is_distinct_from_execution_input() {
        let request = Request::RegisterArtifact(RegisterArtifactRequest {
            request_id: "artifact-1".into(),
            auth_token: "secret".into(),
            artifact_hash: "ab".repeat(32),
            wasm: vec![0, 97, 115, 109],
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let decoded = decode_request(&read_frame(&mut bytes.as_slice()).unwrap()).unwrap();
        assert!(matches!(decoded, Request::RegisterArtifact(_)));

        let execute = Request::Execute(ExecuteRequest {
            request_id: "exec-1".into(),
            auth_token: "secret".into(),
            environment: "general-1".into(),
            artifact_hash: "ab".repeat(32),
            declared_cost: WorkCost {
                cpu: 1,
                memory: 1,
                io: 0,
                network: 0,
            },
            input: b"request-body".to_vec(),
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &execute).unwrap();
        let decoded = decode_request(&read_frame(&mut bytes.as_slice()).unwrap()).unwrap();
        let Request::Execute(decoded) = decoded else {
            panic!("expected execute request");
        };
        assert_eq!(decoded.input, b"request-body");
    }
'''
marker = "\n    #[test]\n    fn rejects_zero_length_frame() {"
if marker not in text:
    raise SystemExit("ipc test insertion anchor missing")
text = text.replace(marker, insert + marker, 1)
protocol.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Runtime: payload is invocation input only. Artifact registration verifies the
# caller's SHA-256 claim instead of silently accepting/re-keying mismatches.
# ---------------------------------------------------------------------------
runtime = Path("container-runtime/crates/container-runtime-core/src/runtime.rs")
text = runtime.read_text(encoding="utf-8")
old = """        let claimed_artifact_hash = artifact_hash.into();
        let artifact_hash = if payload.is_empty() {
            claimed_artifact_hash
        } else {
            let computed = hex::encode(Sha256::digest(&payload));
            if !claimed_artifact_hash.is_empty() && claimed_artifact_hash != computed {
                tracing::warn!(claimed = %claimed_artifact_hash, computed = %computed, "artifact hash did not match payload; using content hash");
            }
            self.cache.put_artifact(computed.clone(), payload);
            computed
        };
        let id = ExecutionId::new(self.next_execution.fetch_add(1, Ordering::Relaxed));
"""
new = """        let artifact_hash = artifact_hash.into();
        let id = ExecutionId::new(self.next_execution.fetch_add(1, Ordering::Relaxed));
"""
if old not in text:
    raise SystemExit("runtime overloaded payload anchor missing")
text = text.replace(old, new, 1)
old = """                    work_ms,
                    payload: Vec::new(),
                },
"""
new = """                    work_ms,
                    payload,
                },
"""
if old not in text:
    raise SystemExit("runtime payload clearing anchor missing")
text = text.replace(old, new, 1)
old = """    pub fn register_artifact(&self, artifact_hash: impl Into<String>, wasm: Vec<u8>) {
        let claimed = artifact_hash.into();
        let computed = hex::encode(Sha256::digest(&wasm));
        if !claimed.is_empty() && claimed != computed {
            tracing::warn!(claimed = %claimed, computed = %computed, "registered artifact hash did not match content; storing by content hash only");
        }
        self.cache.put_artifact(computed, wasm);
    }
"""
new = """    /// Register immutable WASM under its canonical SHA-256 identity.
    /// Returns whether the exact artifact was already present.
    pub fn register_artifact(&self, artifact_hash: &str, wasm: Vec<u8>) -> Result<bool, String> {
        if artifact_hash.len() != 64
            || !artifact_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err("artifact hash must be lowercase SHA-256 hexadecimal".into());
        }
        let computed = hex::encode(Sha256::digest(&wasm));
        if artifact_hash != computed {
            return Err(format!(
                "artifact SHA-256 mismatch: claimed {artifact_hash}, computed {computed}"
            ));
        }
        let already_present = self.cache.contains_artifact(artifact_hash);
        if !already_present {
            self.cache.put_artifact(computed, wasm);
        }
        Ok(already_present)
    }
"""
if old not in text:
    raise SystemExit("runtime register_artifact anchor missing")
text = text.replace(old, new, 1)
runtime.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Container control plane: authenticated artifact registration, strict size
# bounds, and refusal to execute an artifact that was never registered.
# ---------------------------------------------------------------------------
container = Path("container-runtime/crates/container-bin/src/main.rs")
text = container.read_text(encoding="utf-8")
text = text.replace(
    "use ipc_protocol::{decode_request, read_frame, write_frame, Request, Response, PROTOCOL_VERSION};",
    "use ipc_protocol::{\n"
    "    decode_request, read_frame, write_frame, Request, Response, MAX_ARTIFACT_BYTES,\n"
    "    MAX_EXECUTION_INPUT_BYTES, PROTOCOL_VERSION,\n"
    "};",
    1,
)
anchor = """        Request::Execute(request) => {
            if request.auth_token != token {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else if !accepting.load(Ordering::Acquire) {
"""
replacement = """        Request::RegisterArtifact(request) => {
            if request.auth_token != token {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else if request.wasm.is_empty() || request.wasm.len() > MAX_ARTIFACT_BYTES {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "ARTIFACT_SIZE_INVALID".into(),
                    message: format!(
                        "WASM artifact must be between 1 and {MAX_ARTIFACT_BYTES} bytes"
                    ),
                }
            } else {
                match runtime.register_artifact(&request.artifact_hash, request.wasm) {
                    Ok(already_present) => Response::ArtifactRegistered {
                        request_id: request.request_id,
                        artifact_hash: request.artifact_hash,
                        already_present,
                    },
                    Err(message) => Response::Error {
                        request_id: Some(request.request_id),
                        code: "ARTIFACT_HASH_MISMATCH".into(),
                        message,
                    },
                }
            }
        }
        Request::Execute(request) => {
            if request.auth_token != token {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else if request.input.len() > MAX_EXECUTION_INPUT_BYTES {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "EXECUTION_INPUT_TOO_LARGE".into(),
                    message: format!(
                        "execution input exceeds {MAX_EXECUTION_INPUT_BYTES} bytes"
                    ),
                }
            } else if !runtime.cache().contains_artifact(&request.artifact_hash) {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "ARTIFACT_NOT_FOUND".into(),
                    message: "execution artifact is not registered".into(),
                }
            } else if !accepting.load(Ordering::Acquire) {
"""
if anchor not in text:
    raise SystemExit("container execute branch anchor missing")
text = text.replace(anchor, replacement, 1)
if "                    request.payload,\n" not in text:
    raise SystemExit("container request.payload anchor missing")
text = text.replace("                    request.payload,\n", "                    request.input,\n", 1)
container.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Backend ContainerClient: expose explicit registration and execution. These
# methods intentionally do not pretend an Accepted response is a result; the
# result/guest ABI lands in the next WASM runtime wave.
# ---------------------------------------------------------------------------
client = Path("engine/crates/core/src/container_client.rs")
text = client.read_text(encoding="utf-8")
old = """use ipc_protocol::{
    decode_response, read_frame, write_frame, HealthRequest, InspectRequest, PrepareRefreshRequest,
    Request, Response, ResumeRequest,
};
"""
new = """use ipc_protocol::{
    decode_response, read_frame, write_frame, ExecuteRequest, HealthRequest, InspectRequest,
    PrepareRefreshRequest, RegisterArtifactRequest, Request, Response, ResumeRequest,
    WorkCost as IpcWorkCost, MAX_ARTIFACT_BYTES, MAX_EXECUTION_INPUT_BYTES,
};
"""
if old not in text:
    raise SystemExit("ContainerClient import anchor missing")
text = text.replace(old, new, 1)
anchor = """    pub async fn health(&self) -> anyhow::Result<serde_json::Value> {
"""
methods = """    pub async fn register_artifact(
        &self,
        artifact_hash: &str,
        wasm: Vec<u8>,
    ) -> anyhow::Result<bool> {
        if wasm.is_empty() || wasm.len() > MAX_ARTIFACT_BYTES {
            anyhow::bail!("WASM artifact size is outside the Container IPC limit");
        }
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::RegisterArtifact(RegisterArtifactRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            artifact_hash: artifact_hash.to_string(),
            wasm,
        });
        match call(endpoint, request, Duration::from_secs(10)).await? {
            Response::ArtifactRegistered {
                already_present, ..
            } => Ok(already_present),
            Response::Error { code, message, .. } => {
                anyhow::bail!("container artifact registration failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected artifact registration response: {other:?}"),
        }
    }

    pub async fn execute(
        &self,
        environment: &str,
        artifact_hash: &str,
        input: Vec<u8>,
        declared_cost: IpcWorkCost,
    ) -> anyhow::Result<String> {
        if input.len() > MAX_EXECUTION_INPUT_BYTES {
            anyhow::bail!("Container execution input exceeds the IPC limit");
        }
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::Execute(ExecuteRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            environment: environment.to_string(),
            artifact_hash: artifact_hash.to_string(),
            declared_cost,
            input,
        });
        match call(endpoint, request, Duration::from_secs(5)).await? {
            Response::Accepted { execution_id, .. } => Ok(execution_id),
            Response::Error { code, message, .. } => {
                anyhow::bail!("container execution submission failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected container execute response: {other:?}"),
        }
    }

"""
if anchor not in text:
    raise SystemExit("ContainerClient health anchor missing")
text = text.replace(anchor, methods + anchor, 1)
client.write_text(text, encoding="utf-8")

print("route WASM IPC foundation applied")
