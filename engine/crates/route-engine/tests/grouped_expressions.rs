use route_engine::{compile_runtime_image, PhysicalRelSource, RelSourceKind};

#[test]
fn module_accepts_node_style_grouped_boolean_expressions() {
    let module = r#"
export function acceptsPayload(payload) {
    if (payload == null || (payload.kind != "managed" && payload.kind != "custom")) {
        return false;
    }
    return true;
}
"#;

    let sources = vec![PhysicalRelSource::new(
        RelSourceKind::Module,
        "uac",
        "module/uac.module",
        module,
    )];

    compile_runtime_image("server Main {}", sources, &serde_json::json!({}))
        .expect("REL must accept JavaScript/Node-style grouped boolean expressions");
}

#[test]
fn grouping_can_override_boolean_precedence() {
    let module = r#"
export function grouped(a, b, c) {
    if ((a || b) && c) {
        return true;
    }
    return false;
}
"#;

    let sources = vec![PhysicalRelSource::new(
        RelSourceKind::Module,
        "precedence",
        "module/precedence.module",
        module,
    )];

    compile_runtime_image("server Main {}", sources, &serde_json::json!({}))
        .expect("grouped expressions must be legal anywhere an expression is legal");
}
