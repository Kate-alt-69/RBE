from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


broker = Path("engine/crates/core/src/network_broker.rs")
replace_once(
    broker,
    '''const PUBLIC_HTTP_MAX_TIMEOUT_MS: u64 = 10_000;''',
    '''pub const PUBLIC_HTTP_MAX_TIMEOUT_MS: u64 = 10_000;''',
    "public HTTP max timeout export",
)

core = Path("engine/crates/core/src/lib.rs")
replace_once(
    core,
    '''    call_public_http, PublicHttpError, PUBLIC_HTTP_REQUEST_MAX_BYTES,
    PUBLIC_HTTP_RESPONSE_MAX_BYTES, PUBLIC_HTTP_TARGET,
};''',
    '''    call_public_http, PublicHttpError, PUBLIC_HTTP_MAX_TIMEOUT_MS,
    PUBLIC_HTTP_REQUEST_MAX_BYTES, PUBLIC_HTTP_RESPONSE_MAX_BYTES, PUBLIC_HTTP_TARGET,
};''',
    "public HTTP timeout re-export",
)

client = Path("engine/crates/core/src/container_client.rs")
replace_once(
    client,
    '''    decode_response, read_frame, write_frame, AwaitResultRequest, CapabilityGrant, ExecuteRequest,
    HealthRequest, InspectRequest, PrepareRefreshRequest, RegisterArtifactRequest,
    RegisterCapabilityManifestRequest, Request, Response, ResumeRequest, WorkCost as IpcWorkCost,
    CAPABILITY_ABI_VERSION, MAX_ARTIFACT_BYTES, MAX_AWAIT_RESULT_MS, MAX_EXECUTION_INPUT_BYTES,
    MAX_EXECUTION_OUTPUT_BYTES,
};''',
    '''    decode_response, read_frame, write_frame, AwaitResultRequest, CancelRequest, CapabilityGrant,
    ExecuteRequest, HealthRequest, InspectRequest, PrepareRefreshRequest, RegisterArtifactRequest,
    RegisterCapabilityManifestRequest, Request, Response, ResumeRequest, WorkCost as IpcWorkCost,
    CAPABILITY_ABI_VERSION, MAX_ARTIFACT_BYTES, MAX_AWAIT_RESULT_MS, MAX_EXECUTION_INPUT_BYTES,
    MAX_EXECUTION_OUTPUT_BYTES,
};''',
    "Container CancelRequest import",
)
replace_once(
    client,
    '''    pub async fn execute_and_wait(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        artifact_hash: &str,
        input: Vec<u8>,
        declared_cost: IpcWorkCost,
        timeout: Duration,
    ) -> anyhow::Result<Vec<u8>> {
        let execution_id = self
            .execute(identity, artifact_hash, input, declared_cost)
            .await?;
        match self.await_result(&execution_id, timeout).await? {
            Some(output) => Ok(output),
            None => {
                anyhow::bail!("container execution {execution_id} is still pending after timeout")
            }
        }
    }
''',
    '''    pub async fn cancel_execution(&self, execution_id: &str) -> anyhow::Result<bool> {
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request_id = next_request_id();
        let request = Request::Cancel(CancelRequest {
            request_id: request_id.clone(),
            auth_token: endpoint.token.clone(),
            execution_id: execution_id.to_string(),
        });
        match call(endpoint, request, Duration::from_secs(3)).await? {
            Response::Cancelled {
                request_id: returned,
            } if returned == request_id => Ok(true),
            Response::Cancelled { .. } => {
                anyhow::bail!("Container returned a mismatched cancellation response")
            }
            Response::Error {
                request_id: Some(returned),
                code,
                ..
            } if returned == request_id && code == "NOT_FOUND" => Ok(false),
            Response::Error {
                request_id: Some(returned),
                code,
                message,
            } if returned == request_id => {
                anyhow::bail!("container cancellation failed [{code}]: {message}")
            }
            Response::Error { .. } => {
                anyhow::bail!("Container returned a mismatched cancellation error")
            }
            other => anyhow::bail!("unexpected container cancellation response: {other:?}"),
        }
    }

    pub async fn execute_and_wait(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        artifact_hash: &str,
        input: Vec<u8>,
        declared_cost: IpcWorkCost,
        timeout: Duration,
    ) -> anyhow::Result<Vec<u8>> {
        let execution_id = self
            .execute(identity, artifact_hash, input, declared_cost)
            .await?;
        match self.await_result(&execution_id, timeout).await? {
            Some(output) => Ok(output),
            None => match self.cancel_execution(&execution_id).await {
                Ok(true) => anyhow::bail!(
                    "container execution {execution_id} exceeded the caller timeout; cancellation was accepted"
                ),
                Ok(false) => {
                    // Completion can linearize immediately after AwaitResult
                    // reports pending and before Cancel reaches Controller. A
                    // NOT_FOUND cancellation therefore gets one short recovery
                    // read before we classify the synchronous call as timed out.
                    match self
                        .await_result(&execution_id, Duration::from_millis(100))
                        .await?
                    {
                        Some(output) => Ok(output),
                        None => anyhow::bail!(
                            "container execution {execution_id} exceeded the caller timeout and was no longer cancellable"
                        ),
                    }
                }
                Err(error) => anyhow::bail!(
                    "container execution {execution_id} exceeded the caller timeout and cancellation failed: {error}"
                ),
            },
        }
    }
''',
    "synchronous timeout cancellation",
)
replace_once(
    client,
    '''fn next_request_id() -> String {
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("backend-{}-{sequence}", std::process::id())
}
''',
    '''fn next_request_id() -> String {
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("backend-{}-{sequence}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn respond(stream: &mut TcpStream, response: &Response) {
        write_frame(stream, response).unwrap();
    }

    #[tokio::test]
    async fn execute_and_wait_cancels_execution_when_synchronous_deadline_expires() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let token = "test-container-token".to_string();
        let server_token = token.clone();
        let server = std::thread::spawn(move || {
            for step in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let frame = read_frame(&mut stream).unwrap();
                let request: Request = serde_json::from_slice(&frame).unwrap();
                match (step, request) {
                    (0, Request::Execute(request)) => {
                        assert_eq!(request.auth_token, server_token);
                        respond(
                            &mut stream,
                            &Response::Accepted {
                                request_id: request.request_id,
                                execution_id: "exec-timeout".into(),
                            },
                        );
                    }
                    (1, Request::AwaitResult(request)) => {
                        assert_eq!(request.auth_token, server_token);
                        assert_eq!(request.execution_id, "exec-timeout");
                        respond(
                            &mut stream,
                            &Response::ExecutionPending {
                                request_id: request.request_id,
                                execution_id: request.execution_id,
                            },
                        );
                    }
                    (2, Request::Cancel(request)) => {
                        assert_eq!(request.auth_token, server_token);
                        assert_eq!(request.execution_id, "exec-timeout");
                        respond(
                            &mut stream,
                            &Response::Cancelled {
                                request_id: request.request_id,
                            },
                        );
                    }
                    (_, other) => panic!("unexpected mock Container request: {other:?}"),
                }
            }
        });

        let client = ContainerClient::new(address, token, None);
        let error = client
            .execute_and_wait(
                ContainerExecutionIdentity {
                    runtime_image: "runtime-image",
                    source_id: "route:test",
                    environment: "general-1",
                },
                "artifact",
                Vec::new(),
                IpcWorkCost {
                    cpu: 1,
                    memory: 1,
                    io: 0,
                    network: 0,
                },
                Duration::from_millis(1),
            )
            .await
            .expect_err("pending synchronous execution must be cancelled");
        assert!(error.to_string().contains("cancellation was accepted"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn cancellation_not_found_recovers_completion_race() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let token = "test-container-token".to_string();
        let server_token = token.clone();
        let server = std::thread::spawn(move || {
            for step in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let frame = read_frame(&mut stream).unwrap();
                let request: Request = serde_json::from_slice(&frame).unwrap();
                match (step, request) {
                    (0, Request::Execute(request)) => respond(
                        &mut stream,
                        &Response::Accepted {
                            request_id: request.request_id,
                            execution_id: "exec-race".into(),
                        },
                    ),
                    (1, Request::AwaitResult(request)) => respond(
                        &mut stream,
                        &Response::ExecutionPending {
                            request_id: request.request_id,
                            execution_id: request.execution_id,
                        },
                    ),
                    (2, Request::Cancel(request)) => {
                        assert_eq!(request.auth_token, server_token);
                        respond(
                            &mut stream,
                            &Response::Error {
                                request_id: Some(request.request_id),
                                code: "NOT_FOUND".into(),
                                message: "execution already completed".into(),
                            },
                        );
                    }
                    (3, Request::AwaitResult(request)) => respond(
                        &mut stream,
                        &Response::ExecutionFinished {
                            request_id: request.request_id,
                            execution_id: request.execution_id,
                            output: b"done".to_vec(),
                            elapsed_ms: 2,
                        },
                    ),
                    (_, other) => panic!("unexpected mock Container request: {other:?}"),
                }
            }
        });

        let client = ContainerClient::new(address, token, None);
        let output = client
            .execute_and_wait(
                ContainerExecutionIdentity {
                    runtime_image: "runtime-image",
                    source_id: "route:test",
                    environment: "general-1",
                },
                "artifact",
                Vec::new(),
                IpcWorkCost {
                    cpu: 1,
                    memory: 1,
                    io: 0,
                    network: 0,
                },
                Duration::from_millis(1),
            )
            .await
            .unwrap();
        assert_eq!(output, b"done");
        server.join().unwrap();
    }
}
''',
    "Container timeout cancellation tests",
)

