from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


# ---------------------------------------------------------------------------
# wasm_compiler.rs: expose strict method-level compilation while preserving the
# legacy whole-route helper as a one-method-only API.
# ---------------------------------------------------------------------------
path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()

text = replace_once(
    text,
    "use crate::ast::{BinaryOp, Expr, FunctionDef, ImportTarget, RouteFile, Statement, Value};",
    "use crate::ast::{BinaryOp, Expr, FunctionDef, ImportTarget, MethodDef, RouteFile, Statement, Value};",
    "wasm compiler MethodDef import",
)

text = replace_once(
    text,
    """/// Generation 12 resolves multiple exact Route imports by the binding the
/// method actually calls. This removes the former one-import compiler limit
/// without turning namespace imports or ambiguous bindings into wider authority.
/// Capability ABI v3 remains stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 12;""",
    """/// Generation 13 compiles each HTTP method independently so one `.route`
/// can pin distinct native artifacts and explicit fallbacks per verb. Exact
/// import binding, capability authority, and ABI v3 remain unchanged.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 13;""",
    "compiler generation 13",
)

text = replace_once(
    text,
    """/// Native lowering remains intentionally strict: one HTTP method. Compiler
/// generation v12 keeps ABI v3 and resolves any number of exact direct or
/// linked-function imports by the binding actually returned by the method. It
/// retains generation 11's static capability arguments and generation 10's
/// bounded helper folding. Namespace imports, ambiguous bindings, dynamic
/// transformations, wider Module bodies, and nested host-call chains remain
/// interpreter-only.""",
    """/// Native lowering remains intentionally strict per HTTP method. Compiler
/// generation v13 keeps ABI v3 and lets RELC compile every method in a Route
/// independently while retaining generation 12's exact import binding,
/// generation 11's static capability arguments, and generation 10's bounded
/// helper folding. Namespace imports, ambiguous bindings, dynamic transforms,
/// wider Module bodies, and nested host-call chains remain interpreter-only.""",
    "compiler generation contract",
)

text = replace_once(
    text,
    """pub(crate) fn compile_route_with_links(
    file: &RouteFile,
    links: &RouteWasmLinkContext,
) -> RouteWasmCompilation {
    let mut host_imports = BTreeMap::<String, DirectCapabilityImport>::new();""",
    """pub(crate) fn compile_route_with_links(
    file: &RouteFile,
    links: &RouteWasmLinkContext,
) -> RouteWasmCompilation {
    if file.methods.len() != 1 {
        return fallback(
            "whole-route native compilation requires exactly one HTTP method; RELC must compile multi-method Routes per verb",
        );
    }
    compile_route_method_with_links(file, links, &file.methods[0])
}

pub(crate) fn compile_route_method_with_links(
    file: &RouteFile,
    links: &RouteWasmLinkContext,
    method: &MethodDef,
) -> RouteWasmCompilation {
    let mut host_imports = BTreeMap::<String, DirectCapabilityImport>::new();""",
    "method-level compiler entrypoint",
)

text = replace_once(
    text,
    """    if file.methods.len() != 1 {
        return fallback("native route compilation currently requires exactly one HTTP method");
    }

    let method = &file.methods[0];
    let returned_binding = returned_call_binding(&method.body);""",
    """    let returned_binding = returned_call_binding(&method.body);""",
    "remove whole-route method gate from method compiler",
)

text = text.replace("native Route-WASM v12 found ambiguous import binding", "native Route-WASM v13 found ambiguous import binding")
text = text.replace("native Route-WASM v12 supports only exact direct", "native Route-WASM v13 supports only exact direct")

anchor = """    #[test]
    fn multiple_methods_are_not_collapsed_into_one_run_export() {"""
if anchor not in text:
    raise SystemExit("multi-method compiler test anchor missing")
