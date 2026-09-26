from pathlib import Path

cargo_path = Path("engine/crates/route-engine/Cargo.toml")
discovery_path = Path("engine/crates/route-engine/src/discovery.rs")
doc_path = Path("doc/field-manager.md")

cargo = cargo_path.read_text(encoding="utf-8")
old_dep = 'fancy-regex = "=0.16.2"\nwasm-encoder = "0.248"\n'
new_dep = 'fancy-regex = "=0.16.2"\nform_urlencoded = "1"\nwasm-encoder = "0.248"\n'
if old_dep not in cargo:
    raise SystemExit("Cargo dependency anchor not found")
cargo = cargo.replace(old_dep, new_dep, 1)
cargo_path.write_text(cargo, encoding="utf-8")

source = discovery_path.read_text(encoding="utf-8")
old_import = 'use axum::extract::{Path as AxumPath, Query, Request, State};'
new_import = 'use axum::extract::{Path as AxumPath, RawQuery, Request, State};'
if old_import not in source:
    raise SystemExit("Axum Query import anchor not found")
source = source.replace(old_import, new_import, 1)

request_error_anchor = '''fn request_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}
'''
query_helper = '''fn query_fields(raw_query: Option<&str>) -> Result<HashMap<String, String>, String> {
    let mut query = HashMap::new();
    for (key, value) in form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        let key = key.into_owned();
        let value = value.into_owned();
        if query.insert(key.clone(), value).is_some() {
            return Err(format!(
                "duplicate query field {key:?} is ambiguous; each decoded query name may appear only once"
            ));
        }
    }
    Ok(query)
}

'''
if request_error_anchor not in source:
    raise SystemExit("request_error anchor not found")
source = source.replace(request_error_anchor, query_helper + request_error_anchor, 1)

old_execute_sig = '''async fn execute(
    plan: RouteHandlerPlan,
    state: AppState,
    params: HashMap<String, String>,
    query: HashMap<String, String>,
    request: Request,
) -> Response {
'''
new_execute_sig = '''async fn execute(
    plan: RouteHandlerPlan,
    state: AppState,
    params: HashMap<String, String>,
    raw_query: Option<String>,
    request: Request,
) -> Response {
'''
if old_execute_sig not in source:
    raise SystemExit("execute signature anchor not found")
source = source.replace(old_execute_sig, new_execute_sig, 1)

old_path = '''    } = plan;
    let path = request.uri().path().to_string();
    let image = match request
'''
new_path = '''    } = plan;
    let path = request.uri().path().to_string();
    let query = match query_fields(raw_query.as_deref()) {
        Ok(query) => query,
        Err(error) => return request_error(StatusCode::BAD_REQUEST, error),
    };
    let image = match request
'''
if old_path not in source:
    raise SystemExit("execute query insertion anchor not found")
source = source.replace(old_path, new_path, 1)

old_handler = '''        let handler = move |State(state): State<AppState>,
                            AxumPath(params): AxumPath<HashMap<String, String>>,
                            Query(query): Query<HashMap<String, String>>,
                            request: Request| {
            let handler_plan = handler_plan.clone();
            async move { execute(handler_plan, state, params, query, request).await }
        };
'''
new_handler = '''        let handler = move |State(state): State<AppState>,
                            AxumPath(params): AxumPath<HashMap<String, String>>,
                            RawQuery(raw_query): RawQuery,
                            request: Request| {
            let handler_plan = handler_plan.clone();
            async move { execute(handler_plan, state, params, raw_query, request).await }
        };
'''
if old_handler not in source:
    raise SystemExit("route handler Query extractor anchor not found")
source = source.replace(old_handler, new_handler, 1)

test_anchor = '''    #[test]
    fn json_content_type_detection_matches_structured_media_types() {
'''
query_tests = '''    #[test]
    fn query_fields_decode_unique_scalar_values() {
        let query = query_fields(Some("a=1&b=hello+world&encoded%20key=value%2Ftwo")).unwrap();
        assert_eq!(query.get("a").map(String::as_str), Some("1"));
        assert_eq!(query.get("b").map(String::as_str), Some("hello world"));
        assert_eq!(
            query.get("encoded key").map(String::as_str),
            Some("value/two")
        );
        assert!(query_fields(None).unwrap().is_empty());
    }

    #[test]
    fn query_fields_reject_repeated_decoded_names() {
        for raw in ["id=first&id=second", "%69%64=first&id=second"] {
            let error = query_fields(Some(raw)).unwrap_err();
            assert!(error.contains("duplicate query field \"id\""), "{error}");
            assert!(error.contains("may appear only once"), "{error}");
        }
    }

'''
if test_anchor not in source:
    raise SystemExit("HTTP edge test anchor not found")
source = source.replace(test_anchor, query_tests + test_anchor, 1)
discovery_path.write_text(source, encoding="utf-8")

doc = doc_path.read_text(encoding="utf-8")
old_doc = '''FieldManager can resolve from the immutable request snapshot without re-parsing the HTTP request. Supported declarative sources are:
'''
new_doc = '''FieldManager can resolve from the immutable request snapshot without re-parsing the HTTP request. Query names are decoded at the HTTP boundary and remain scalar identities: the same decoded query name may appear only once. Repeated names (including percent-encoded aliases that decode to the same name) are rejected with HTTP 400 instead of being silently collapsed by first/last-value-wins behavior. Supported declarative sources are:
'''
if old_doc not in doc:
    raise SystemExit("FieldManager query documentation anchor not found")
doc = doc.replace(old_doc, new_doc, 1)
doc_path.write_text(doc, encoding="utf-8")
