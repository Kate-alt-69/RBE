from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


# Keep artifact registration, exact manifest registration, and execution behind
# one backend API so callers cannot accidentally execute an admitted artifact
# without first binding its RuntimeImage/SourceId capability authority.
client = Path("engine/crates/core/src/container_client.rs")
replace_once(
    client,
    '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerExecutionIdentity<'a> {
    pub runtime_image: &'a str,
    pub source_id: &'a str,
    pub environment: &'a str,
}

#[derive(Clone)]''',
    '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerExecutionIdentity<'a> {
    pub runtime_image: &'a str,
    pub source_id: &'a str,
    pub environment: &'a str,
}

#[derive(Debug)]
pub struct ContainerAuthorizedExecution<'a> {
    pub identity: ContainerExecutionIdentity<'a>,
    pub artifact_hash: &'a str,
    pub wasm: Vec<u8>,
    pub grants: Vec<CapabilityGrant>,
    pub input: Vec<u8>,
    pub declared_cost: IpcWorkCost,
    pub timeout: Duration,
}

#[derive(Clone)]''',
    "authorized execution request type",
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

    pub async fn health(&self)''',
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

    /// Admit and execute one immutable artifact under one exact capability
    /// identity. Registration is deliberately repeated/idempotent so an
    /// Environment generation restart cannot leave a stale backend-side cache
    /// authorizing work that Controller has already invalidated.
    pub async fn execute_authorized(
        &self,
        request: ContainerAuthorizedExecution<'_>,
    ) -> anyhow::Result<Vec<u8>> {
        self.register_artifact(request.artifact_hash, request.wasm)
            .await?;
        let generation = self
            .register_capability_manifest(request.identity, request.grants)
            .await?;
        tracing::debug!(
            runtime_image = request.identity.runtime_image,
            source_id = request.identity.source_id,
            environment = request.identity.environment,
            generation,
            artifact_hash = request.artifact_hash,
            "Container execution authority admitted"
        );
        self.execute_and_wait(
            request.identity,
            request.artifact_hash,
            request.input,
            request.declared_cost,
            request.timeout,
        )
        .await
    }

    pub async fn health(&self)''',
    "authorized execution method",
)

core_lib = Path("engine/crates/core/src/lib.rs")
replace_once(
    core_lib,
    '''pub use container_client::{ContainerClient, ContainerEndpointSnapshot};''',
    '''pub use container_client::{
    ContainerAuthorizedExecution, ContainerClient, ContainerEndpointSnapshot,
    ContainerExecutionIdentity,
};
pub use ipc_protocol::{
    CapabilityGrant as ContainerCapabilityGrant, CapabilityKind as ContainerCapabilityKind,
    WorkCost as ContainerWorkCost,
};''',
    "core Container admission exports",
)

# Runtime Image routers bind exact source/image identity into each native handler.
discovery = Path("engine/crates/route-engine/src/discovery.rs")
replace_once(
    discovery,
    '''use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};''',
    '''use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body};''',
    "route duration import",
)
replace_once(
    discovery,
    '''use core_lib::AppState;''',
    '''use core_lib::{
    AppState, ContainerAuthorizedExecution, ContainerExecutionIdentity, ContainerWorkCost,
};''',
    "route Container admission imports",
)
replace_once(
    discovery,
    '''use crate::server_rel::ServerProgram;
use crate::terminal::Terminal;''',
    '''use crate::server_rel::ServerProgram;
use crate::source_registry::SourceId;
use crate::terminal::Terminal;''',
    "route SourceId import",
)
replace_once(
    discovery,
    '''use crate::video_host::RuntimeHostCapabilities;''',
    '''use crate::video_host::RuntimeHostCapabilities;
use crate::wasm_compiler::RouteWasmArtifact;''',
    "route WASM artifact import",
)

insert_anchor = '''async fn execute(
    inline_file: Arc<ModuleFile>,'''
insert_block = '''const NATIVE_ROUTE_ENVIRONMENT: &str = "general-1";
const NATIVE_ROUTE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
struct NativeRoutePlan {
    runtime_image: String,
    source_id: SourceId,
    artifact: RouteWasmArtifact,
}

fn route_value_response(path: &str, value: Value) -> Response {
    match rel_http_response(&value) {
        Ok(Some(response)) => response,
        Ok(None) => Json(value_to_json(&value)).into_response(),
        Err(error) => {
            tracing::error!(error = %error, path = %path, "REL response descriptor rejected");
            append_runtime_error(path, &error);
            request_error(StatusCode::INTERNAL_SERVER_ERROR, error)
        }
    }
}

async fn execute_native_route(
    plan: &NativeRoutePlan,
    image: &RuntimeImage,
    state: &AppState,
    path: &str,
) -> Response {
    if image.image_id != plan.runtime_image {
        let error = "native Route-WASM identity no longer matches the active Runtime Image";
        tracing::error!(
            path = %path,
            expected_image = %plan.runtime_image,
            active_image = %image.image_id,
            source = %plan.source_id,
            "native route authority changed underneath the router"
        );
        append_runtime_error(path, error);
        return request_error(StatusCode::INTERNAL_SERVER_ERROR, error);
    }

    if image
        .capabilities
        .get(&plan.source_id)
        .is_some_and(|capabilities| !capabilities.is_empty())
    {
        let error = "native Route-WASM declares capabilities not lowered by the native compiler";
        tracing::error!(path = %path, source = %plan.source_id, "native route capability invariant failed");
        append_runtime_error(path, error);
        return request_error(StatusCode::INTERNAL_SERVER_ERROR, error);
    }

    let identity = ContainerExecutionIdentity {
        runtime_image: &plan.runtime_image,
        source_id: plan.source_id.as_str(),
        environment: NATIVE_ROUTE_ENVIRONMENT,
    };
    let output = match state
        .container
        .execute_authorized(ContainerAuthorizedExecution {
            identity,
            artifact_hash: &plan.artifact.sha256,
            wasm: plan.artifact.bytes.clone(),
            grants: Vec::new(),
            input: Vec::new(),
            declared_cost: ContainerWorkCost {
                cpu: 1,
                memory: 1,
                io: 0,
                network: 0,
            },
            timeout: NATIVE_ROUTE_TIMEOUT,
        })
        .await
    {
        Ok(output) => output,
        Err(error) => {
            tracing::error!(
                error = %error,
                path = %path,
                source = %plan.source_id,
                image = %plan.runtime_image,
                "native Route-WASM Container execution failed"
            );
            append_runtime_error(path, "native Route-WASM Container execution failed");
            return request_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "native route execution failed",
            );
        }
    };

    let value = match serde_json::from_slice::<serde_json::Value>(&output) {
        Ok(value) => json_to_value(value),
        Err(error) => {
            tracing::error!(
                error = %error,
                path = %path,
                source = %plan.source_id,
                "native Route-WASM returned invalid JSON"
            );
            append_runtime_error(path, "native Route-WASM returned invalid JSON");
            return request_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "native route returned an invalid result",
            );
        }
    };
    route_value_response(path, value)
}

async fn execute(
    inline_file: Arc<ModuleFile>,'''
