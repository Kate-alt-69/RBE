//! RELC compile product for the native Service/OID pipeline.
//!
//! The compatibility RELC entrypoints continue to return only `RuntimeImage`.
//! Native compilation needs one additional compiler-owned product: the exact
//! linked-REL reachability snapshot derived from the *same* parsed Runtime Image
//! and original source bytes. Keeping that pairing explicit prevents backend or
//! Service code from rebuilding compiler meaning after RELC has finished.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value as JsonValue;

use crate::oid_link::LinkedRelKind;
use crate::rel_image_discovery::{
    discover_linked_rel_from_runtime_image, RuntimeImageLinkedRelError,
};
use crate::rel_symbol_discovery::LinkedRelDiscovery;
use crate::relc::{
    compile_runtime_image_with_packages, PackageLinkContext, PhysicalRelSource, RelcError,
};
use crate::runtime_image::RuntimeImage;
use crate::service_oid::{OID_PACKAGE_END, OID_PACKAGE_START, OID_REL_END, OID_REL_START};

const MAX_CAPACITY_CONTRIBUTORS: usize = 8;

#[derive(Debug)]
pub struct RelcLinkedImage {
    pub image: RuntimeImage,
    pub linked_rel: LinkedRelDiscovery,
}

impl RelcLinkedImage {
    pub fn into_image(self) -> RuntimeImage {
        self.image
    }
}

/// Compile a Runtime Image and its Phase-4 linked-REL snapshot with no installed
/// package roots. This mirrors `relc::compile_runtime_image` while preserving the
/// source bytes needed for native OID identity.
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

/// Compile a Runtime Image plus the exact linked-REL snapshot consumed by the
/// Phase-4 OID allocator. Package resolution remains outside RELC; the supplied
/// context is the same verified root-only view accepted by the existing compiler
/// entrypoint.
pub fn compile_runtime_image_with_packages_and_linked_rel(
    raw_server_source: &str,
    physical_sources: Vec<PhysicalRelSource>,
    settings_json: &JsonValue,
    package_links: &PackageLinkContext,
) -> Result<RelcLinkedImage, RelcLinkedImageError> {
    // RELC owns sorting/parsing of its copy. Keep the caller's exact original
    // source strings available for source-identity hashing after the immutable
    // Runtime Image has been produced.
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

    // Phase 4 allocates only the post-DCE reachable symbol set. Capacity must be
    // checked *after* discovery so dead/private helpers do not inflate the count.
    validate_linked_rel_oid_capacity(&linked_rel).map_err(RelcLinkedImageError::Capacity)?;

    Ok(RelcLinkedImage { image, linked_rel })
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
) -> Result<(), OidCapacityError> {
    let requested = discovery.symbols.len();
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
            Self::LinkedRel => "reachable linked REL targets",
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
    Capacity(OidCapacityError),
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
            Self::Capacity(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RelcLinkedImageError {}

#[cfg(test)]
mod tests {
    use super::*;

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
}
