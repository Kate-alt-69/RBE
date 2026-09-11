from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


cache = Path("engine/crates/route-engine/src/cache.rs")
replace_once(
    cache,
    '''    #[test]
    fn dynamic_route_records_explicit_interpreter_fallback_without_wasm() {
        let root = temp_dir("wasm-fallback");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("echo.route"),
            "class Route { post(req) { return req.body; } }",
        )
        .unwrap();''',
    '''    #[test]
    fn unsupported_dynamic_route_records_explicit_interpreter_fallback_without_wasm() {
        let root = temp_dir("wasm-fallback");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("echo.route"),
            "class Route { post(req) { return req.query; } }",
        )
        .unwrap();''',
    "cache fallback expectation",
)
# Add a cache-level assertion that the newly supported request-body form emits
# actual WASM, instead of merely changing the old negative test.
replace_once(
    cache,
    '''    #[test]
    fn dynamic_route_records_explicit_interpreter_fallback_without_wasm() {''',
    '''    #[test]
    fn request_body_route_emits_native_wasm() {
        let root = temp_dir("wasm-request-body");
        let api_dir = root.join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("echo.route"),
            "class Route { post(req) { return req.body; } }",
        )
        .unwrap();

        let cache_root = root.join(".cache");
        let io = atomic_io::AtomicIo::new();
        let first = sync(&io, &api_dir, &cache_root).unwrap();
        assert_eq!(first[0].result, Ok(SyncAction::Regenerated));
        let wasm = std::fs::read(&first[0].wasm_path).unwrap();
        wasmparser::validate(&wasm).unwrap();
        let manifest = std::fs::read_to_string(&first[0].manifest_path).unwrap();
        assert!(manifest.contains("\\\"status\\\": \\\"native\\\""));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dynamic_route_records_explicit_interpreter_fallback_without_wasm() {''',
    "cache native request-body test insertion anchor",
)

relc = Path("engine/crates/route-engine/src/relc.rs")
replace_once(
    relc,
    '''                "api/dynamic.route",
                "class Route { post(req) { return req.body; } }",''',
    '''                "api/dynamic.route",
                "class Route { post(req) { return req.query; } }",''',
    "RELC unsupported dynamic fixture",
)
replace_once(
    relc,
    '''            .contains("runtime REL evaluation"));''',
    '''            .contains("outside the native Route-WASM v2 subset"));''',
    "RELC v2 fallback reason",
)
