from pathlib import Path

path = Path("container-runtime/crates/container-bin/src/main.rs")
text = path.read_text(encoding="utf-8")

def replace_once(old: str, new: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected exactly one anchor, got {count}: {old[:100]!r}")
    text = text.replace(old, new, 1)

replace_once(
    "use container_runtime_core::{\n    EnvironmentId, EnvironmentRegistry, Runtime, RuntimeConfig, WorkCost,\n};",
    "use container_runtime_core::{\n    CapabilityBroker, EnvironmentId, EnvironmentRegistry, Runtime, RuntimeConfig, WorkCost,\n};",
)
replace_once(
    "    MAX_EXECUTION_INPUT_BYTES, MAX_EXECUTION_OUTPUT_BYTES, PROTOCOL_VERSION,\n};",
    "    MAX_EXECUTION_INPUT_BYTES, MAX_EXECUTION_OUTPUT_BYTES, CAPABILITY_ABI_VERSION,\n    PROTOCOL_VERSION,\n};",
)
replace_once(
    "    let accepting = Arc::new(AtomicBool::new(true));",
    "    let capability_broker = Arc::new(CapabilityBroker::new(debug));\n    let accepting = Arc::new(AtomicBool::new(true));",
)
replace_once(
    "        run_control_server(&address, token, runtime.clone(), accepting)?;",
    "        run_control_server(\n            &address,\n            token,\n            runtime.clone(),\n            accepting,\n            capability_broker,\n        )?;",
)
replace_once(
    "fn run_control_server(\n    address: &str,\n    token: String,\n    runtime: Arc<Runtime>,\n    accepting: Arc<AtomicBool>,\n) -> anyhow::Result<()> {",
    "fn run_control_server(\n    address: &str,\n    token: String,\n    runtime: Arc<Runtime>,\n    accepting: Arc<AtomicBool>,\n    capability_broker: Arc<CapabilityBroker>,\n) -> anyhow::Result<()> {",
)
replace_once(
    "                let accepting = Arc::clone(&accepting);\n                thread::spawn(move || {\n                    if let Err(err) = handle_connection(stream, &token, &runtime, &accepting) {",
    "                let accepting = Arc::clone(&accepting);\n                let capability_broker = Arc::clone(&capability_broker);\n                thread::spawn(move || {\n                    if let Err(err) = handle_connection(\n                        stream,\n                        &token,\n                        &runtime,\n                        &accepting,\n                        &capability_broker,\n                    ) {",
)
replace_once(
    "fn handle_connection(\n    mut stream: TcpStream,\n    token: &str,\n    runtime: &Runtime,\n    accepting: &AtomicBool,\n) -> anyhow::Result<()> {",
    "fn handle_connection(\n    mut stream: TcpStream,\n    token: &str,\n    runtime: &Runtime,\n    accepting: &AtomicBool,\n    capability_broker: &CapabilityBroker,\n) -> anyhow::Result<()> {",
)
capability_arm = '''        Request::RegisterCapabilityManifest(request) => {
            if request.auth_token != token {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else {
                match capability_broker.register_manifest(&request) {
                    Ok(grants) => {
                        emit_event(
                            "capability_manifest_registered",
                            &format!(
                                "runtime_image={} source_id={} environment={} generation={} grants={grants}",
                                request.runtime_image,
                                request.source_id,
                                request.environment,
                                request.generation
                            ),
                        );
                        Response::CapabilityManifestRegistered {
                            request_id: request.request_id,
                            runtime_image: request.runtime_image,
                            source_id: request.source_id,
                            environment: request.environment,
                            generation: request.generation,
                            grants,
                        }
                    }
                    Err(error) => Response::Error {
                        request_id: Some(request.request_id),
                        code: error.code.into(),
                        message: error.message,
                    },
                }
            }
        }
'''
replace_once(
    "        Request::Execute(request) => {",
    capability_arm + "        Request::Execute(request) => {",
)
replace_once(
    "                let requeued = runtime.restart_environment(environment);\n                emit_event(\n                    \"environment_restart\",\n                    &format!(\"environment={environment} requeued={requeued}\"),\n                );",
    "                let previous_generation = runtime.environment_generation(environment);\n                let revoked = capability_broker\n                    .revoke_environment_generation(&request.environment, previous_generation);\n                let requeued = runtime.restart_environment(environment);\n                emit_event(\n                    \"environment_restart\",\n                    &format!(\n                        \"environment={environment} requeued={requeued} revoked_manifests={revoked}\"\n                    ),\n                );",
)
replace_once(
    '                        "protocol": PROTOCOL_VERSION,\n                        "process": "container",',
    '                        "protocol": PROTOCOL_VERSION,\n                        "capability_abi": CAPABILITY_ABI_VERSION,\n                        "capability_manifests": capability_broker.manifest_count(),\n                        "process": "container",',
)
path.write_text(text, encoding="utf-8")