new_tests = r'''    #[test]
    fn multi_method_route_compiles_each_method_independently() {
        let route = parse(
            r#"class Route {
                get(req) { return { ok: true, method: "get" }; }
                post(req) { return req.body; }
            }"#,
        );
        assert_eq!(route.methods.len(), 2);
        let links = RouteWasmLinkContext::default();

        let RouteWasmCompilation::Native(get) =
            compile_route_method_with_links(&route, &links, &route.methods[0])
        else {
            panic!("GET should compile independently");
        };
        assert_eq!(get.verb, "get");
        assert_eq!(get.input, RouteWasmInput::None);
        let get_result = WasmExecutor::new()
            .unwrap()
            .execute(&get.bytes, ExecutionLimits::default())
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&get_result.output).unwrap(),
            serde_json::json!({ "ok": true, "method": "get" })
        );

        let RouteWasmCompilation::Native(post) =
            compile_route_method_with_links(&route, &links, &route.methods[1])
        else {
            panic!("POST should compile independently");
        };
        assert_eq!(post.verb, "post");
        assert_eq!(post.input, RouteWasmInput::JsonBody);
        let post_result = WasmExecutor::new()
            .unwrap()
            .execute_with_input(
                &post.bytes,
                br#"{"value":42}"#,
                ExecutionLimits::default(),
            )
            .unwrap();
        assert_eq!(post_result.output, br#"{"value":42}"#);
    }

    #[test]
    fn multi_method_route_can_mix_native_and_explicit_fallback() {
        let route = parse(
            r#"class Route {
                get(req) { return true; }
                post(req) { return req.query; }
            }"#,
        );
        let links = RouteWasmLinkContext::default();
        assert!(compile_route_method_with_links(&route, &links, &route.methods[0]).is_native());
        let RouteWasmCompilation::InterpreterFallback { reason } =
            compile_route_method_with_links(&route, &links, &route.methods[1])
        else {
            panic!("unsupported POST should remain an explicit method fallback");
        };
        assert!(reason.contains("outside the native Route-WASM v3 subset"));
    }

'''
text = text.replace(anchor, new_tests + anchor, 1)
path.write_text(text)


# ---------------------------------------------------------------------------
# runtime_image.rs: artifact/fallback authority is now keyed by source + verb.
# ---------------------------------------------------------------------------
path = Path("engine/crates/route-engine/src/runtime_image.rs")
text = path.read_text()

text = replace_once(
    text,
    """    /// Exact native route artifacts pinned at image-link time. These bytes
    /// are the only route WASM payloads eligible for Container registration.
    pub route_wasm_artifacts: BTreeMap<SourceId, RouteWasmArtifact>,
    /// Routes outside the current native compiler subset remain explicit.
    pub route_wasm_fallbacks: BTreeMap<SourceId, String>,""",
    """    /// Exact native route artifacts pinned at image-link time, grouped by
    /// source and HTTP verb. These bytes are the only Route-WASM payloads
    /// eligible for Container registration.
    pub route_wasm_artifacts: BTreeMap<SourceId, BTreeMap<String, RouteWasmArtifact>>,
    /// Method-level compiler fallbacks remain explicit instead of collapsing an
    /// entire multi-method Route back to the evaluator.
    pub route_wasm_fallbacks: BTreeMap<SourceId, BTreeMap<String, String>>,""",
    "runtime image per-method artifact fields",
)

text = replace_once(
    text,
    """    pub fn route_wasm_artifact(&self, id: &SourceId) -> Option<&RouteWasmArtifact> {
        self.route_wasm_artifacts.get(id)
    }

    pub fn route_wasm_fallback(&self, id: &SourceId) -> Option<&str> {
        self.route_wasm_fallbacks.get(id).map(String::as_str)
    }""",
    """    pub fn route_wasm_artifact(
        &self,
        id: &SourceId,
        verb: &str,
    ) -> Option<&RouteWasmArtifact> {
        self.route_wasm_artifacts.get(id)?.get(verb)
    }

    pub fn route_wasm_fallback(&self, id: &SourceId, verb: &str) -> Option<&str> {
        self.route_wasm_fallbacks
            .get(id)?
            .get(verb)
            .map(String::as_str)
    }""",
    "runtime image per-method accessors",
)
path.write_text(text)


# ---------------------------------------------------------------------------
# relc.rs: compile each method independently, but preserve the source-wide
# Storage fail-closed rule because grants are still source-scoped.
# ---------------------------------------------------------------------------
path = Path("engine/crates/route-engine/src/relc.rs")
text = path.read_text()

text = replace_once(
    text,
    "use crate::wasm_compiler::{compile_route_with_links, RouteWasmCompilation, RouteWasmLinkContext};",
    "use crate::wasm_compiler::{compile_route_method_with_links, RouteWasmCompilation, RouteWasmLinkContext};",
    "RELC method compiler import",
)

text = replace_once(
    text,
    """    // PASS 7/8/9 are metadata/lowering boundaries in v1. The current evaluator
    // remains the executable representation while the image owns validated
    // policy, identities and graph information.""",
    """    // PASS 7/8/9 lower validated REL into executable Runtime Image state.
    // Route methods inside the native subset become immutable WASM artifacts;
    // unsupported methods retain an explicit evaluator fallback. The image owns
    // the exact policy, identities, graph, and executable representation.""",
    "RELC executable representation comment",
)

