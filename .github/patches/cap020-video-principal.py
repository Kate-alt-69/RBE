from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


module_runtime = Path("engine/crates/route-engine/src/module_runtime.rs")
replace_once(
    module_runtime,
    '''fn module_owner(module_dir: &Path, path: &Path) -> String {
    let root = normalize(module_dir);
    let relative = path.strip_prefix(&root).unwrap_or(path).with_extension("");
    let parts = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => {
                Some(encode_owner_segment(&value.to_string_lossy()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        "root".into()
    } else {
        parts.join(".")
    }
}
''',
    '''fn module_owner(module_dir: &Path, path: &Path) -> String {
    let root = normalize(module_dir);
    let relative = path.strip_prefix(&root).unwrap_or(path).with_extension("");
    canonical_module_owner(&relative)
}

/// Canonical capability principal for one linked Module REL logical name.
/// RELC uses this exact function so propagated host authority has the same
/// owner identity that ModuleExecutor supplies to VideoLanguage at runtime.
pub(crate) fn module_owner_from_logical_name(logical_name: &str) -> String {
    canonical_module_owner(Path::new(logical_name))
}

fn canonical_module_owner(relative: &Path) -> String {
    let parts = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => {
                Some(encode_owner_segment(&value.to_string_lossy()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        "root".into()
    } else {
        parts.join(".")
    }
}
''',
    "shared canonical module owner",
)
replace_once(
    module_runtime,
    '''    #[test]
    fn rejects_legacy_video_capability_name_at_boot() {''',
    '''    #[test]
    fn linked_logical_name_uses_same_canonical_owner_as_runtime_resolution() {
        assert_eq!(
            module_owner_from_logical_name("learning/catalog"),
            "learning.catalog"
        );
        assert_eq!(module_owner_from_logical_name("user data/auth"), "user_20data.auth");
    }

    #[test]
    fn rejects_legacy_video_capability_name_at_boot() {''',
    "canonical owner parity test",
)

runtime_image = Path("engine/crates/route-engine/src/runtime_image.rs")
replace_once(
    runtime_image,
    '''    Video { operation: String },''',
    '''    Video { owner: String, operation: String },''',
    "Video requirement principal",
)
replace_once(
    runtime_image,
    '''            RuntimeCapabilityRequirement::Video { operation } => {
                return Err(RuntimeCapabilityLoweringError {
                    message: format!(
                        "Video capability operation {operation:?} has no native Container grant lowering yet"
                    ),
                });
            }''',
    '''            RuntimeCapabilityRequirement::Video { owner, operation } => {
                return Err(RuntimeCapabilityLoweringError {
                    message: format!(
                        "Video capability principal {owner:?} operation {operation:?} has no native Container grant lowering yet"
                    ),
                });
            }''',
    "Video fail-closed principal message",
)
replace_once(
    runtime_image,
    '''            RuntimeCapabilityRequirement::Video {
                operation: "status".into(),
            },''',
    '''            RuntimeCapabilityRequirement::Video {
                owner: "media.bridge".into(),
                operation: "status".into(),
            },''',
    "Video lowering test principal",
)

relc = Path("engine/crates/route-engine/src/relc.rs")
replace_once(
    relc,
    '''use crate::module_runtime::ModuleProgram;''',
    '''use crate::module_runtime::{module_owner_from_logical_name, ModuleProgram};''',
    "RELC canonical module owner import",
) if "use crate::module_runtime::ModuleProgram;" in relc.read_text(encoding="utf-8") else None

# module_runtime imports are arranged differently on some revisions; insert next
# to the modules import if the direct ModuleProgram anchor was not present.
text = relc.read_text(encoding="utf-8")
if "module_owner_from_logical_name" not in text:
    replace_once(
        relc,
        '''use crate::modules::binding_name;''',
        '''use crate::module_runtime::module_owner_from_logical_name;
use crate::modules::binding_name;''',
        "RELC canonical module owner import fallback",
    )

