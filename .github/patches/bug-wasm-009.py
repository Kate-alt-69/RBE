from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()

text = replace_once(
    text,
    """/// Generation 11 lets request-independent REL expressions and local helpers
/// feed exact host-capability arguments while preserving the same Controller
/// authority boundary. Dynamic request-derived transformations remain outside
/// the native subset. Capability ABI v3 remains stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 11;""",
    """/// Generation 12 resolves multiple exact Route imports by the binding the
/// method actually calls. This removes the former one-import compiler limit
/// without turning namespace imports or ambiguous bindings into wider authority.
/// Capability ABI v3 remains stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 12;""",
    "compiler generation",
)

text = replace_once(
    text,
    """/// generation v11 keeps ABI v3, allows request-independent expressions and
/// local helpers to construct exact capability arguments, and retains generation
/// 10's bounded helper folding plus generation 8's immutable linked Module
/// authority path. Dynamic transformations, host-dependent helpers, wider
/// Module bodies, namespace imports, and nested host-call chains remain
/// interpreter-only.""",
    """/// generation v12 keeps ABI v3 and resolves any number of exact direct or
/// linked-function imports by the binding actually returned by the method. It
/// retains generation 11's static capability arguments and generation 10's
/// bounded helper folding. Namespace imports, ambiguous bindings, dynamic
/// transformations, wider Module bodies, and nested host-call chains remain
/// interpreter-only.""",
    "compiler contract",
)

old_imports = r'''    let mut host_import = None;
    let mut linked_import = None;
    match file.imports.as_slice() {
        [] => {}
        [import] => {
            if let Some(found) = direct_capability_import(import) {
                host_import = Some(found);
            } else if matches!(base_import(import), ImportTarget::CustomFunction { .. }) {
                let binding = binding_name(import);
                let Some(linked) = links.module_functions.get(&binding) else {
                    return fallback(
                        "native linked Module function has no immutable RELC link context",
                    );
                };
                linked_import = Some((binding, linked));
            } else {
                return fallback(
                    "native Route-WASM v10 only supports one direct http.get/post/request import or one linked Module function import",
                );
            }
        }
        _ => {
            return fallback(
                "native Route-WASM v10 supports at most one direct or linked host-call import",
            )
        }
    }
'''
new_imports = r'''    let mut host_imports = BTreeMap::<String, DirectCapabilityImport>::new();
    let mut linked_imports = BTreeMap::<String, &LinkedModuleFunction>::new();
    for import in &file.imports {
        if let Some(found) = direct_capability_import(import) {
            let binding = found.binding.clone();
            if linked_imports.contains_key(&binding)
                || host_imports.insert(binding.clone(), found).is_some()
            {
                return fallback(format!(
                    "native Route-WASM v12 found ambiguous import binding {binding:?}"
                ));
            }
            continue;
        }

        if matches!(base_import(import), ImportTarget::CustomFunction { .. }) {
            let binding = binding_name(import);
            let Some(linked) = links.module_functions.get(&binding) else {
                return fallback(
                    "native linked Module function has no immutable RELC link context",
                );
            };
            if host_imports.contains_key(&binding)
                || linked_imports.insert(binding.clone(), linked).is_some()
            {
                return fallback(format!(
                    "native Route-WASM v12 found ambiguous import binding {binding:?}"
                ));
            }
            continue;
        }

        return fallback(
            "native Route-WASM v12 supports only exact direct http.get/post/request imports or exact linked Module function imports; namespace imports remain interpreter-only",
        );
    }
'''
text = replace_once(text, old_imports, new_imports, "import selection")

text = replace_once(
    text,
    """    let method = &file.methods[0];
    let (bytes, input) = if let Some(import) = host_import.as_ref() {""",
    """    let method = &file.methods[0];
    let returned_binding = returned_call_binding(&method.body);
    let host_import = returned_binding.and_then(|binding| host_imports.get(binding));
    let linked_import = returned_binding
        .and_then(|binding| linked_imports.get(binding).map(|linked| (binding.to_string(), *linked)));

    let (bytes, input) = if let Some(import) = host_import {""",
    "selected import binding",
)

text = replace_once(
    text,
    """    } else if let Some((binding, linked)) = linked_import.as_ref() {""",
    """    } else if let Some((binding, linked)) = linked_import.as_ref() {""",
    "linked selected import form",
)

anchor = """fn fallback(reason: impl Into<String>) -> RouteWasmCompilation {"""
if anchor not in text:
    raise SystemExit("returned binding helper anchor missing")
helper = r'''fn returned_call_binding(body: &[Statement]) -> Option<&str> {
    let [Statement::Return(Expr::Call(target, _))] = body else {
        return None;
    };
    let Expr::Ident(binding) = target.as_ref() else {
        return None;
    };
    Some(binding.as_str())
}

'''
text = text.replace(anchor, helper + anchor, 1)

anchor = """    #[test]
    fn direct_static_http_get_is_native_v3_capability_call() {"""
if anchor not in text:
    raise SystemExit("multi-import tests anchor missing")
new_tests = r'''    #[test]
    fn multiple_exact_direct_imports_select_the_called_binding() {
        let route = parse(
            r#":import[http.get as fetch]
               :import[http.post as send]
               class Route {
                   get(req) { return send("https://example.com/data", { ok: true }); }
               }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("multiple exact direct imports should select the returned binding");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Network);
            assert_eq!(request.target, PUBLIC_HTTP_TARGET);
            assert_eq!(request.operation, "post");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["https://example.com/data", { "ok": true }])
            );
            Ok(br#"{"ok":true}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, br#"{"ok":true}"#);
    }

    #[test]
    fn mixed_exact_direct_and_linked_imports_select_the_linked_binding() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               export function load(path) { return readEntry(path); }"#,
        );
        let links = link_module_function("load", "accounts.cache", &module, "load");
        let route = parse(
            r#":import[http.get as fetch]
               :import["./module/accounts/cache".load]
               class Route { get(req) { return load("users/kate.json"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("mixed exact imports should select the linked binding");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Storage);
            assert_eq!(request.target, "storage:accounts.cache");
            assert_eq!(request.operation, "read");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["users/kate.json"])
            );
            Ok(br#"{"found":true}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, br#"{"found":true}"#);
    }

    #[test]
    fn namespace_import_among_exact_imports_still_fails_closed() {
        let route = parse(
            r#":import[http]
               :import[http.get as fetch]
               class Route { get(req) { return fetch("https://example.com/data"); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("namespace import must not gain native authority implicitly");
        };
        assert!(reason.contains("namespace imports remain interpreter-only"));
    }

'''
text = text.replace(anchor, new_tests + anchor, 1)

path.write_text(text)