old_loop = r'''    let mut route_wasm_artifacts = BTreeMap::new();
    let mut route_wasm_fallbacks = BTreeMap::new();
    for (id, unit) in &compiled {
        let CompiledUnit::Route(file) = unit else {
            continue;
        };
        let link_context = route_wasm_link_context(&registry, &compiled, file);
        match compile_route_with_links(file, &link_context) {
            RouteWasmCompilation::Native(artifact) => {
                route_wasm_artifacts.insert(id.clone(), artifact);
            }
            RouteWasmCompilation::InterpreterFallback { reason } => {
                let requires_storage = capabilities.get(id).is_some_and(|requirements| {
                    requirements.iter().any(|requirement| {
                        matches!(requirement, RuntimeCapabilityRequirement::Storage { .. })
                    })
                });
                if requires_storage {
                    return Err(RelcError::Capability {
                        code: "RELC3001",
                        source: id.clone(),
                        message: format!(
                            "Environment Storage authority requires native Container execution; Route-WASM v7 could not lower this Route: {reason}"
                        ),
                    });
                }
                route_wasm_fallbacks.insert(id.clone(), reason);
            }
        }
    }
'''
new_loop = r'''    let mut route_wasm_artifacts = BTreeMap::new();
    let mut route_wasm_fallbacks = BTreeMap::new();
    for (id, unit) in &compiled {
        let CompiledUnit::Route(file) = unit else {
            continue;
        };
        let link_context = route_wasm_link_context(&registry, &compiled, file);
        let requires_storage = capabilities.get(id).is_some_and(|requirements| {
            requirements.iter().any(|requirement| {
                matches!(requirement, RuntimeCapabilityRequirement::Storage { .. })
            })
        });
        let mut method_artifacts = BTreeMap::new();
        let mut method_fallbacks = BTreeMap::new();
        let mut seen_verbs = BTreeSet::new();

        for method in &file.methods {
            if !seen_verbs.insert(method.verb.clone()) {
                return Err(RelcError::Link(format!(
                    "{id} declares duplicate Route method {:?}",
                    method.verb
                )));
            }
            match compile_route_method_with_links(file, &link_context, method) {
                RouteWasmCompilation::Native(artifact) => {
                    debug_assert_eq!(artifact.verb, method.verb);
                    method_artifacts.insert(method.verb.clone(), artifact);
                }
                RouteWasmCompilation::InterpreterFallback { reason } => {
                    // Storage authority is currently lowered at SourceId scope.
                    // Until grants become method-scoped, allowing any method of
                    // a Storage-capable Route to escape native execution would
                    // blur the capability boundary. Fail the image instead.
                    if requires_storage {
                        return Err(RelcError::Capability {
                            code: "RELC3001",
                            source: id.clone(),
                            message: format!(
                                "Environment Storage authority requires native Container execution; Route-WASM v13 could not lower method {:?}: {reason}",
                                method.verb
                            ),
                        });
                    }
                    method_fallbacks.insert(method.verb.clone(), reason);
                }
            }
        }

        if !method_artifacts.is_empty() {
            route_wasm_artifacts.insert(id.clone(), method_artifacts);
        }
        if !method_fallbacks.is_empty() {
            route_wasm_fallbacks.insert(id.clone(), method_fallbacks);
        }
    }
'''
text = replace_once(text, old_loop, new_loop, "RELC per-method Route-WASM lowering")

# Update existing single-method tests to the verb-aware Runtime Image API.
replacements = {
    "image.route_wasm_artifact(static_id).unwrap()": "image.route_wasm_artifact(static_id, \"get\").unwrap()",
    "image.route_wasm_fallback(static_id).is_none()": "image.route_wasm_fallback(static_id, \"get\").is_none()",
    "image.route_wasm_artifact(dynamic_id).is_none()": "image.route_wasm_artifact(dynamic_id, \"post\").is_none()",
    "route_wasm_fallback(dynamic_id)": "route_wasm_fallback(dynamic_id, \"post\")",
    "image.route_wasm_artifact(route).is_some()": "image.route_wasm_artifact(route, \"get\").is_some()",
    "image.route_wasm_fallback(route).is_none()": "image.route_wasm_fallback(route, \"get\").is_none()",
    ".route_wasm_artifact(route)\n            .expect(\"dynamic Storage route must compile to native WASM\")": ".route_wasm_artifact(route, \"post\")\n            .expect(\"dynamic Storage route must compile to native WASM\")",
}
for old, new in replacements.items():
    if old not in text:
        raise SystemExit(f"RELC test accessor anchor missing: {old!r}")
    text = text.replace(old, new)