discovery = Path("engine/crates/route-engine/src/discovery.rs")
replace_once(
    discovery,
    '''    AppState, ContainerAuthorizedExecution, ContainerCapabilityKind, ContainerExecutionIdentity,
    ContainerWorkCost, CONTAINER_MAX_EXECUTION_INPUT_BYTES,
};''',
    '''    AppState, ContainerAuthorizedExecution, ContainerCapabilityKind, ContainerExecutionIdentity,
    ContainerWorkCost, CONTAINER_MAX_EXECUTION_INPUT_BYTES, PUBLIC_HTTP_MAX_TIMEOUT_MS,
};''',
    "native broker timeout import",
)
replace_once(
    discovery,
    '''const NATIVE_ROUTE_ENVIRONMENT_PROFILE: &str = "general";
const NATIVE_ROUTE_TIMEOUT: Duration = Duration::from_secs(10);''',
    '''const NATIVE_ROUTE_ENVIRONMENT_PROFILE: &str = "general";
const NATIVE_ROUTE_TIMEOUT_HEADROOM_MS: u64 = 2_000;
const NATIVE_ROUTE_TIMEOUT: Duration = Duration::from_millis(
    PUBLIC_HTTP_MAX_TIMEOUT_MS.saturating_add(NATIVE_ROUTE_TIMEOUT_HEADROOM_MS),
);''',
    "native route broker headroom",
)
replace_once(
    discovery,
    '''    #[test]
    fn response_descriptor_becomes_real_http_response() {''',
    '''    #[test]
    fn native_route_wait_budget_exceeds_public_http_broker_timeout() {
        assert!(
            NATIVE_ROUTE_TIMEOUT.as_millis() > u128::from(PUBLIC_HTTP_MAX_TIMEOUT_MS),
            "outer Container wait must leave IPC/cancellation headroom after broker timeout"
        );
        assert_eq!(
            NATIVE_ROUTE_TIMEOUT.as_millis(),
            u128::from(PUBLIC_HTTP_MAX_TIMEOUT_MS + NATIVE_ROUTE_TIMEOUT_HEADROOM_MS)
        );
    }

    #[test]
    fn response_descriptor_becomes_real_http_response() {''',
    "native timeout relationship test",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''Routes outside the native compiler subset continue through the linked REL evaluator using their explicit fallback reason. Once a route is native, Container admission/execution failure is fail-closed and does **not** silently fall back to the in-process evaluator.''',
    '''Routes outside the native compiler subset continue through the linked REL evaluator using their explicit fallback reason. Once a route is native, Container admission/execution failure is fail-closed and does **not** silently fall back to the in-process evaluator. Synchronous native execution also has an outer wait budget larger than the public HTTP broker's maximum operation timeout; if that caller deadline is still exceeded, Backend sends authenticated Container cancellation instead of abandoning a live execution. A cancellation/completion race is recovered with one bounded final result read.''',
    "native timeout/cancellation documentation",
)
