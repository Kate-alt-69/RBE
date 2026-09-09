from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    (ROOT / path).write_text(text, encoding="utf-8")


# RuntimeSourceManifest must preserve embedded Route REL's explicit path.
path = "engine/crates/route-engine/src/runtime_image.rs"
text = read(path)
old = '''    pub exports: Vec<String>,
    pub imports: Vec<String>,
}'''
new = '''    pub exports: Vec<String>,
    pub imports: Vec<String>,
    pub route_path: Option<String>,
}'''
if old not in text:
    raise SystemExit("missing RuntimeSourceManifest path anchor")
text = text.replace(old, new, 1)
write(path, text)


path = "engine/crates/route-engine/src/relc.rs"
text = read(path)
server_anchor = '''    let server_id = registry.register_physical(
        RelSourceKind::Server,
        &server.name,
        "server.server",
        raw_server_source,
    )?;

'''
if server_anchor not in text:
    raise SystemExit("missing RELC server registry anchor")
text = text.replace(
    server_anchor,
    server_anchor + '''    let mut embedded_route_paths = BTreeMap::<SourceId, String>::new();

''',
    1,
)
old_loop = '''    for embedded in extracted.embedded {
        registry.register_embedded(
            &server_id,
            embedded.kind,
            embedded.logical_name,
            embedded.block_index,
            embedded.start_line,
            embedded.source,
        )?;
    }
'''
if old_loop not in text:
    raise SystemExit("missing RELC embedded registration anchor")
new_loop = '''    for embedded in extracted.embedded {
        let route_path = if embedded.kind == RelSourceKind::Route {
            embedded.attributes.get("path").cloned()
        } else {
            None
        };
        let embedded_id = registry.register_embedded(
            &server_id,
            embedded.kind,
            embedded.logical_name,
            embedded.block_index,
            embedded.start_line,
            embedded.source,
        )?;
        if let Some(route_path) = route_path {
            embedded_route_paths.insert(embedded_id, route_path);
        }
    }
'''
text = text.replace(old_loop, new_loop, 1)
manifest_anchor = '''            exports: unit.exports(),
            imports: unit.imports().iter().map(import_label).collect(),
        };'''
if manifest_anchor not in text:
    raise SystemExit("missing RuntimeSourceManifest construction anchor")
text = text.replace(
    manifest_anchor,
    '''            exports: unit.exports(),
            imports: unit.imports().iter().map(import_label).collect(),
            route_path: embedded_route_paths.get(source.id()).cloned(),
        };''',
    1,
)
write(path, text)


# Both registration and collision validation use the explicit embedded path when
# present; physical routes continue using their deterministic /api/... mapping.
path = "engine/crates/route-engine/src/discovery.rs"
text = read(path)
old = '''        let url_path = url_path_for_logical(&manifest.logical_name);
        tracing::info!(
            source = %id,'''
new = '''        let url_path = manifest
            .route_path
            .clone()
            .unwrap_or_else(|| url_path_for_logical(&manifest.logical_name));
        tracing::info!(
            source = %id,'''
if old not in text:
    raise SystemExit("missing Runtime Image route path anchor")
text = text.replace(old, new, 1)
write(path, text)


path = "engine/crates/route-engine/src/route_collision.rs"
text = read(path)
if '    "/health",\n' not in text:
    text = text.replace(
        '''const RESERVED_NATIVE_API_PREFIXES: &[&str] = &[
''',
        '''const RESERVED_NATIVE_API_PREFIXES: &[&str] = &[
    "/health",
''',
        1,
    )
old = '''        let url_path = url_path_for_logical(&manifest.logical_name);
        let owner = PathBuf::from(id.as_str());'''
new = '''        let url_path = manifest
            .route_path
            .clone()
            .unwrap_or_else(|| url_path_for_logical(&manifest.logical_name));
        let owner = PathBuf::from(id.as_str());'''
if old not in text:
    raise SystemExit("missing Runtime Image collision path anchor")
text = text.replace(old, new, 1)
write(path, text)


# Focused RELC assertion: the documented embedded path survives linking.
path = "engine/crates/route-engine/src/relc.rs"
text = read(path)
test_anchor = '''        assert_eq!(image.routes.len(), 1);
'''
if test_anchor not in text:
    raise SystemExit("missing RELC embedded image test anchor")
assertion = '''        assert_eq!(image.routes.len(), 1);
        let route = image.source(&image.routes[0]).unwrap();
        assert_eq!(route.route_path.as_deref(), Some("/health"));
'''
text = text.replace(test_anchor, assertion, 1)
write(path, text)
