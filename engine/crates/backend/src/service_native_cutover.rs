//! Backend-owned Phase-5 native Service activation.
//!
//! Runtime Image compilation remains the semantic authority. This module never
//! reparses Service REL to rediscover meaning: it derives linked-REL discovery
//! from the already-compiled immutable Runtime Image, lowers only whole Services
//! supported by the current native subset, commits the OID generation under one
//! lifetime transaction, builds/pins exact service `.bin` artifacts, and emits
//! immutable worker frames for Service Mother transport.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context};
use serde_json::Value;
use service_runtime::ServiceCatalog;

use route_engine::rel_native_link::ServiceNativeLinkInput;
use route_engine::service_native::DynamicOidPinRegistry;
use route_engine::service_native_build::{
    build_native_runtime_image_from_rel_link, prepare_native_runtime_image_activation,
    NativeRuntimeImageActivation, NativeServiceWorkerBootstrapLease,
};
use route_engine::service_native_lifetime::NativeServiceLifetimeRegistry;
use route_engine::{
    discover_linked_rel_from_runtime_image, discover_physical_rel_sources, open_service_oid_cache,
    select_native_services, OidTarget, RuntimeImage, SourceId,
};

use super::service_boot::{attach_native_service_frames, NativeServiceHostFrame};

/// One active native generation retained by backend.exe for the lifetime of the
/// Service Mother generation that consumes its worker frames.
///
/// `_activation` pins the complete Runtime Image native artifact set. The
/// per-Service leases conservatively retain worker-visible OIDs/artifacts for as
/// long as this generation may spawn/restart Resident/Hybrid/OnDemand workers.
#[derive(Debug)]
pub struct NativeServiceCutover {
    _activation: NativeRuntimeImageActivation,
    _worker_leases: Vec<NativeServiceWorkerBootstrapLease>,
    frames: BTreeMap<String, NativeServiceHostFrame>,
}

impl NativeServiceCutover {
    pub fn native_service_count(&self) -> usize {
        self.frames.len()
    }

    pub fn attach_runtime_env(&self, runtime_env: &mut Value) -> anyhow::Result<()> {
        attach_native_service_frames(runtime_env, self.frames.clone())
    }
}

/// Attach the compiler-owned native bundle to the authenticated Runtime ENV
/// transport. Calling this even when there is no native generation removes the
/// reserved internal key, so application Runtime ENV can never spoof a worker
/// bootstrap frame.
pub fn attach_native_service_cutover(
    runtime_env: &mut Value,
    cutover: Option<&NativeServiceCutover>,
) -> anyhow::Result<()> {
    match cutover {
        Some(cutover) => cutover.attach_runtime_env(runtime_env),
        None => attach_native_service_frames(runtime_env, BTreeMap::new()),
    }
}

