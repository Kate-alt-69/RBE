from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"missing anchor in {path}: {old[:180]!r}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# Compiler generations are part of both disk-cache validity and immutable
# Runtime Image identity. Codegen changes must bump this even when the worker ABI
# itself remains compatible.
replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    "pub const ROUTE_WASM_ABI_VERSION: u32 = 1;\nconst MAX_STATIC_OUTPUT_BYTES",
    "pub const ROUTE_WASM_ABI_VERSION: u32 = 1;\n"
    "pub const ROUTE_WASM_COMPILER_VERSION: u32 = 1;\n"
    "const MAX_STATIC_OUTPUT_BYTES",
)

replace_once(
    "engine/crates/route-engine/src/lib.rs",
    "    ROUTE_WASM_ABI_VERSION,\n};",
    "    ROUTE_WASM_ABI_VERSION, ROUTE_WASM_COMPILER_VERSION,\n};",
)

# Cache entries from a different compiler generation are not reusable even when
# source bytes and host ABI version happen to match.
cache = Path("engine/crates/route-engine/src/cache.rs")
text = cache.read_text(encoding="utf-8")
text = text.replace(
    "use crate::wasm_compiler::{compile_route, RouteWasmCompilation, ROUTE_WASM_ABI_VERSION};",
    "use crate::wasm_compiler::{\n"
    "    compile_route, RouteWasmCompilation, ROUTE_WASM_ABI_VERSION, ROUTE_WASM_COMPILER_VERSION,\n"
    "};",
    1,
)
text = text.replace("const CACHE_MANIFEST_VERSION: u64 = 2;", "const CACHE_MANIFEST_VERSION: u64 = 3;", 1)
old = """    if wasm.get("abi_version").and_then(serde_json::Value::as_u64)
        != Some(u64::from(ROUTE_WASM_ABI_VERSION))
    {
        return false;
    }
"""
new = """    if wasm.get("abi_version").and_then(serde_json::Value::as_u64)
        != Some(u64::from(ROUTE_WASM_ABI_VERSION))
        || wasm
            .get("compiler_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(ROUTE_WASM_COMPILER_VERSION))
    {
        return false;
    }
"""
if old not in text:
    raise SystemExit("cache compiler-version validity anchor missing")
text = text.replace(old, new, 1)
text = text.replace(
    '            "abi_version": ROUTE_WASM_ABI_VERSION,\n            "sha256": artifact.sha256,',
    '            "abi_version": ROUTE_WASM_ABI_VERSION,\n'
    '            "compiler_version": ROUTE_WASM_COMPILER_VERSION,\n'
    '            "sha256": artifact.sha256,',
    1,
)
text = text.replace(
    '            "abi_version": ROUTE_WASM_ABI_VERSION,\n            "reason": reason,',
    '            "abi_version": ROUTE_WASM_ABI_VERSION,\n'
    '            "compiler_version": ROUTE_WASM_COMPILER_VERSION,\n'
    '            "reason": reason,',
    1,
)
cache.write_text(text, encoding="utf-8")

# Runtime Image owns the exact validated native artifact bytes and their digest;
# no request path may reopen .cache or recompile source after activation.
runtime_image = Path("engine/crates/route-engine/src/runtime_image.rs")
text = runtime_image.read_text(encoding="utf-8")
text = text.replace(
    "use crate::source_registry::{RelSourceKind, SourceId};",
    "use crate::source_registry::{RelSourceKind, SourceId};\n"
    "use crate::wasm_compiler::{\n"
    "    RouteWasmArtifact, ROUTE_WASM_ABI_VERSION, ROUTE_WASM_COMPILER_VERSION,\n"
    "};",
    1,
)
text = text.replace(
    "    pub capabilities: BTreeMap<SourceId, BTreeSet<String>>,\n    pub executables: BTreeMap<SourceId, RuntimeExecutable>,",
    "    pub capabilities: BTreeMap<SourceId, BTreeSet<String>>,\n"
    "    /// Exact native route artifacts pinned at image-link time. These bytes\n"
    "    /// are the only route WASM payloads eligible for Container registration.\n"
    "    pub route_wasm_artifacts: BTreeMap<SourceId, RouteWasmArtifact>,\n"
    "    /// Routes outside the current native compiler subset remain explicit.\n"
    "    pub route_wasm_fallbacks: BTreeMap<SourceId, String>,\n"
    "    pub executables: BTreeMap<SourceId, RuntimeExecutable>,",
    1,
)
anchor = """    pub fn executable(&self, id: &SourceId) -> Option<&RuntimeExecutable> {
        self.executables.get(id)
    }

"""
insert = """    pub fn route_wasm_artifact(&self, id: &SourceId) -> Option<&RouteWasmArtifact> {
        self.route_wasm_artifacts.get(id)
    }

    pub fn route_wasm_fallback(&self, id: &SourceId) -> Option<&str> {
        self.route_wasm_fallbacks.get(id).map(String::as_str)
    }

"""
if anchor not in text:
    raise SystemExit("RuntimeImage artifact method anchor missing")
