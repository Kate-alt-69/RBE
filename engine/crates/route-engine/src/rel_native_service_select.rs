//! Select whole Services for the native OID pipeline.
//!
//! Native lowering is optional. A Service enters this selection only when every
//! executable root in the current subset can be lowered; otherwise the complete
//! Service remains evaluator-backed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::ast::ServiceProgram;
use crate::oid_link::{LinkedRelKind, LinkedRelSymbolSpec};
use crate::oid_materialize::NativeOidFragment;
use crate::rel_native_lowering::{lower_linked_rel_function, NativeRelLowering};
use crate::rel_symbol_discovery::LinkedRelDiscovery;
use crate::runtime_image::RuntimeImage;
use crate::service_oid::OidTarget;
use crate::source_registry::{RelSourceKind, SourceId};

#[derive(Debug, Clone)]
pub struct NativeServiceSelection {
    pub discovery: LinkedRelDiscovery,
    pub fragments: BTreeMap<String, NativeOidFragment>,
    pub native_services: BTreeSet<String>,
    pub source_ids: BTreeMap<String, SourceId>,
    pub evaluator_fallbacks: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct RuntimeService {
    source_id: SourceId,
    program: Arc<ServiceProgram>,
}

pub fn select_native_services(
    image: &RuntimeImage,
    discovery: &LinkedRelDiscovery,
    target: &OidTarget,
) -> Result<NativeServiceSelection, NativeServiceSelectionError> {
    let mut services = BTreeMap::new();
    for source_id in &image.services {
        let manifest = image
            .source(source_id)
            .ok_or_else(|| NativeServiceSelectionError::MissingManifest(source_id.clone()))?;
        if manifest.kind != RelSourceKind::Service {
            return Err(NativeServiceSelectionError::WrongSourceKind {
                source_id: source_id.clone(),
                kind: manifest.kind,
            });
        }
        let program = image
            .service_program(source_id)
            .ok_or_else(|| NativeServiceSelectionError::MissingProgram(source_id.clone()))?;
        if services
            .insert(
                manifest.logical_name.clone(),
                RuntimeService {
                    source_id: source_id.clone(),
                    program,
                },
            )
            .is_some()
        {
            return Err(NativeServiceSelectionError::DuplicateService(
                manifest.logical_name.clone(),
            ));
        }
    }

    for service in discovery
        .service_roots
        .keys()
        .chain(discovery.service_exports.keys())
    {
        if !services.contains_key(service) {
            return Err(NativeServiceSelectionError::MissingRuntimeService(
                service.clone(),
            ));
        }
    }

    select_programs(discovery, &services, target)
}

fn select_programs(
    discovery: &LinkedRelDiscovery,
    services: &BTreeMap<String, RuntimeService>,
    target: &OidTarget,
) -> Result<NativeServiceSelection, NativeServiceSelectionError> {
    let symbols = discovery
        .symbols
        .iter()
        .map(|symbol| (symbol.canonical_id.as_str(), symbol))
        .collect::<BTreeMap<_, _>>();

    let mut selected_ids = BTreeSet::new();
    let mut fragments = BTreeMap::new();
    let mut native_services = BTreeSet::new();
    let mut source_ids = BTreeMap::new();
    let mut evaluator_fallbacks = BTreeMap::new();
    let mut service_roots = BTreeMap::new();
    let mut service_exports = BTreeMap::new();

    for (service, runtime) in services {
        let Some(roots) = discovery.service_roots.get(service) else {
            evaluator_fallbacks.insert(service.clone(), "no executable linked-REL roots".into());
            continue;
        };
        let exports = discovery
            .service_exports
            .get(service)
            .cloned()
            .unwrap_or_default();

        let parsed_exports = runtime.program.exports.iter().cloned().collect::<BTreeSet<_>>();
        let discovered_exports = exports.keys().cloned().collect::<BTreeSet<_>>();
        if parsed_exports != discovered_exports {
            return Err(NativeServiceSelectionError::ExportSurfaceMismatch {
                service: service.clone(),
                parsed: parsed_exports,
                discovered: discovered_exports,
            });
        }

        if exports.is_empty() {
            evaluator_fallbacks.insert(service.clone(), "no public Service exports".into());
            continue;
        }
        if !runtime.program.lifecycle.is_empty() {
            evaluator_fallbacks.insert(
                service.clone(),
                "lifecycle hooks are not in the initial native Service subset".into(),
            );
            continue;
        }

        let export_roots = exports.values().cloned().collect::<BTreeSet<_>>();
        if roots != &export_roots {
            evaluator_fallbacks.insert(
                service.clone(),
                "Service has non-export executable roots".into(),
            );
            continue;
        }

        let functions = runtime
            .program
            .functions
            .iter()
            .map(|function| (function.name.as_str(), function))
            .collect::<BTreeMap<_, _>>();
        let mut local_fragments = BTreeMap::new();
        let mut local_ids = BTreeSet::new();
        let mut fallback = None;

        for (export, canonical_id) in &exports {
            let symbol = symbols.get(canonical_id.as_str()).ok_or_else(|| {
                NativeServiceSelectionError::MissingSymbol {
                    service: service.clone(),
                    export: export.clone(),
                    canonical_id: canonical_id.clone(),
                }
            })?;
            if symbol.kind != LinkedRelKind::ServiceExport {
                return Err(NativeServiceSelectionError::WrongExportKind {
                    service: service.clone(),
                    export: export.clone(),
                    kind: symbol.kind,
                });
            }
            let function = functions.get(export.as_str()).ok_or_else(|| {
                NativeServiceSelectionError::MissingFunction {
                    service: service.clone(),
                    export: export.clone(),
                }
            })?;

            match lower_linked_rel_function(symbol, function, target) {
                Ok(NativeRelLowering::Native(fragment)) => {
                    local_fragments.insert(canonical_id.clone(), fragment);
                    local_ids.insert(canonical_id.clone());
                }
                Ok(NativeRelLowering::EvaluatorFallback { reason }) => {
                    fallback = Some(format!("export {export:?}: {reason}"));
                    break;
                }
                Err(error) => {
                    fallback = Some(format!(
                        "export {export:?}: target {} cannot lower natively: {error}",
                        target.label()
                    ));
                    break;
                }
            }
        }

        if let Some(reason) = fallback {
            evaluator_fallbacks.insert(service.clone(), reason);
            continue;
        }

        fragments.extend(local_fragments);
        selected_ids.extend(local_ids);
        native_services.insert(service.clone());
        source_ids.insert(service.clone(), runtime.source_id.clone());
        service_roots.insert(service.clone(), roots.clone());
        service_exports.insert(service.clone(), exports);
    }

    let selected_symbols = discovery
        .symbols
        .iter()
        .filter(|symbol| selected_ids.contains(&symbol.canonical_id))
        .cloned()
        .collect();

    Ok(NativeServiceSelection {
        discovery: LinkedRelDiscovery {
            symbols: selected_symbols,
            service_roots,
            service_exports,
            reachable_symbols: selected_ids,
        },
        fragments,
        native_services,
        source_ids,
        evaluator_fallbacks,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeServiceSelectionError {
    MissingManifest(SourceId),
    WrongSourceKind {
        source_id: SourceId,
        kind: RelSourceKind,
    },
    MissingProgram(SourceId),
    DuplicateService(String),
    MissingRuntimeService(String),
    ExportSurfaceMismatch {
        service: String,
        parsed: BTreeSet<String>,
        discovered: BTreeSet<String>,
    },
    MissingSymbol {
        service: String,
        export: String,
        canonical_id: String,
    },
    WrongExportKind {
        service: String,
        export: String,
        kind: LinkedRelKind,
    },
    MissingFunction {
        service: String,
        export: String,
    },
}

impl fmt::Display for NativeServiceSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingManifest(id) => write!(f, "Runtime Image Service {id} has no manifest"),
            Self::WrongSourceKind { source_id, kind } => {
                write!(f, "Runtime Image Service {source_id} points at {kind} metadata")
            }
            Self::MissingProgram(id) => write!(f, "Runtime Image Service {id} has no ServiceProgram"),
            Self::DuplicateService(service) => {
                write!(f, "Runtime Image repeats logical Service {service:?}")
            }
            Self::MissingRuntimeService(service) => write!(
                f,
                "linked REL discovery references missing Runtime Image Service {service:?}"
            ),
            Self::ExportSurfaceMismatch {
                service,
                parsed,
                discovered,
            } => write!(
                f,
                "Service {service:?} export surface mismatch: parsed={parsed:?}, discovered={discovered:?}"
            ),
            Self::MissingSymbol {
                service,
                export,
                canonical_id,
            } => write!(
                f,
                "Service {service:?} export {export:?} references missing symbol {canonical_id:?}"
            ),
            Self::WrongExportKind {
                service,
                export,
                kind,
            } => write!(
                f,
                "Service {service:?} export {export:?} has non-Service-export kind {kind:?}"
            ),
            Self::MissingFunction { service, export } => write!(
                f,
                "Service {service:?} export {export:?} has no parsed function body"
            ),
        }
    }
}

impl std::error::Error for NativeServiceSelectionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, FunctionDef, MethodDef, Statement};

    fn id(name: &str) -> SourceId {
        SourceId::physical(RelSourceKind::Service, name).unwrap()
    }

    fn bool_fn(name: &str, value: bool) -> FunctionDef {
        FunctionDef {
            name: name.into(),
            params: Vec::new(),
            body: vec![Statement::Return(Expr::Bool(value))],
        }
    }

    fn program(functions: Vec<FunctionDef>, exports: &[&str]) -> ServiceProgram {
        ServiceProgram {
            imports: Vec::new(),
            functions,
            exports: exports.iter().map(|value| (*value).to_string()).collect(),
            class_name: None,
            lifecycle: Vec::new(),
            classes: Vec::new(),
        }
    }

    fn symbol(id: &str, kind: LinkedRelKind) -> LinkedRelSymbolSpec {
        LinkedRelSymbolSpec {
            canonical_id: id.into(),
            kind,
            source_sha256: "a".repeat(64),
            required_symbols: BTreeSet::new(),
            capabilities: BTreeSet::new(),
        }
    }

    fn discovery(exports: &[(&str, &str)], extra: Vec<LinkedRelSymbolSpec>) -> LinkedRelDiscovery {
        let export_map = exports
            .iter()
            .map(|(name, id)| ((*name).to_string(), (*id).to_string()))
            .collect::<BTreeMap<_, _>>();
        let roots = export_map.values().cloned().collect::<BTreeSet<_>>();
        let mut symbols = exports
            .iter()
            .map(|(_, id)| symbol(id, LinkedRelKind::ServiceExport))
            .chain(extra)
            .collect::<Vec<_>>();
        symbols.sort_by(|a, b| a.canonical_id.cmp(&b.canonical_id));
        let reachable_symbols = symbols.iter().map(|s| s.canonical_id.clone()).collect();

        LinkedRelDiscovery {
            symbols,
            service_roots: BTreeMap::from([("demo".into(), roots)]),
            service_exports: BTreeMap::from([("demo".into(), export_map)]),
            reachable_symbols,
        }
    }

    fn services(program: ServiceProgram) -> BTreeMap<String, RuntimeService> {
        BTreeMap::from([(
            "demo".into(),
            RuntimeService {
                source_id: id("demo"),
                program: Arc::new(program),
            },
        )])
    }

    #[test]
    fn fully_lowerable_service_is_selected() {
        let linked = discovery(
            &[
                ("ready", "service_demo_ready"),
                ("healthy", "service_demo_healthy"),
            ],
            Vec::new(),
        );
        let programs = services(program(
            vec![bool_fn("ready", true), bool_fn("healthy", false)],
            &["ready", "healthy"],
        ));

        let selected = select_programs(&linked, &programs, &OidTarget::current()).unwrap();

        assert_eq!(selected.native_services, BTreeSet::from(["demo".into()]));
        assert_eq!(selected.fragments.len(), 2);
        assert_eq!(selected.discovery.symbols.len(), 2);
        assert!(selected.evaluator_fallbacks.is_empty());
    }

    #[test]
    fn one_bad_export_rejects_the_whole_service() {
        let linked = discovery(
            &[
                ("ready", "service_demo_ready"),
                ("payload", "service_demo_payload"),
            ],
            Vec::new(),
        );
        let programs = services(program(
            vec![
                bool_fn("ready", true),
                FunctionDef {
                    name: "payload".into(),
                    params: Vec::new(),
                    body: vec![Statement::Return(Expr::Null)],
                },
            ],
            &["ready", "payload"],
        ));

        let selected = select_programs(&linked, &programs, &OidTarget::current()).unwrap();

        assert!(selected.native_services.is_empty());
        assert!(selected.fragments.is_empty());
        assert!(selected.discovery.symbols.is_empty());
        assert!(selected.evaluator_fallbacks["demo"].contains("payload"));
    }

    #[test]
    fn lifecycle_and_unrelated_symbols_never_leak_into_native_generation() {
        let extra = symbol("module_unused_ready", LinkedRelKind::ModuleExport);
        let mut linked = discovery(&[("ready", "service_demo_ready")], vec![extra]);
        let lifecycle = "service_demo_lifecycle_start".to_string();
        linked.symbols.push(symbol(&lifecycle, LinkedRelKind::Function));
        linked
            .service_roots
            .get_mut("demo")
            .unwrap()
            .insert(lifecycle.clone());
        linked.reachable_symbols.insert(lifecycle);

        let mut service = program(vec![bool_fn("ready", true)], &["ready"]);
        service.lifecycle.push(MethodDef {
            verb: "start".into(),
            param_name: None,
            body: vec![Statement::Return(Expr::Bool(true))],
        });

        let selected = select_programs(&linked, &services(service), &OidTarget::current()).unwrap();

        assert!(selected.native_services.is_empty());
        assert!(selected.discovery.symbols.is_empty());
        assert!(selected.evaluator_fallbacks["demo"].contains("lifecycle"));
    }

    #[test]
    fn unrelated_linked_symbol_is_filtered_from_successful_selection() {
        let linked = discovery(
            &[("ready", "service_demo_ready")],
            vec![symbol("module_unused_ready", LinkedRelKind::ModuleExport)],
        );
        let selected = select_programs(
            &linked,
            &services(program(vec![bool_fn("ready", true)], &["ready"])),
            &OidTarget::current(),
        )
        .unwrap();

        assert_eq!(selected.discovery.symbols.len(), 1);
        assert_eq!(
            selected.discovery.symbols[0].canonical_id,
            "service_demo_ready"
        );
        assert!(!selected
            .discovery
            .reachable_symbols
            .contains("module_unused_ready"));
    }
}