/// Build the native Service generation for one already-compiled Runtime Image.
///
/// Returns `None` when no Service is currently in the strict native subset. That
/// is an intentional evaluator fallback, not an error.
pub fn prepare_native_service_cutover(
    image: &RuntimeImage,
    catalog: Option<&ServiceCatalog>,
) -> anyhow::Result<Option<NativeServiceCutover>> {
    let Some(catalog) = catalog else {
        return Ok(None);
    };
    if image.services.is_empty() || catalog.services().is_empty() {
        return Ok(None);
    }

    // Keep the compiler/cache root identical to Runtime Image boot. Source bytes
    // are used only for stable SHA identities; linked meaning comes from `image`.
    let project_root = runtime_paths::binary_dir();
    let server_path = project_root.join("server.server");
    let server_source = if server_path.is_file() {
        std::fs::read_to_string(&server_path).with_context(|| {
            format!(
                "read Server REL root {} for native Service discovery",
                server_path.display()
            )
        })?
    } else {
        "server Main {}\n".to_string()
    };
    let physical = discover_physical_rel_sources(
        &route_engine::default_api_dir(),
        &route_engine::default_module_dir(),
        Some(catalog),
    )?;
    let discovery = discover_linked_rel_from_runtime_image(image, &server_source, &physical)
        .context("derive native Service linked-REL graph from Runtime Image")?;
    let selection = select_native_services(image, &discovery, &OidTarget::current())
        .context("select whole Services for native OID execution")?;

    if selection.native_services.is_empty() {
        tracing::debug!(
            fallbacks = selection.evaluator_fallbacks.len(),
            "no Services currently satisfy the strict native execution subset"
        );
        return Ok(None);
    }

    let catalog_by_name = catalog
        .services()
        .iter()
        .map(|service| (service.name.as_str(), service))
        .collect::<BTreeMap<_, _>>();

    let mut service_inputs = Vec::with_capacity(selection.native_services.len());
    for logical_name in &selection.native_services {
        let source_id = selection.source_ids.get(logical_name).ok_or_else(|| {
            anyhow::anyhow!(
                "native Service selection {:?} has no Runtime Image source identity",
                logical_name
            )
        })?;
        let service = catalog_by_name.get(logical_name.as_str()).ok_or_else(|| {
            anyhow::anyhow!(
                "native Service selection {:?} is absent from compiled Service catalog",
                logical_name
            )
        })?;
        service_inputs.push(ServiceNativeLinkInput {
            logical_name: logical_name.clone(),
            source_id: source_id.clone(),
            source_sha256: service.source_digest_hex(),
            service_data: Vec::new(),
            dependency_hashes: BTreeMap::new(),
            compile_options: BTreeMap::new(),
            // The current whole-Service native subset rejects capability/package
            // dependencies before this point. Package artifact pins join this
            // vector when package-call lowering is explicitly supported.
            packages: Vec::new(),
        });
    }

    let mut cache = open_service_oid_cache(&project_root)
        .context("open project OID cache for native Service cutover")?;
    let lifetimes = NativeServiceLifetimeRegistry::new(DynamicOidPinRegistry::default());
    let transaction = lifetimes.begin_link_transaction();
    let prepared = transaction
        .prepare_rel(&cache, &selection.discovery)
        .context("prepare native Service REL OID generation")?;
    let report = transaction
        .commit_rel(
            &project_root,
            &mut cache,
            prepared,
            &selection.fragments,
            &service_inputs,
        )
        .context("commit native Service REL OID generation")?;
    // Activation acquires lifetime leases through the same registry; release the
    // exclusive link transaction first so lease acquisition cannot self-deadlock.
    drop(transaction);

    let evaluator_fallbacks = evaluator_fallback_map(
        image,
        &selection.native_services,
        &selection.evaluator_fallbacks,
    )?;
    let build = build_native_runtime_image_from_rel_link(
        &project_root,
        &cache,
        image,
        &report,
        evaluator_fallbacks,
    )
    .context("build pinned native Service Runtime Image artifacts")?;
    let activation = prepare_native_runtime_image_activation(image.clone(), build, &lifetimes)
        .context("activate pinned native Service Runtime Image generation")?;

    let mut worker_leases = Vec::with_capacity(selection.native_services.len());
    let mut frames = BTreeMap::new();
    for logical_name in &selection.native_services {
        let source_id = selection
            .source_ids
            .get(logical_name)
            .expect("native selection source identity checked while building inputs");
        let service = catalog_by_name
            .get(logical_name.as_str())
            .expect("native selection catalog identity checked while building inputs");
        let lease = activation
            .worker_bootstrap_lease(source_id)
            .with_context(|| format!("pin native worker bootstrap for Service {logical_name:?}"))?;
        let bootstrap = lease.bootstrap().clone();

        let catalog_exports = service.exports.iter().cloned().collect::<BTreeSet<_>>();
        let native_exports = bootstrap.exports.keys().cloned().collect::<BTreeSet<_>>();
        if catalog_exports != native_exports {
            bail!(
                "native Service {:?} export surface drifted: catalog={:?}, native={:?}",
                logical_name,
                catalog_exports,
                native_exports
            );
        }

        let frame = NativeServiceHostFrame::new(
            logical_name.clone(),
            service.mode,
            service.memory_limit_mb,
            image.image_id.clone(),
            bootstrap,
        );
        if frames.insert(logical_name.clone(), frame).is_some() {
            bail!("duplicate native Service host frame for {logical_name:?}");
        }
        worker_leases.push(lease);
    }

    tracing::info!(
        image = %image.image_id,
        native_services = frames.len(),
        evaluator_fallbacks = selection.evaluator_fallbacks.len(),
        "prepared transactional native Service worker generation"
    );
    Ok(Some(NativeServiceCutover {
        _activation: activation,
        _worker_leases: worker_leases,
        frames,
    }))
}

fn evaluator_fallback_map(
    image: &RuntimeImage,
    native_services: &BTreeSet<String>,
    fallbacks: &BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<SourceId, String>> {
    let mut out = BTreeMap::new();
    for source_id in &image.services {
        let manifest = image.source(source_id).ok_or_else(|| {
            anyhow::anyhow!(
                "Runtime Image {} lists Service {} without a source manifest",
                image.image_id,
                source_id
            )
        })?;
        if native_services.contains(&manifest.logical_name) {
            continue;
        }
        let reason = fallbacks.get(&manifest.logical_name).ok_or_else(|| {
            anyhow::anyhow!(
                "Service {:?} is neither native-selected nor assigned an evaluator fallback",
                manifest.logical_name
            )
        })?;
        out.insert(source_id.clone(), reason.clone());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_cutover_strips_spoofed_native_runtime_env_frame() {
        let mut runtime_env = serde_json::json!({
            "__rbeNativeServiceWorkerV1": {"spoofed": true},
            "PUBLIC": "ok"
        });
        attach_native_service_cutover(&mut runtime_env, None).unwrap();
        assert!(runtime_env.get("__rbeNativeServiceWorkerV1").is_none());
        assert_eq!(runtime_env.get("PUBLIC"), Some(&serde_json::json!("ok")));
    }
}