replace_once(discovery, insert_anchor, insert_block, "native route execution helpers")

replace_once(
    discovery,
    '''    module_program: Arc<ModuleProgram>,
    takes_request: bool,
    state: AppState,''',
    '''    module_program: Arc<ModuleProgram>,
    native_plan: Option<Arc<NativeRoutePlan>>,
    takes_request: bool,
    state: AppState,''',
    "execute native plan argument",
)
replace_once(
    discovery,
    '''        Vec::new()
    };
    let executor = ModuleExecutor::with_services_and_host_capabilities(''',
    '''        Vec::new()
    };
    if let Some(plan) = native_plan.as_deref() {
        return execute_native_route(plan, image.as_ref(), &state, &path).await;
    }
    let executor = ModuleExecutor::with_services_and_host_capabilities(''',
    "native dispatch before evaluator",
)
replace_once(
    discovery,
    '''    match executor
        .call_inline(inline_file, INLINE_ROUTE_HANDLER, args)
        .await
    {
        Ok(value) => match rel_http_response(&value) {
            Ok(Some(response)) => response,
            Ok(None) => Json(value_to_json(&value)).into_response(),
            Err(error) => {
                tracing::error!(error = %error, path = %path, "REL response descriptor rejected");
                append_runtime_error(&path, &error);
                request_error(StatusCode::INTERNAL_SERVER_ERROR, error)
            }
        },''',
    '''    match executor
        .call_inline(inline_file, INLINE_ROUTE_HANDLER, args)
        .await
    {
        Ok(value) => route_value_response(&path, value),''',
    "shared route value rendering",
)
replace_once(
    discovery,
    '''fn build_method_router(
    file: &RouteFile,
    module_program: Arc<ModuleProgram>,
    _url_path: String,
) -> MethodRouter<AppState> {''',
    '''fn build_method_router(
    file: &RouteFile,
    module_program: Arc<ModuleProgram>,
    native_plan: Option<Arc<NativeRoutePlan>>,
) -> MethodRouter<AppState> {''',
    "method router native plan",
)
replace_once(
    discovery,
    '''        let module_program = module_program.clone();
        let verb = method_def.verb.clone();''',
    '''        let module_program = module_program.clone();
        let native_plan = native_plan.clone();
        let verb = method_def.verb.clone();''',
    "clone native plan into handler",
)
replace_once(
    discovery,
    '''                    inline_file,
                    module_program,
                    takes_request,''',
    '''                    inline_file,
                    module_program,
                    native_plan,
                    takes_request,''',
    "pass native plan to execute",
)
replace_once(
    discovery,
    '''            build_method_router(&route_file, module_program.clone(), url_path.clone()),''',
    '''            build_method_router(&route_file, module_program.clone(), None),''',
    "legacy disk router stays evaluator-only",
)
replace_once(
    discovery,
    '''        router = router.route(
            &url_path,
            build_method_router(
                route_file.as_ref(),
                module_program.clone(),
                url_path.clone(),
            ),
        );''',
    '''        let native_plan = image.route_wasm_artifact(id).map(|artifact| {
            Arc::new(NativeRoutePlan {
                runtime_image: image.image_id.clone(),
                source_id: id.clone(),
                artifact: artifact.clone(),
            })
        });
        router = router.route(
            &url_path,
            build_method_router(route_file.as_ref(), module_program.clone(), native_plan),
        );''',
    "Runtime Image native route plan",
)

# Update the current runtime contract; this is not a legacy document.
doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''The public HTTP route dispatcher still retains the linked REL evaluator path while native dispatch coverage expands. Native compilation existing in the image does not mean every HTTP request is already executed through the Container runtime.''',
    '''The public HTTP route dispatcher now executes a linked native Route-WASM artifact through the standalone Container runtime when that exact Runtime Image + SourceId has a native artifact. Admission registers the immutable artifact and an exact capability manifest before execution; the current native subset has no imports, so its manifest is intentionally empty.

Routes outside the native compiler subset continue through the linked REL evaluator using their explicit fallback reason. Once a route is native, Container admission/execution failure is fail-closed and does **not** silently fall back to the in-process evaluator.''',
    "Runtime Image native dispatch documentation",
)
