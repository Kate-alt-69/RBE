from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


runtime_image = Path("engine/crates/route-engine/src/runtime_image.rs")
replace_once(
    runtime_image,
    '''#[derive(Debug, Clone)]
pub struct RuntimeSourceManifest {''',
    '''/// Host-crossing operations RELC discovered for a source. These are
/// compiler requirements, not Controller grants: target policy/byte limits and
/// reachability are still lowered explicitly before native execution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimeCapabilityRequirement {
    PublicHttp { operation: String },
    Video { operation: String },
    Service { service: String, operation: String },
}

#[derive(Debug, Clone)]
pub struct RuntimeSourceManifest {''',
    "typed capability requirement model",
)
replace_once(
    runtime_image,
    '''    pub capabilities: BTreeMap<SourceId, BTreeSet<String>>,''',
    '''    pub capabilities: BTreeMap<SourceId, BTreeSet<RuntimeCapabilityRequirement>>,''',
    "typed Runtime Image capability field",
)
replace_once(
    runtime_image,
    '''    pub fn route_wasm_artifact(&self, id: &SourceId) -> Option<&RouteWasmArtifact> {''',
    '''    pub fn capability_requirements(
        &self,
        id: &SourceId,
    ) -> Option<&BTreeSet<RuntimeCapabilityRequirement>> {
        self.capabilities.get(id)
    }

    pub fn route_wasm_artifact(&self, id: &SourceId) -> Option<&RouteWasmArtifact> {''',
    "Runtime Image typed capability accessor",
)

