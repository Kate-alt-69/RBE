from pathlib import Path

path = Path("engine/crates/core/src/container_client.rs")
text = path.read_text(encoding="utf-8")


def replace_once(old: str, new: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one ContainerClient anchor, found {count}: {old[:120]!r}")
    text = text.replace(old, new, 1)


replace_once(
    '''#[derive(Debug, Clone)]
pub struct ContainerEndpointSnapshot {
    pub address: SocketAddr,
    pub pid: Option<u32>,
    pub generation: u64,
}
''',
    '''#[derive(Debug, Clone)]
pub struct ContainerEndpointSnapshot {
    pub address: SocketAddr,
    pub pid: Option<u32>,
    pub generation: u64,
}

/// Immutable caller identity used for capability registration and execution.
/// Environment generation is deliberately not part of this type because only
/// Container Controller is allowed to stamp the live generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerExecutionIdentity<'a> {
    pub runtime_image: &'a str,
    pub source_id: &'a str,
    pub environment: &'a str,
}
''',
)

replace_once(
    '''    pub async fn register_capability_manifest(
        &self,
        runtime_image: &str,
        source_id: &str,
        environment: &str,
        grants: Vec<CapabilityGrant>,
    ) -> anyhow::Result<u64> {''',
    '''    pub async fn register_capability_manifest(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        grants: Vec<CapabilityGrant>,
    ) -> anyhow::Result<u64> {''',
)

replace_once(
    '''    pub async fn execute(
        &self,
        runtime_image: &str,
        source_id: &str,
        environment: &str,
        artifact_hash: &str,
        input: Vec<u8>,
        declared_cost: IpcWorkCost,
    ) -> anyhow::Result<String> {''',
    '''    pub async fn execute(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        artifact_hash: &str,
        input: Vec<u8>,
        declared_cost: IpcWorkCost,
    ) -> anyhow::Result<String> {''',
)

replace_once(
    '''    pub async fn execute_and_wait(
        &self,
        runtime_image: &str,
        source_id: &str,
        environment: &str,
        artifact_hash: &str,
        input: Vec<u8>,
        declared_cost: IpcWorkCost,
        timeout: Duration,
    ) -> anyhow::Result<Vec<u8>> {''',
    '''    pub async fn execute_and_wait(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        artifact_hash: &str,
        input: Vec<u8>,
        declared_cost: IpcWorkCost,
        timeout: Duration,
    ) -> anyhow::Result<Vec<u8>> {''',
)

identity_fields = '''            runtime_image: runtime_image.to_string(),
            source_id: source_id.to_string(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: environment.to_string(),'''
identity_fields_new = '''            runtime_image: identity.runtime_image.to_string(),
            source_id: identity.source_id.to_string(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: identity.environment.to_string(),'''
count = text.count(identity_fields)
if count != 2:
    raise SystemExit(f"expected two loose Container identity field blocks, found {count}")
text = text.replace(identity_fields, identity_fields_new)

replace_once(
    '''            .execute(
                runtime_image,
                source_id,
                environment,
                artifact_hash,
                input,
                declared_cost,
            )''',
    '''            .execute(identity, artifact_hash, input, declared_cost)''',
)

# The public execution surface should no longer accept the identity components
# as independent arguments after this repair.
for signature in ["pub async fn execute(", "pub async fn execute_and_wait("]:
    start = text.index(signature)
    end = text.index(") -> anyhow::Result", start)
    block = text[start:end]
    for loose in ["runtime_image: &str", "source_id: &str", "environment: &str"]:
        if loose in block:
            raise SystemExit(f"loose execution identity survived in {signature}: {loose}")

path.write_text(text, encoding="utf-8")
