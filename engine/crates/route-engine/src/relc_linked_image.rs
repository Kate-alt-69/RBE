//! RELC compile product for the native Service/OID pipeline.
//!
//! The compatibility RELC entrypoints continue to return only `RuntimeImage`.
//! Native compilation needs additional compiler-owned products: the exact
//! linked-REL reachability snapshot, Container Task metadata, and the
//! whole-Service native selection derived from the *same* parsed Runtime Image
//! and original source bytes. Keeping those products paired prevents backend,
//! Container, or Service code from rebuilding compiler meaning after RELC has
//! finished.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value as JsonValue;

pub mod container_task_plan;
pub use container_task_plan::{
    discover_container_tasks, ContainerTaskBoundaryHint, ContainerTaskCapabilitySlot,
    ContainerTaskCodeRef, ContainerTaskDiscovery, ContainerTaskDiscoveryError, ContainerTaskGraph,
    ContainerTaskKind, ContainerTaskNode, ContainerTaskOidRequest, ContainerTaskPlan,
    ContainerTaskSourceFile, ContainerTaskSourceSite,
};

use crate::oid_link::LinkedRelKind;
use crate::rel_image_discovery::{
    discover_linked_rel_from_runtime_image, RuntimeImageLinkedRelError,
};
use crate::rel_native_service_select::{
    select_native_services, NativeServiceSelection, NativeServiceSelectionError,
};
use crate::rel_symbol_discovery::LinkedRelDiscovery;
use crate::relc::{
    compile_runtime_image_with_packages, PackageLinkContext, PhysicalRelSource, RelcError,
};
use crate::runtime_image::RuntimeImage;
use crate::service_oid::{
    OidTarget, OID_PACKAGE_END, OID_PACKAGE_START, OID_REL_END, OID_REL_START,
};

const MAX_CAPACITY_CONTRIBUTORS: usize = 8;

#[derive(Debug)]
pub struct RelcLinkedImage {
    pub image: RuntimeImage,
    pub linked_rel: LinkedRelDiscovery,
    /// Phase-2 compiler-owned workload metadata for Container Task Image
    /// assembly. This is still pre-assembly: no `.bin` is written and no
    /// project-local OID index mutation happens in this phase.
    pub container_tasks: ContainerTaskDiscovery,
    /// Phase-2 compiler output for the Services that can be lowered completely
    /// with the current native subset. Phase 3/4 consumes the fragments and
    /// filtered discovery without rediscovering source or reparsing Service REL.
    pub native_services: NativeServiceSelection,
}

impl RelcLinkedImage {
    pub fn into_image(self) -> RuntimeImage {
        self.image
    }
}

/// Compile a Runtime Image and its linked-REL/native-Service/Container-Task
/// compiler products with no installed package roots. This mirrors
/// `relc::compile_runtime_image` while preserving source bytes needed for native
/// OID identity and CTI source/error metadata.
pub fn compile_runtime_image_with_linked_rel(
    raw_server_source: &str,
    physical_sources: Vec<PhysicalRelSource>,
    settings_json: &JsonValue,
) -> Result<RelcLinkedImage, RelcLinkedImageError> {
    compile_runtime_image_with_packages_and_linked_rel(
        raw_server_source,
        physical_sources,
        settings_json,
        &PackageLinkContext::default(),
    )
}

/// Compile a Runtime Image plus the exact linked-REL snapshot, Container Task
/// discovery, and Phase-2 native-Service selection consumed by later OID/CTI
/// phases.
///
/// Package resolution remains outside RELC; the supplied context is the same
/// verified root-only view accepted by the existing compiler entrypoint.
pub fn compile_runtime_image_with_packages_and_linked_rel(
    raw_server_source: &str,
    physical_sources: Vec<PhysicalRelSource>,
    settings_json: &JsonValue,
    package_links: &PackageLinkContext,
) -> Result<RelcLinkedImage, RelcLinkedImageError> {
    // RELC owns sorting/parsing of its copy. Keep the caller's exact original
    // source strings available for source-identity hashing and CTI source maps
    // after the immutable Runtime Image has been produced.
    let discovery_sources = physical_sources.clone();
    let image = compile_runtime_image_with_packages(
        raw_server_source,
        physical_sources,
        settings_json,
        package_links,
    )
    .map_err(RelcLinkedImageError::Compile)?;

    // Package OIDs are allocated for verified public exports, not merely the
    // subset imported by one Service. Fail with useful counts before touching
    // the project-local OID index if the fixed package range cannot represent
    // the verified root surface at all.
    validate_package_oid_capacity(package_links).map_err(RelcLinkedImageError::Capacity)?;

    let linked_rel =
        discover_linked_rel_from_runtime_image(&image, raw_server_source, &discovery_sources)
            .map_err(RelcLinkedImageError::LinkedRel)?;

    // CTI Task discovery consumes compiler facts only. It does not allocate an
    // OID or assemble a `.bin`; instead it emits canonical Task OID requests,
    // DAG metadata, capability slots, logging dictionaries and source/error maps
    // for the later assembler/index phases.
    let container_tasks =
        discover_container_tasks(&image, &linked_rel, raw_server_source, &discovery_sources)
            .map_err(RelcLinkedImageError::ContainerTask)?;

    // Phase 4 allocates only the post-DCE reachable symbol set plus the Task OID
    // identities emitted above. Capacity must be checked after discovery so
    // dead/private helpers do not inflate the count, while CTI roots still reserve
    // space in the same authoritative REL OID range.
    validate_linked_rel_oid_capacity(&linked_rel, container_tasks.tasks.len())
        .map_err(RelcLinkedImageError::Capacity)?;

    // Phase 2 lowers only whole Services whose complete executable root surface
    // is supported. Unsupported Services remain explicit evaluator fallbacks and
    // emit no placeholder fragments. This selection is compiler-owned so later
    // phases never need to reparse source or recreate symbol meaning.
    let native_services = select_native_services(&image, &linked_rel, &OidTarget::current())
        .map_err(RelcLinkedImageError::NativeSelection)?;

    Ok(RelcLinkedImage {
        image,
        linked_rel,
        container_tasks,
        native_services,
    })
}