anchor = """    #[test]
    fn direct_http_get_links_native_artifact_with_exact_network_requirement() {"""
if anchor not in text:
    raise SystemExit("RELC per-method tests anchor missing")
new_tests = r'''    #[test]
    fn runtime_image_pins_native_artifacts_and_fallbacks_per_method() {
        let routes = vec![PhysicalRelSource::new(
            RelSourceKind::Route,
            "mixed-methods",
            "api/mixed-methods.route",
            r#"class Route {
                get(req) { return { ok: true }; }
                post(req) { return req.query; }
            }"#,
        )];
        let image =
            compile_runtime_image("server Main {}", routes, &serde_json::json!({})).unwrap();
        let route = image.routes.first().unwrap();

        let get = image
            .route_wasm_artifact(route, "get")
            .expect("GET should have a native artifact");
        assert_eq!(get.verb, "get");
        assert!(image.route_wasm_fallback(route, "get").is_none());

        assert!(image.route_wasm_artifact(route, "post").is_none());
        assert!(image
            .route_wasm_fallback(route, "post")
            .is_some_and(|reason| reason.contains("outside the native Route-WASM v3 subset")));
    }

    #[test]
    fn storage_multi_method_route_fails_closed_if_any_method_falls_back() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Module,
                "cache",
                "module/cache.module",
                r#":import[storage.read as readEntry]
                   export function load(path) { return readEntry(path); }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "storage-mixed",
                "api/storage-mixed.route",
                r#":import["./module/cache".load]
                   class Route {
                       get(req) { return load("users/kate.json"); }
                       post(req) { return req.query; }
                   }"#,
            ),
        ];
        let error = compile_runtime_image("server Main {}", sources, &serde_json::json!({}))
            .expect_err("Storage-capable Route must not mix native and evaluator execution");
        assert_eq!(error.code(), "RELC3001");
        let rendered = error.to_string();
        assert!(rendered.contains("Route-WASM v13"));
        assert!(rendered.contains("post"));
    }

'''
text = text.replace(anchor, new_tests + anchor, 1)
path.write_text(text)


# ---------------------------------------------------------------------------
# discovery.rs: bind the native plan matching each Axum method instead of
# cloning one route-wide artifact into every handler.
# ---------------------------------------------------------------------------
path = Path("engine/crates/route-engine/src/discovery.rs")
text = path.read_text()

text = replace_once(
    text,
    """fn build_method_router(
    file: &RouteFile,
    module_program: Arc<ModuleProgram>,
    native_plan: Option<Arc<NativeRoutePlan>>,
) -> MethodRouter<AppState> {
    let mut router = MethodRouter::<AppState>::new();
    for method_def in &file.methods {""",
    """fn build_method_router(
    file: &RouteFile,
    module_program: Arc<ModuleProgram>,
    native_plans: Option<Arc<HashMap<String, Arc<NativeRoutePlan>>>>,
) -> MethodRouter<AppState> {
    let mut router = MethodRouter::<AppState>::new();
    for method_def in &file.methods {
        let native_plan = native_plans
            .as_ref()
            .and_then(|plans| plans.get(&method_def.verb))
            .cloned();""",
    "per-method router native plan selection",
)

old_plan = r'''        let native_plan = image.route_wasm_artifact(id).map(|artifact| {
            Arc::new(NativeRoutePlan {
                runtime_image: image.image_id.clone(),
                source_id: id.clone(),
                artifact: artifact.clone(),
            })
        });
        router = router.route(
            &url_path,
            build_method_router(route_file.as_ref(), module_program.clone(), native_plan),
        );
'''
new_plan = r'''        let mut native_plans = HashMap::new();
        for method in &route_file.methods {
            let Some(artifact) = image.route_wasm_artifact(id, &method.verb) else {
                continue;
            };
            if artifact.verb != method.verb {
                anyhow::bail!(
                    "Runtime Image route {id} method {:?} points at mismatched native artifact {:?}",
                    method.verb,
                    artifact.verb
                );
            }
            native_plans.insert(
                method.verb.clone(),
                Arc::new(NativeRoutePlan {
                    runtime_image: image.image_id.clone(),
                    source_id: id.clone(),
                    artifact: artifact.clone(),
                }),
            );
        }
        let native_plans = (!native_plans.is_empty()).then(|| Arc::new(native_plans));
        router = router.route(
            &url_path,
            build_method_router(route_file.as_ref(), module_program.clone(), native_plans),
        );
'''
text = replace_once(text, old_plan, new_plan, "Runtime Image per-method native plan construction")
path.write_text(text)
