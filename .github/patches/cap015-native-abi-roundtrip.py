from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


cargo = Path("engine/crates/route-engine/Cargo.toml")
replace_once(
    cargo,
    '''[dev-dependencies]
wasmparser = "0.248"''',
    '''[dev-dependencies]
wasmparser = "0.248"
execution-engine = { path = "../../../container-runtime/crates/execution-engine" }''',
    "Route Engine Wasmtime test dependency",
)

compiler = Path("engine/crates/route-engine/src/wasm_compiler.rs")
replace_once(
    compiler,
    '''mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;''',
    '''mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use execution_engine::{CapabilityHost, ExecutionLimits, WasmExecutor};''',
    "WasmExecutor test imports",
)
replace_once(
    compiler,
    '''    #[test]
    fn aliased_static_http_get_is_native() {''',
    '''    #[test]
    fn generated_http_wasm_round_trips_through_real_capability_host_abi() {
        let route = parse(
            r#":import[http.get]
               class Route { get(req) { return get("https://example.com/data"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("direct HTTP route should compile natively");
        };
        let expected = br#"{"status":200,"ok":true,"headers":{},"body":"ok","contentType":"text/plain"}"#;
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, core_lib::ContainerCapabilityKind::Network);
            assert_eq!(request.target, PUBLIC_HTTP_TARGET);
            assert_eq!(request.operation, "get");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["https://example.com/data"])
            );
            Ok(br#"{"status":200,"ok":true,"headers":{},"body":"ok","contentType":"text/plain"}"#.to_vec())
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
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output, expected);
        assert!(result.fuel_consumed > 0);
    }

    #[test]
    fn generated_http_wasm_fails_closed_when_host_denies_capability() {
        let route = parse(
            r#":import[http.get]
               class Route { get(req) { return get("https://example.com/data"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("direct HTTP route should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, core_lib::ContainerCapabilityKind::Network);
            assert_eq!(request.target, PUBLIC_HTTP_TARGET);
            assert_eq!(request.operation, "get");
            Err("CAPABILITY_DENIED: test denial".into())
        });
        let error = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .expect_err("host denial must fail the whole WASM execution");
        let message = error.to_string();
        assert!(message.contains("WASM ABI violation"));
        assert!(message.contains("CAPABILITY_DENIED"));
    }

    #[test]
    fn aliased_static_http_get_is_native() {''',
    "native compiler-to-Wasmtime round-trip tests",
)
