//! RELC compile product for the native Service/OID pipeline.
//!
//! The compatibility RELC entrypoints continue to return only `RuntimeImage`.
//! Native compilation needs one additional compiler-owned product: the exact
//! linked-REL reachability snapshot derived from the *same* parsed Runtime Image
//! and original source bytes. Keeping that pairing explicit prevents backend or
//! Service code from rebuilding compiler meaning after RELC has finished.

use std::fmt;

use serde_json::Value as JsonValue;

use crate::rel_image_discovery::{
    discover_linked_rel_from_runtime_image, RuntimeImageLinkedRelError,
};
use crate::rel_symbol_discovery::LinkedRelDiscovery;
use crate::relc::{
    compile_runtime_image_with_packages, PackageLinkContext, PhysicalRelSource, RelcError,
};
use crate::runtime_image::RuntimeImage;

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
    let linked_rel = discover_linked_rel_from_runtime_image(
        &image,
        raw_server_source,
        &discovery_sources,
    )
    .map_err(RelcLinkedImageError::LinkedRel)?;

    Ok(RelcLinkedImage { image, linked_rel })
}

#[derive(Debug)]
pub enum RelcLinkedImageError {
    Compile(RelcError),
    LinkedRel(RuntimeImageLinkedRelError),
}

impl fmt::Display for RelcLinkedImageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compile(error) => write!(formatter, "RELC Runtime Image compilation failed: {error}"),
            Self::LinkedRel(error) => {
                write!(formatter, "RELC linked-REL discovery failed after image link: {error}")
            }
        }
    }
}

impl std::error::Error for RelcLinkedImageError {}