text = text.replace(anchor, anchor + insert, 1)
old = """pub(crate) fn stable_image_hash(source_hash: u64, settings: &serde_json::Value) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    feed_hash(&mut hash, b"RBE_RUNTIME_IMAGE_V1");
    feed_hash(&mut hash, &source_hash.to_be_bytes());
    hash_json(&mut hash, settings);
    hash
}
"""
new = """pub(crate) fn stable_image_hash(source_hash: u64, settings: &serde_json::Value) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    feed_hash(&mut hash, b"RBE_RUNTIME_IMAGE_V2");
    feed_hash(&mut hash, &source_hash.to_be_bytes());
    feed_hash(&mut hash, &ROUTE_WASM_ABI_VERSION.to_be_bytes());
    feed_hash(&mut hash, &ROUTE_WASM_COMPILER_VERSION.to_be_bytes());
    hash_json(&mut hash, settings);
    hash
}
"""
if old not in text:
    raise SystemExit("Runtime Image identity anchor missing")
text = text.replace(old, new, 1)
runtime_image.write_text(text, encoding="utf-8")

# RELC compiles each already-validated RouteFile exactly once into either a
# pinned native artifact or an explicit fallback reason before image activation.
relc = Path("engine/crates/route-engine/src/relc.rs")
text = relc.read_text(encoding="utf-8")
text = text.replace(
    "use crate::source_registry::{RelSourceKind, RelSourceRegistry, SourceId, SourceRegistryError};",
    "use crate::source_registry::{RelSourceKind, RelSourceRegistry, SourceId, SourceRegistryError};\n"
    "use crate::wasm_compiler::{compile_route, RouteWasmCompilation};",
    1,
)
anchor = """    let executables = compiled
        .iter()
        .map(|(id, unit)| (id.clone(), unit.runtime_executable()))
        .collect::<BTreeMap<_, _>>();

"""
insert = """    let mut route_wasm_artifacts = BTreeMap::new();
    let mut route_wasm_fallbacks = BTreeMap::new();
    for (id, unit) in &compiled {
        let CompiledUnit::Route(file) = unit else {
            continue;
        };
        match compile_route(file) {
            RouteWasmCompilation::Native(artifact) => {
                route_wasm_artifacts.insert(id.clone(), artifact);
            }
            RouteWasmCompilation::InterpreterFallback { reason } => {
                route_wasm_fallbacks.insert(id.clone(), reason);
            }
        }
    }

"""
if anchor not in text:
    raise SystemExit("RELC executable collection anchor missing")
text = text.replace(anchor, anchor + insert, 1)
text = text.replace(
    "        capabilities,\n        executables,",
    "        capabilities,\n        route_wasm_artifacts,\n        route_wasm_fallbacks,\n        executables,",
    1,
)
# Add an image-level contract test using a physical literal route and a dynamic
# route, proving both maps are keyed by immutable SourceId rather than file path.
marker = """    #[test]
    fn runtime_image_id_changes_when_settings_change() {
"""
test = r'''    #[test]
    fn runtime_image_pins_native_route_artifacts_and_explicit_fallbacks() {
        let routes = vec![
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "static",
                "api/static.route",
                "class Route { get(req) { return { ok: true }; } }",
            ),
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "dynamic",
                "api/dynamic.route",
                "class Route { post(req) { return req.body; } }",
            ),
        ];
        let image = compile_runtime_image("server Main {}", routes, &serde_json::json!({})).unwrap();
        let static_id = image
            .routes
            .iter()
            .find(|id| image.source(id).is_some_and(|source| source.logical_name == "static"))
            .unwrap();
        let dynamic_id = image
            .routes
            .iter()
            .find(|id| image.source(id).is_some_and(|source| source.logical_name == "dynamic"))
            .unwrap();

        let artifact = image.route_wasm_artifact(static_id).unwrap();
        assert_eq!(artifact.verb, "get");
        assert_eq!(artifact.sha256.len(), 64);
        assert_eq!(&artifact.bytes[..4], b"\0asm");
        assert!(image.route_wasm_fallback(static_id).is_none());
        assert!(image.route_wasm_artifact(dynamic_id).is_none());
        assert!(image
            .route_wasm_fallback(dynamic_id)
            .unwrap()
            .contains("runtime REL evaluation"));
    }

'''
if marker not in text:
    raise SystemExit("RELC Runtime Image test anchor missing")
text = text.replace(marker, test + marker, 1)
relc.write_text(text, encoding="utf-8")

print("Runtime Image WASM artifact pinning applied")