fn validate_package_oid_capacity(links: &PackageLinkContext) -> Result<(), OidCapacityError> {
    let requested = links
        .roots
        .values()
        .map(|root| root.exports.len())
        .sum::<usize>();
    let capacity = oid_capacity(OID_PACKAGE_START, OID_PACKAGE_END);
    if requested <= capacity {
        return Ok(());
    }

    let contributors = ranked_contributors(
        links
            .roots
            .iter()
            .map(|(package, root)| (package.clone(), root.exports.len())),
    );
    Err(OidCapacityError {
        space: OidCapacitySpace::Package,
        requested,
        capacity,
        contributors,
    })
}

fn validate_linked_rel_oid_capacity(
    discovery: &LinkedRelDiscovery,
    container_task_count: usize,
) -> Result<(), OidCapacityError> {
    let requested = discovery.symbols.len().saturating_add(container_task_count);
    let capacity = oid_capacity(OID_REL_START, OID_REL_END);
    if requested <= capacity {
        return Ok(());
    }

    let mut by_kind = BTreeMap::<&'static str, usize>::new();
    for symbol in &discovery.symbols {
        *by_kind
            .entry(linked_rel_kind_label(symbol.kind))
            .or_default() += 1;
    }
    if container_task_count > 0 {
        by_kind.insert("container_tasks", container_task_count);
    }
    let contributors = ranked_contributors(
        by_kind
            .into_iter()
            .map(|(kind, count)| (kind.to_string(), count)),
    );
    Err(OidCapacityError {
        space: OidCapacitySpace::LinkedRel,
        requested,
        capacity,
        contributors,
    })
}

fn oid_capacity(start: u16, end: u16) -> usize {
    usize::from(end) - usize::from(start) + 1
}

fn linked_rel_kind_label(kind: LinkedRelKind) -> &'static str {
    match kind {
        LinkedRelKind::Function => "functions",
        LinkedRelKind::FirstClassFunction => "first_class_functions",
        LinkedRelKind::ModuleExport => "module_exports",
        LinkedRelKind::RouteExport => "route_exports",
        LinkedRelKind::ServiceExport => "service_exports",
        LinkedRelKind::Class => "class_descriptors",
        LinkedRelKind::Constructor => "constructors",
        LinkedRelKind::Method => "methods",
    }
}

fn ranked_contributors(
    contributors: impl IntoIterator<Item = (String, usize)>,
) -> Vec<OidCapacityContributor> {
    let mut contributors = contributors
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(label, count)| OidCapacityContributor { label, count })
        .collect::<Vec<_>>();
    contributors.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.label.cmp(&right.label))
    });
    contributors.truncate(MAX_CAPACITY_CONTRIBUTORS);
    contributors
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OidCapacitySpace {
    Package,
    LinkedRel,
}

impl OidCapacitySpace {
    fn label(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::LinkedRel => "linked REL",
        }
    }

    fn target_label(self) -> &'static str {
        match self {
            Self::Package => "reachable package operations",
            Self::LinkedRel => "reachable linked REL targets plus Container Tasks",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidCapacityContributor {
    pub label: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidCapacityError {
    pub space: OidCapacitySpace,
    pub requested: usize,
    pub capacity: usize,
    pub contributors: Vec<OidCapacityContributor>,
}

impl fmt::Display for OidCapacityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "OID {} space exhausted:", self.space.label())?;
        writeln!(
            formatter,
            "{}: {}",
            self.space.target_label(),
            self.requested
        )?;
        writeln!(formatter, "capacity: {}", self.capacity)?;
        if !self.contributors.is_empty() {
            writeln!(formatter, "largest contributors:")?;
            for contributor in &self.contributors {
                writeln!(formatter, "  {}: {}", contributor.label, contributor.count)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for OidCapacityError {}

#[derive(Debug)]
pub enum RelcLinkedImageError {
    Compile(RelcError),
    LinkedRel(RuntimeImageLinkedRelError),
    ContainerTask(ContainerTaskDiscoveryError),
    Capacity(OidCapacityError),
    NativeSelection(NativeServiceSelectionError),
}

impl fmt::Display for RelcLinkedImageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compile(error) => {
                write!(formatter, "RELC Runtime Image compilation failed: {error}")
            }
            Self::LinkedRel(error) => {
                write!(
                    formatter,
                    "RELC linked-REL discovery failed after image link: {error}"
                )
            }
            Self::ContainerTask(error) => {
                write!(formatter, "RELC Container Task discovery failed: {error}")
            }
            Self::Capacity(error) => write!(formatter, "{error}"),
            Self::NativeSelection(error) => {
                write!(formatter, "RELC native Service selection failed: {error}")
            }
        }
    }
}