relc = Path("engine/crates/route-engine/src/relc.rs")
replace_once(
    relc,
    '''use crate::runtime_image::{
    stable_image_hash, stable_source_hash, RuntimeExecutable, RuntimeImage, RuntimeSourceManifest,
};''',
    '''use crate::runtime_image::{
    stable_image_hash, stable_source_hash, RuntimeCapabilityRequirement, RuntimeExecutable,
    RuntimeImage, RuntimeSourceManifest,
};''',
    "RELC typed capability import",
)
replace_once(
    relc,
    '''    let mut capabilities = BTreeMap::new();
    let mut service_assignments = BTreeMap::new();''',
    '''    let capabilities = capability_requirements(&registry, &compiled)?;
    let mut service_assignments = BTreeMap::new();''',
    "RELC typed capability analysis",
)
replace_once(
    relc,
    '''        capabilities.insert(source.id().clone(), capability_set(unit.imports()));
        sources.push(manifest);''',
    '''        sources.push(manifest);''',
    "remove legacy string capability collection",
)
replace_once(
    relc,
    '''fn capability_set(imports: &[ImportTarget]) -> BTreeSet<String> {
    imports
        .iter()
        .filter_map(|import| match import_base(import) {
            ImportTarget::Builtin(name) => Some(name.clone()),
            ImportTarget::BuiltinFunction { module, .. } => Some(module.clone()),
            _ => None,
        })
        .collect()
}
''',
    '''const HTTP_HOST_OPERATIONS: &[&str] = &["get", "post", "request"];
const VIDEO_HOST_OPERATIONS: &[&str] = &[
    "status",
    "databaseHealth",
    "database_health",
    "get",
    "job",
    "variants",
    "create",
    "queueDownload",
    "queue_download",
    "reserveLive",
    "reserve_live",
    "liveSession",
    "live_session",
    "endLive",
    "end_live",
];

fn builtin_host_requirements(
    module: &str,
    function: Option<&str>,
) -> BTreeSet<RuntimeCapabilityRequirement> {
    let operations = match module {
        "http" => HTTP_HOST_OPERATIONS,
        "vm" | "video-manager" => VIDEO_HOST_OPERATIONS,
        _ => return BTreeSet::new(),
    };
    operations
        .iter()
        .copied()
        .filter(|operation| function.is_none_or(|function| function == *operation))
        .map(|operation| match module {
            "http" => RuntimeCapabilityRequirement::PublicHttp {
                operation: operation.to_string(),
            },
            _ => RuntimeCapabilityRequirement::Video {
                operation: operation.to_string(),
            },
        })
        .collect()
}

fn direct_capability_requirements(
    registry: &RelSourceRegistry,
    compiled: &BTreeMap<SourceId, CompiledUnit>,
    imports: &[ImportTarget],
) -> Result<BTreeSet<RuntimeCapabilityRequirement>, RelcError> {
    let mut out = BTreeSet::new();
    for import in imports {
        match import_base(import) {
            ImportTarget::Builtin(module) => {
                out.extend(builtin_host_requirements(module, None));
            }
            ImportTarget::BuiltinFunction { module, function } => {
                out.extend(builtin_host_requirements(module, Some(function)));
            }
            ImportTarget::Service(service) => {
                let target = registry
                    .get_logical(RelSourceKind::Service, service)
                    .ok_or_else(|| RelcError::Link(format!("missing service `{service}`")))?;
                let unit = compiled.get(target.id()).ok_or_else(|| {
                    RelcError::Link(format!("service `{service}` was registered but not compiled"))
                })?;
                for operation in unit.exports() {
                    out.insert(RuntimeCapabilityRequirement::Service {
                        service: service.clone(),
                        operation,
                    });
                }
            }
            ImportTarget::ServiceFunction { service, function } => {
                out.insert(RuntimeCapabilityRequirement::Service {
                    service: service.clone(),
                    operation: function.clone(),
                });
            }
            ImportTarget::Custom(_)
            | ImportTarget::CustomFunction { .. }
            | ImportTarget::Aliased { .. } => {}
        }
    }
    Ok(out)
}

fn imported_module_sources(
    registry: &RelSourceRegistry,
    imports: &[ImportTarget],
) -> BTreeSet<SourceId> {
    imports
        .iter()
        .filter_map(|import| match import_base(import) {
            ImportTarget::Custom(path) | ImportTarget::CustomFunction { path, .. } => registry
                .get_logical(RelSourceKind::Module, &logical_module_name(path))
                .map(|source| source.id().clone()),
            _ => None,
        })
        .collect()
}

fn capability_requirements(
    registry: &RelSourceRegistry,
    compiled: &BTreeMap<SourceId, CompiledUnit>,
) -> Result<BTreeMap<SourceId, BTreeSet<RuntimeCapabilityRequirement>>, RelcError> {
    let mut requirements = BTreeMap::new();
    let mut module_dependencies = BTreeMap::new();
    for (source, unit) in compiled {
        requirements.insert(
            source.clone(),
            direct_capability_requirements(registry, compiled, unit.imports())?,
        );
        module_dependencies.insert(
            source.clone(),
            imported_module_sources(registry, unit.imports()),
        );
    }

    // Module code executes in the caller's execution context, so host-crossing
    // requirements must propagate through module imports. Iterate to a fixed
    // point so permitted module cycles remain deterministic and finite.
    loop {
        let snapshot = requirements.clone();
        let mut changed = false;
        for (source, dependencies) in &module_dependencies {
            let inherited = dependencies
                .iter()
                .filter_map(|dependency| snapshot.get(dependency))
                .flat_map(|set| set.iter().cloned())
                .collect::<BTreeSet<_>>();
            let current = requirements
                .get_mut(source)
                .expect("compiled source capability table missing");
            let before = current.len();
            current.extend(inherited);
            changed |= current.len() != before;
        }
        if !changed {
            break;
        }
    }
    Ok(requirements)
}
''',
    "typed transitive capability analysis",
)
# Add tests near the end of RELC tests.
replace_once(
    relc,
    '''    #[test]
    fn route_runtime_env_is_a_capability_error_not_a_grammar_error() {''',
    '''    #[test]
    fn typed_host_requirements_ignore_local_builtins() {
        let modules = vec![PhysicalRelSource::new(
            RelSourceKind::Module,
            "hosted",
            "module/hosted.module",
            r#":import[json, http.get, video-manager.status]
               export function run(value) { return value; }"#,
        )];
        let image =
            compile_runtime_image("server Main {}", modules, &serde_json::json!({})).unwrap();
        let id = &image.modules[0];
        let requirements = image.capability_requirements(id).unwrap();
        assert!(requirements.contains(&RuntimeCapabilityRequirement::PublicHttp {
            operation: "get".into(),
        }));
        assert!(requirements.contains(&RuntimeCapabilityRequirement::Video {
            operation: "status".into(),
        }));
        assert_eq!(requirements.len(), 2, "json is local and needs no host grant");
    }

    #[test]
    fn module_host_requirements_propagate_to_route_callers() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Module,
                "bridge",
                "module/bridge.module",
                r#":import[http.post]
                   export function run(value) { return value; }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "uses-bridge",
                "api/uses-bridge.route",
                r#":import[module&bridge]
                   class Route { get(req) { return true; } }"#,
            ),
        ];
        let image =
            compile_runtime_image("server Main {}", sources, &serde_json::json!({})).unwrap();
        let route = &image.routes[0];
        assert!(image
            .capability_requirements(route)
            .unwrap()
            .contains(&RuntimeCapabilityRequirement::PublicHttp {
                operation: "post".into(),
            }));
    }

    #[test]
    fn service_namespace_import_expands_exported_operations() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Service,
                "search",
                "service/search.service",
                r#":service[name = search]
                   export function find(value) { return value; }
                   export function remove(value) { return value; }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Module,
                "client",
                "module/client.module",
                r#":import[service:search]
                   export function run(value) { return value; }"#,
            ),
        ];
        let image =
            compile_runtime_image("server Main {}", sources, &serde_json::json!({})).unwrap();
        let module = image
            .modules
            .iter()
            .find(|id| image.source(id).is_some_and(|source| source.logical_name == "client"))
            .unwrap();
        let requirements = image.capability_requirements(module).unwrap();
        for operation in ["find", "remove"] {
            assert!(requirements.contains(&RuntimeCapabilityRequirement::Service {
                service: "search".into(),
                operation: operation.into(),
            }));
        }
    }

    #[test]
    fn route_runtime_env_is_a_capability_error_not_a_grammar_error() {''',
    "typed capability tests",
)

discovery = Path("engine/crates/route-engine/src/discovery.rs")
replace_once(
    discovery,
    '''    if image
        .capabilities
        .get(&plan.source_id)
        .is_some_and(|capabilities| !capabilities.is_empty())''',
    '''    if image
        .capability_requirements(&plan.source_id)
        .is_some_and(|capabilities| !capabilities.is_empty())''',
    "native typed capability gate",
)
replace_once(
    discovery,
    '''        let error = "native Route-WASM declares capabilities not lowered by the native compiler";''',
    '''        let error = "native Route-WASM declares host capability requirements not lowered by the native compiler";''',
    "native typed capability error",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''    capabilities
    routeWasmArtifacts''',
    '''    capabilities (typed host requirements)
    routeWasmArtifacts''',
    "runtime image field documentation",
)
replace_once(
    doc,
    '''- Service assignments/capability metadata pinned to the image.''',
    '''- Service assignments and typed host-capability requirements pinned to the image. RELC distinguishes local language helpers from host-crossing HTTP, Video Manager, and Service operations and propagates Module requirements to callers. These compiler requirements are not themselves Controller grants; native lowering must still bind exact policy/limits before registration.''',
    "typed capability documentation",
)