replace_once(
    relc,
    '''fn builtin_host_requirements(
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
''',
    '''fn builtin_host_requirements(
    source: &SourceId,
    module: &str,
    function: Option<&str>,
    video_owner: Option<&str>,
) -> Result<BTreeSet<RuntimeCapabilityRequirement>, RelcError> {
    let operations = match module {
        "http" => HTTP_HOST_OPERATIONS,
        "vm" | "video-manager" => VIDEO_HOST_OPERATIONS,
        _ => return Ok(BTreeSet::new()),
    };
    let video_owner = if matches!(module, "vm" | "video-manager") {
        Some(video_owner.ok_or_else(|| RelcError::Capability {
            source: source.clone(),
            message: "Video Manager authority is module-owned; direct Video capability requirements must originate from Module REL".into(),
        })?)
    } else {
        None
    };
    Ok(operations
        .iter()
        .copied()
        .filter(|operation| function.is_none_or(|function| function == *operation))
        .map(|operation| match module {
            "http" => RuntimeCapabilityRequirement::PublicHttp {
                operation: operation.to_string(),
            },
            _ => RuntimeCapabilityRequirement::Video {
                owner: video_owner.expect("Video owner validated above").to_string(),
                operation: operation.to_string(),
            },
        })
        .collect())
}
''',
    "principal-aware builtin host requirements",
)
replace_once(
    relc,
    '''fn direct_capability_requirements(
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
            }''',
    '''fn direct_capability_requirements(
    source: &SourceId,
    registry: &RelSourceRegistry,
    compiled: &BTreeMap<SourceId, CompiledUnit>,
    imports: &[ImportTarget],
) -> Result<BTreeSet<RuntimeCapabilityRequirement>, RelcError> {
    let source_record = registry
        .get(source)
        .ok_or_else(|| RelcError::Link(format!("compiled source {source} is not registered")))?;
    let video_owner = (source_record.kind() == RelSourceKind::Module)
        .then(|| module_owner_from_logical_name(source_record.logical_name()));
    let mut out = BTreeSet::new();
    for import in imports {
        match import_base(import) {
            ImportTarget::Builtin(module) => {
                out.extend(builtin_host_requirements(
                    source,
                    module,
                    None,
                    video_owner.as_deref(),
                )?);
            }
            ImportTarget::BuiltinFunction { module, function } => {
                out.extend(builtin_host_requirements(
                    source,
                    module,
                    Some(function),
                    video_owner.as_deref(),
                )?);
            }''',
    "direct capability principal source",
)
replace_once(
    relc,
    '''            direct_capability_requirements(registry, compiled, unit.imports())?,''',
    '''            direct_capability_requirements(source, registry, compiled, unit.imports())?,''',
    "capability source identity propagation",
)
replace_once(
    relc,
    '''        assert!(requirements.contains(&RuntimeCapabilityRequirement::Video {
            operation: "status".into(),
        }));''',
    '''        assert!(requirements.contains(&RuntimeCapabilityRequirement::Video {
            owner: "hosted".into(),
            operation: "status".into(),
        }));''',
    "typed host requirement owner assertion",
)
replace_once(
    relc,
    '''    #[test]
    fn service_namespace_import_expands_exported_operations() {''',
    '''    #[test]
    fn video_principal_survives_nested_module_to_route_propagation() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Module,
                "learning/catalog",
                "module/learning/catalog.module",
                r#":import[video-manager.status]
                   export function run(value) { return value; }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "catalog-status",
                "api/catalog-status.route",
                r#":import[module&learning/catalog]
                   class Route { get(req) { return true; } }"#,
            ),
        ];
        let image =
            compile_runtime_image("server Main {}", sources, &serde_json::json!({})).unwrap();
        let expected = RuntimeCapabilityRequirement::Video {
            owner: "learning.catalog".into(),
            operation: "status".into(),
        };
        let module = image
            .modules
            .iter()
            .find(|id| image.source(id).is_some_and(|source| source.logical_name == "learning/catalog"))
            .unwrap();
        assert!(image.capability_requirements(module).unwrap().contains(&expected));
        let route = image.routes.first().unwrap();
        assert!(
            image.capability_requirements(route).unwrap().contains(&expected),
            "propagation must preserve the declaring module principal"
        );
    }

    #[test]
    fn distinct_video_modules_keep_distinct_principals() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Module,
                "media/alpha",
                "module/media/alpha.module",
                r#":import[vm.status]
                   export function run(value) { return value; }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Module,
                "media/beta",
                "module/media/beta.module",
                r#":import[vm.status]
                   export function run(value) { return value; }"#,
            ),
        ];
        let image =
            compile_runtime_image("server Main {}", sources, &serde_json::json!({})).unwrap();
        let owners = image
            .modules
            .iter()
            .flat_map(|id| image.capability_requirements(id).into_iter().flatten())
            .filter_map(|requirement| match requirement {
                RuntimeCapabilityRequirement::Video { owner, operation } if operation == "status" => {
                    Some(owner.as_str())
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(owners, BTreeSet::from(["media.alpha", "media.beta"]));
    }

    #[test]
    fn service_namespace_import_expands_exported_operations() {''',
    "Video principal propagation tests",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''RELC `PublicHttp` requirements lower to one exact `Network/public-http` Controller grant with an explicit operation set; Service and Video requirements remain fail-closed until their own native lowering exists.''',
    '''RELC `PublicHttp` requirements lower to one exact `Network/public-http` Controller grant with an explicit operation set. Video requirements now retain the canonical declaring Module principal (for example `learning.catalog`) through dependency propagation, but remain fail-closed until Video grant/host lowering is implemented; Service requirements likewise remain fail-closed until their native lowering exists.''',
    "principal-aware Video docs",
)