impl std::error::Error for RelcLinkedImageError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_registry::RelSourceKind;

    #[test]
    fn fixed_dynamic_capacities_match_the_frozen_range_map() {
        assert_eq!(oid_capacity(OID_PACKAGE_START, OID_PACKAGE_END), 10_371);
        assert_eq!(oid_capacity(OID_REL_START, OID_REL_END), 35_078);
    }

    #[test]
    fn contributor_ranking_is_largest_first_and_deterministic() {
        let ranked = ranked_contributors([
            ("small".to_string(), 2),
            ("zeta".to_string(), 7),
            ("alpha".to_string(), 7),
            ("empty".to_string(), 0),
        ]);
        assert_eq!(
            ranked,
            vec![
                OidCapacityContributor {
                    label: "alpha".into(),
                    count: 7,
                },
                OidCapacityContributor {
                    label: "zeta".into(),
                    count: 7,
                },
                OidCapacityContributor {
                    label: "small".into(),
                    count: 2,
                },
            ]
        );
    }

    #[test]
    fn capacity_error_renders_requested_capacity_and_contributors() {
        let error = OidCapacityError {
            space: OidCapacitySpace::Package,
            requested: 10_502,
            capacity: 10_371,
            contributors: vec![OidCapacityContributor {
                label: "giant-sdk".into(),
                count: 8_900,
            }],
        };
        let rendered = error.to_string();
        assert!(rendered.contains("OID package space exhausted:"));
        assert!(rendered.contains("reachable package operations: 10502"));
        assert!(rendered.contains("capacity: 10371"));
        assert!(rendered.contains("giant-sdk: 8900"));
    }

    #[test]
    fn container_task_hash_ignores_unreachable_function_changes() {
        fn compile(source: &str) -> RelcLinkedImage {
            compile_runtime_image_with_linked_rel(
                "server Main {}",
                vec![PhysicalRelSource::new(
                    RelSourceKind::Service,
                    "demo",
                    "services/demo.service",
                    source,
                )],
                &serde_json::json!({}),
            )
            .expect("compile CTI task metadata")
        }

        let first = compile(
            r#":service[name = demo]
               export function ready() { return true; }
               function unused() { return 1; }"#,
        );
        let unrelated_changed = compile(
            r#":service[name = demo]
               export function ready() { return true; }
               function unused() { return 999; }"#,
        );
        let reachable_changed = compile(
            r#":service[name = demo]
               export function ready() { return false; }
               function unused() { return 999; }"#,
        );

        let first_hash = &first.container_tasks.tasks[0].semantic_sha256;
        assert_eq!(
            first_hash, &unrelated_changed.container_tasks.tasks[0].semantic_sha256,
            "unreachable same-file helpers must not invalidate a Task"
        );
        assert_ne!(
            first_hash, &reachable_changed.container_tasks.tasks[0].semantic_sha256,
            "reachable body changes must invalidate a Task"
        );
    }

    #[test]
    fn compiler_product_carries_native_service_and_container_task_products() {
        let service = PhysicalRelSource::new(
            RelSourceKind::Service,
            "demo",
            "services/demo.service",
            r#"
                :service[name = demo]
                export function ready() {
                    return true;
                }
            "#,
        );

        let product = compile_runtime_image_with_linked_rel(
            "server Main {}",
            vec![service],
            &serde_json::json!({}),
        )
        .expect("compile linked Runtime Image with native Service and CTI Task selection");

        assert!(product.native_services.native_services.contains("demo"));
        assert_eq!(product.native_services.fragments.len(), 1);
        assert!(product.native_services.evaluator_fallbacks.is_empty());
        assert_eq!(product.native_services.discovery.service_exports.len(), 1);

        assert_eq!(product.container_tasks.tasks.len(), 1);
        let task = &product.container_tasks.tasks[0];
        assert_eq!(task.kind, ContainerTaskKind::ServiceExport);
        assert_eq!(task.canonical_id, "task_service_demo_ready");
        assert_eq!(task.task_oid, None);
        assert_eq!(task.graph.nodes[0].kind, core_lib::CtiNodeKind::WasmBlock);
        assert_eq!(task.graph.nodes[1].kind, core_lib::CtiNodeKind::Return);
        assert_eq!(task.graph.nodes[2].kind, core_lib::CtiNodeKind::Fail);
        assert!(!task.semantic_sha256.is_empty());
        assert!(task.required_oid_symbols.contains("service_demo_ready"));
        assert!(task.log_events.entries().len() >= 3);
    }
}
