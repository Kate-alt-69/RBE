from pathlib import Path


def one(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


path = Path("container-runtime/crates/container-bin/src/environment_process.rs")
text = path.read_text(encoding="utf-8")

text = one(
    text,
    '''use container_runtime_core::{
    Canceller, CapabilityBroker, CapabilityCall, EnvironmentId, EnvironmentProfile,
    EnvironmentStorageManager, ExecutionTask, Runner, DEFAULT_ENVIRONMENT_STORAGE_BYTES,
};''',
    '''use container_runtime_core::{
    dispatch_storage_capability, Canceller, CapabilityBroker, CapabilityCall, EnvironmentId,
    EnvironmentProfile, EnvironmentStorageManager, ExecutionProvenance, ExecutionTask, Runner,
    DEFAULT_ENVIRONMENT_STORAGE_BYTES,
};''',
    "Storage dispatcher imports",
)

text = one(
    text,
    'const CHILD_PROTOCOL_VERSION: u16 = 3;',
    'const CHILD_PROTOCOL_VERSION: u16 = 4;',
    "Environment child protocol version",
)

text = one(
    text,
    '''    Cancel {
        request_id: String,
        session: String,
        environment: String,
        generation: u64,
        execution_id: String,
    },
    CapabilityResult {''',
    '''    Cancel {
        request_id: String,
        session: String,
        environment: String,
        generation: u64,
        execution_id: String,
    },
    StorageCapability {
        request_id: String,
        session: String,
        execution_id: String,
        runtime_image: String,
        source_id: String,
        capability_abi: u16,
        environment: String,
        generation: u64,
        call: WorkerCapabilityCall,
        max_response_bytes: u64,
    },
    CapabilityResult {''',
    "Storage child request variant",
)

text = one(
    text,
    '''    CapabilityCall {
        request_id: String,
        call: WorkerCapabilityCall,
    },
    Error {''',
    '''    CapabilityCall {
        request_id: String,
        call: WorkerCapabilityCall,
    },
    StorageCapabilityResult {
        request_id: String,
        execution_id: String,
        result: WorkerCapabilityResult,
    },
    Error {''',
    "Storage child response variant",
)

text = one(
    text,
    '''        let request = CapabilityDispatchRequest {
            execution_id: task.id.to_string(),''',
    '''        if environment_owned_capability(call.kind) {
            return self.dispatch_environment_storage(
                task,
                provenance,
                call,
                authorized.max_response_bytes,
            );
        }

        let request = CapabilityDispatchRequest {
            execution_id: task.id.to_string(),''',
    "Controller Environment-owned capability branch",
)

spawn_marker = '''    fn spawn_one(&self, id: EnvironmentId, generation: u64) -> Result<ManagedEnvironment> {'''
if text.count(spawn_marker) != 1:
    raise SystemExit(f"spawn_one marker count={text.count(spawn_marker)}")
storage_method = r'''    fn dispatch_environment_storage(
        &self,
        task: &ExecutionTask,
        provenance: &ExecutionProvenance,
        call: WorkerCapabilityCall,
        max_response_bytes: u64,
    ) -> WorkerCapabilityResult {
        let call_id = call.call_id;
        let environment = match parse_environment(&provenance.environment) {
            Some(environment) => environment,
            None => {
                return capability_error(
                    call_id,
                    "CAPABILITY_ENVIRONMENT_INVALID",
                    "Storage capability provenance names an unknown Environment",
                )
            }
        };
        let endpoint = {
            let table = match self.processes.lock() {
                Ok(table) => table,
                Err(_) => {
                    return capability_error(
                        call_id,
                        "CAPABILITY_ENVIRONMENT_STATE_FAILED",
                        "Environment process table is unavailable",
                    )
                }
            };
            let Some(managed) = table.get(&environment) else {
                return capability_error(
                    call_id,
                    "CAPABILITY_ENVIRONMENT_UNAVAILABLE",
                    "owning Environment process is unavailable",
                );
            };
            if managed.endpoint.generation != provenance.generation {
                return capability_error(
                    call_id,
                    "CAPABILITY_ENVIRONMENT_STALE",
                    "Storage capability provenance targets a stale Environment generation",
                );
            }
            managed.endpoint.clone()
        };

        let mut stream = match TcpStream::connect_timeout(&endpoint.address, CONNECT_TIMEOUT) {
            Ok(stream) => stream,
            Err(_) => {
                return capability_error(
                    call_id,
                    "CAPABILITY_ENVIRONMENT_UNAVAILABLE",
                    "owning Environment process could not be reached",
                )
            }
        };
        let io_timeout = Duration::from_millis(task.limits.wall_time_ms.max(1).min(30_000));
        if stream.set_read_timeout(Some(io_timeout)).is_err()
            || stream.set_write_timeout(Some(CONNECT_TIMEOUT)).is_err()
        {
            return capability_error(
                call_id,
                "CAPABILITY_ENVIRONMENT_IO",
                "failed to configure Environment Storage channel",
            );
        }

        let execution_id = task.id.to_string();
        let request_id = format!("storage-{execution_id}-{call_id}");
        let request = ChildRequest::StorageCapability {
            request_id: request_id.clone(),
            session: endpoint.session,
            execution_id: execution_id.clone(),
            runtime_image: provenance.runtime_image.clone(),
            source_id: provenance.source_id.clone(),
            capability_abi: provenance.capability_abi,
            environment: provenance.environment.clone(),
            generation: provenance.generation,
            call,
            max_response_bytes,
        };
        if write_frame(&mut stream, &request).is_err() {
            return capability_error(
                call_id,
                "CAPABILITY_ENVIRONMENT_IO",
                "failed to send Environment Storage capability request",
            );
        }
        let response = match read_typed::<ChildResponse, _>(&mut BufReader::new(stream)) {
            Ok(response) => response,
            Err(_) => {
                return capability_error(
                    call_id,
                    "CAPABILITY_ENVIRONMENT_IO",
                    "failed to read Environment Storage capability response",
                )
            }
        };
        match response {
            ChildResponse::StorageCapabilityResult {
                request_id: returned_request,
                execution_id: returned_execution,
                result,
            } if returned_request == request_id
                && returned_execution == execution_id
                && capability_result_id(&result) == call_id =>
            {
                result
            }
            ChildResponse::Error {
                request_id: Some(returned_request),
                code,
                message,
            } if returned_request == request_id => capability_error(call_id, code, message),
            _ => capability_error(
                call_id,
                "CAPABILITY_ENVIRONMENT_PROTOCOL",
                "Environment Storage capability response identity did not match the request",
            ),
        }
    }

'''
text = text.replace(spawn_marker, storage_method + spawn_marker, 1)

cancel_tail = '''        ChildRequest::CapabilityResult { .. } => child_error(
            None,
            "CAPABILITY_RESULT_UNEXPECTED",
            "capability results are only accepted during an active execution",
        ),'''
if text.count(cancel_tail) != 1:
    raise SystemExit(f"child capability-result marker count={text.count(cancel_tail)}")
storage_branch = r'''        ChildRequest::StorageCapability {
            request_id,
            session,
            execution_id,
            runtime_image,
            source_id,
            capability_abi,
            environment,
            generation,
            call,
            max_response_bytes,
        } => {
            if !valid_session(bootstrap, &session) {
                child_error(
                    Some(request_id),
                    "AUTH_FAILED",
                    "Environment session rejected",
                )
            } else if environment != bootstrap.environment || generation != bootstrap.generation {
                child_error(
                    Some(request_id),
                    "ENVIRONMENT_IDENTITY_MISMATCH",
                    "Environment/generation does not match the child process",
                )
            } else if capability_abi != CAPABILITY_ABI_VERSION
                || !valid_runtime_image(&runtime_image)
                || !valid_source_id(&source_id)
            {
                child_error(
                    Some(request_id),
                    "CAPABILITY_PROVENANCE_INVALID",
                    "Storage capability provenance is invalid",
                )
            } else if call.kind != CapabilityKind::Storage {
                child_error(
                    Some(request_id),
                    "CAPABILITY_KIND_INVALID",
                    "Environment Storage channel accepts only Storage capabilities",
                )
            } else if max_response_bytes == 0
                || max_response_bytes > MAX_CAPABILITY_PAYLOAD_BYTES as u64
            {
                child_error(
                    Some(request_id),
                    "CAPABILITY_RESPONSE_LIMIT_INVALID",
                    "Storage capability response limit is invalid",
                )
            } else {
                match active_execution_identity_matches(
                    state,
                    &execution_id,
                    generation,
                    &runtime_image,
                    &source_id,
                    capability_abi,
                ) {
                    Err(_) => child_error(
                        Some(request_id),
                        "CAPABILITY_STATE_FAILED",
                        "Environment active execution state is unavailable",
                    ),
                    Ok(false) => child_error(
                        Some(request_id),
                        "CAPABILITY_EXECUTION_MISMATCH",
                        "Storage capability is not bound to the active execution provenance",
                    ),
                    Ok(true) => {
                        let result = match dispatch_storage_capability(
                            &state.storage,
                            &call.target,
                            &call.operation,
                            &call.payload,
                            max_response_bytes,
                        ) {
                            Ok(payload) => WorkerCapabilityResult::Success {
                                call_id: call.call_id,
                                payload,
                            },
                            Err(error) => WorkerCapabilityResult::Error {
                                call_id: call.call_id,
                                code: error.code.to_string(),
                                message: error.message,
                            },
                        };
                        ChildResponse::StorageCapabilityResult {
                            request_id,
                            execution_id,
                            result,
                        }
                    }
                }
            }
        }
'''
text = text.replace(cancel_tail, storage_branch + cancel_tail, 1)

cap_result_marker = '''fn capability_result_id(result: &WorkerCapabilityResult) -> u64 {'''
if text.count(cap_result_marker) != 1:
    raise SystemExit(f"capability_result_id marker count={text.count(cap_result_marker)}")
helpers = r'''fn environment_owned_capability(kind: CapabilityKind) -> bool {
    kind == CapabilityKind::Storage
}

fn active_execution_identity_matches(
    state: &EnvironmentChildState,
    execution_id: &str,
    generation: u64,
    runtime_image: &str,
    source_id: &str,
    capability_abi: u16,
) -> Result<bool, String> {
    let active = state
        .active_executions
        .lock()
        .map_err(|_| "Environment active execution table poisoned".to_string())?;
    Ok(active.get(execution_id).is_some_and(|identity| {
        identity.generation == generation
            && identity.runtime_image == runtime_image
            && identity.source_id == source_id
            && identity.capability_abi == capability_abi
    }))
}

fn capability_error(
    call_id: u64,
    code: impl Into<String>,
    message: impl Into<String>,
) -> WorkerCapabilityResult {
    WorkerCapabilityResult::Error {
        call_id,
        code: code.into(),
        message: message.into(),
    }
}

'''
text = text.replace(cap_result_marker, helpers + cap_result_marker, 1)

test_marker = '''    #[test]
    fn session_comparison_rejects_wrong_value() {'''
if text.count(test_marker) != 1:
    raise SystemExit(f"test insertion marker count={text.count(test_marker)}")
tests = r'''    fn test_storage_state(name: &str) -> (std::path::PathBuf, Arc<EnvironmentChildState>) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-environment-storage-transport-{name}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let storage = EnvironmentStorageManager::open(root.clone(), 4096).unwrap();
        (
            root,
            Arc::new(EnvironmentChildState {
                storage,
                active_executions: Mutex::new(HashMap::new()),
                cancelled: Mutex::new(HashMap::new()),
            }),
        )
    }

    fn child_round_trip(
        bootstrap: Arc<Bootstrap>,
        state: Arc<EnvironmentChildState>,
        request: ChildRequest,
    ) -> ChildResponse {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_child_connection(stream, &bootstrap, &state).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        write_frame(&mut client, &request).unwrap();
        let response = read_typed(&mut BufReader::new(client)).unwrap();
        server.join().unwrap();
        response
    }

    #[test]
    fn storage_capability_is_environment_owned() {
        assert!(environment_owned_capability(CapabilityKind::Storage));
        assert!(!environment_owned_capability(CapabilityKind::Service));
        assert!(!environment_owned_capability(CapabilityKind::Network));
    }

    #[test]
    fn storage_child_transport_commits_only_for_exact_active_provenance() {
        let (root, state) = test_storage_state("exact");
        let runtime_image = "ab".repeat(32);
        let source_id = "route:storage-test".to_string();
        let execution_id = "exec-storage-test".to_string();
        let generation = 7;
        state.active_executions.lock().unwrap().insert(
            execution_id.clone(),
            ActiveExecutionIdentity {
                generation,
                runtime_image: runtime_image.clone(),
                source_id: source_id.clone(),
                capability_abi: CAPABILITY_ABI_VERSION,
            },
        );
        let bootstrap = Arc::new(Bootstrap {
            version: CHILD_PROTOCOL_VERSION,
            environment: "general-1".into(),
            generation,
            storage_limit_bytes: 4096,
            debug: false,
            session: "ab".repeat(32),
        });
        let payload = serde_json::to_vec(&serde_json::json!([[
            {"op":"put", "path":"users/kate", "data_hex":"6b617465"}
        ]]))
        .unwrap();
        let call = WorkerCapabilityCall {
            call_id: 42,
            kind: CapabilityKind::Storage,
            target: "storage:uac".into(),
            operation: "commit".into(),
            payload,
        };
        let request = ChildRequest::StorageCapability {
            request_id: "storage-ok".into(),
            session: bootstrap.session.clone(),
            execution_id: execution_id.clone(),
            runtime_image: runtime_image.clone(),
            source_id: source_id.clone(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: bootstrap.environment.clone(),
            generation,
            call: call.clone(),
            max_response_bytes: 4096,
        };
        let response = child_round_trip(Arc::clone(&bootstrap), Arc::clone(&state), request);
        match response {
            ChildResponse::StorageCapabilityResult {
                request_id,
                execution_id: returned_execution,
                result: WorkerCapabilityResult::Success { call_id, .. },
            } => {
                assert_eq!(request_id, "storage-ok");
                assert_eq!(returned_execution, execution_id);
                assert_eq!(call_id, 42);
            }
            other => panic!("unexpected Storage response: {other:?}"),
        }
        assert_eq!(
            state.storage.read("uac", "users/kate").unwrap(),
            Some(b"kate".to_vec())
        );

        let stale_request = ChildRequest::StorageCapability {
            request_id: "storage-stale".into(),
            session: bootstrap.session.clone(),
            execution_id,
            runtime_image,
            source_id: "route:wrong-source".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: bootstrap.environment.clone(),
            generation,
            call,
            max_response_bytes: 4096,
        };
        let stale = child_round_trip(bootstrap, Arc::clone(&state), stale_request);
        match stale {
            ChildResponse::Error {
                request_id: Some(request_id),
                code,
                ..
            } => {
                assert_eq!(request_id, "storage-stale");
                assert_eq!(code, "CAPABILITY_EXECUTION_MISMATCH");
            }
            other => panic!("unexpected stale Storage response: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

'''
text = text.replace(test_marker, tests + test_marker, 1)

path.write_text(text, encoding="utf-8")
