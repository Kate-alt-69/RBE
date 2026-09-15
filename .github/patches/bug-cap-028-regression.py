from pathlib import Path

path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()
old = r'''    #[test]
    fn linked_module_dynamic_route_argument_stays_interpreter_fallback() {
        let module = parse_module(
            r#":import[service:uac.get_user as getUser]
               export function lookup(id) { return getUser(id); }"#,
        );
        let links = link_module_function("lookup", "accounts", &module, "lookup");
        let route = parse(
            r#":import["./module/accounts".lookup]
               class Route { post(req) { return lookup(req.body); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } =
            compile_route_with_links(&route, &links)
        else {
            panic!("dynamic linked arguments must remain interpreter-only");
        };
        assert!(reason.contains("static JSON arguments"));
    }
'''
new = r'''    #[test]
    fn linked_module_transformed_dynamic_route_argument_stays_interpreter_fallback() {
        let module = parse_module(
            r#":import[service:uac.get_user as getUser]
               export function lookup(id) { return getUser(id); }"#,
        );
        let links = link_module_function("lookup", "accounts", &module, "lookup");
        let route = parse(
            r#":import["./module/accounts".lookup]
               class Route { post(req) { return lookup(req.body.id); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } =
            compile_route_with_links(&route, &links)
        else {
            panic!("transformed dynamic linked arguments must remain interpreter-only");
        };
        assert!(reason.contains("exactly one req.body value passed unchanged"));
    }
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"dynamic fallback regression anchor count={count}")
path.write_text(text.replace(old, new, 1))
