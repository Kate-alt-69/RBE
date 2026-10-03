//! Phase 5 compiler-side native Service build transaction.
//!
//! This module joins the Phase-4 OID adapter/assembler with Phase-5 Runtime
//! Image pinning. A successful build always means the exact same verified OID
//! cache snapshot produced the Service plan, native `.bin`, and immutable pin.
//! No worker execution policy lives here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::runtime_image::RuntimeImage;
use crate::service_native::{
    load_pinned_service_bin, NativeRuntimeImagePins, NativeServiceArtifactPin, PackageArtifactPin,
    ServiceBinCacheLookup, ServiceNativeError,
};
use crate::service_oid::OidCache;
use crate::service_oid_adapter::{
    assemble_service_from_oid_cache, build_service_plan_from_oid_cache, write_service_plan_atomic,
    ServiceOidAdapterError,
};
use crate::source_registry::{RelSourceKind, SourceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeServiceBuildSpec {
    pub source_id: SourceId,
    pub service_source_sha256: String,
    pub entry_oids: Vec<u16>,
    pub service_data: Vec<u8>,
    pub dependency_hashes: BTreeMap<String, String>,
    pub compile_options: BTreeMap<String, String>,
    pub packages: Vec<PackageArtifactPin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltNativeService {
    pub pin: NativeServiceArtifactPin,
    pub plan_path: PathBuf,
    pub bin_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRuntimeImageBuild {
    pub pins: NativeRuntimeImagePins,
    pub services: BTreeMap<SourceId, BuiltNativeService>,
}

/// Build one native Service artifact against one immutable in-memory OID cache
/// snapshot. The generated plan and `.bin` are disposable cache artifacts; the
/// returned pin is the authority selected for Runtime Image activation.
pub fn build_native_service_artifact(
    project_root: &Path,
    cache: &OidCache,
    image: &RuntimeImage,
    spec: NativeServiceBuildSpec,
) -> Result<BuiltNativeService, NativeServiceBuildError> {
    validate_service_target(image, &spec.source_id)?;

    let dependency_hashes =
        bind_package_dependencies(spec.dependency_hashes, &spec.packages, &spec.source_id)?;

    let plan = build_service_plan_from_oid_cache(
        cache,
        spec.source_id.as_str(),
        spec.service_source_sha256,
        spec.entry_oids,
        spec.service_data,
        dependency_hashes,
        spec.compile_options,
    )
    .map_err(NativeServiceBuildError::Adapter)?;

    let (plan_hash, plan_path) =
        write_service_plan_atomic(project_root, &plan).map_err(NativeServiceBuildError::Adapter)?;
    let (bin, bin_path) = assemble_service_from_oid_cache(project_root, cache, &plan)
        .map_err(NativeServiceBuildError::Adapter)?;

    let pin = NativeServiceArtifactPin::from_assembled(
        spec.source_id.clone(),
        cache.index().generation,
        plan,
        &bin,
        spec.packages,
    )
    .map_err(NativeServiceBuildError::Native)?;

    if pin.plan_hash != plan_hash {
        return Err(NativeServiceBuildError::ArtifactDrift(format!(
            "Service {} plan cache wrote hash {plan_hash}, pin resolved {}",
            spec.source_id, pin.plan_hash
        )));
    }

    let compiler_cache_root = project_root.join(".cache/compiler");
    let expected_bin_path = pin
        .cache_path(&compiler_cache_root)
        .map_err(NativeServiceBuildError::Native)?;
    if bin_path != expected_bin_path {
        return Err(NativeServiceBuildError::ArtifactDrift(format!(
            "Service {} assembler wrote {}, pin resolves {}",
            spec.source_id,
            bin_path.display(),
            expected_bin_path.display()
        )));
    }

    match load_pinned_service_bin(&compiler_cache_root, &pin)
        .map_err(NativeServiceBuildError::Native)?
    {
        ServiceBinCacheLookup::Hit(_) => {}
        ServiceBinCacheLookup::Miss => {
            return Err(NativeServiceBuildError::ArtifactDrift(format!(
                "Service {} native bin disappeared immediately after atomic assembly",
                spec.source_id
            )))
        }
        ServiceBinCacheLookup::CorruptEvicted { reason, .. } => {
            return Err(NativeServiceBuildError::ArtifactDrift(format!(
                "Service {} native bin failed post-write verification: {reason}",
                spec.source_id
            )))
        }
    }

    Ok(BuiltNativeService {
        pin,
        plan_path,
        bin_path,
    })
}

/// Re-open the exact Service `.bin` selected by a pin. A missing/corrupt cache
/// entry is rebuilt once from the already-pinned plan plus exact OID records.
/// If the current OID index no longer matches that plan, rebuilding fails closed
/// instead of silently assembling against a newer meaning.
pub fn ensure_pinned_service_bin(
    project_root: &Path,
    cache: &OidCache,
    pin: &NativeServiceArtifactPin,
) -> Result<PathBuf, NativeServiceBuildError> {
    let compiler_cache_root = project_root.join(".cache/compiler");
    match load_pinned_service_bin(&compiler_cache_root, pin)
        .map_err(NativeServiceBuildError::Native)?
    {
        ServiceBinCacheLookup::Hit(_) => {
            return pin
                .cache_path(&compiler_cache_root)
                .map_err(NativeServiceBuildError::Native)
        }
        ServiceBinCacheLookup::Miss | ServiceBinCacheLookup::CorruptEvicted { .. } => {}
    }

    let (rebuilt, path) = assemble_service_from_oid_cache(project_root, cache, &pin.plan)
        .map_err(NativeServiceBuildError::Adapter)?;
    if rebuilt.assembly_hash != pin.assembly_hash {
        return Err(NativeServiceBuildError::ArtifactDrift(format!(
            "Service {} rebuild produced assembly {} instead of pinned {}",
            pin.source_id, rebuilt.assembly_hash, pin.assembly_hash
        )));
    }

    match load_pinned_service_bin(&compiler_cache_root, pin)
        .map_err(NativeServiceBuildError::Native)?
    {
        ServiceBinCacheLookup::Hit(_) => Ok(path),
        ServiceBinCacheLookup::Miss => Err(NativeServiceBuildError::ArtifactDrift(format!(
            "Service {} rebuilt native bin is still missing",
            pin.source_id
        ))),
        ServiceBinCacheLookup::CorruptEvicted { reason, .. } => {
            Err(NativeServiceBuildError::ArtifactDrift(format!(
                "Service {} rebuilt native bin is still invalid: {reason}",
                pin.source_id
            )))
        }
    }
}

/// Build every native Service selected for Image B before activation. Missing
/// native specs are allowed only when the caller supplies an explicit evaluator
/// fallback reason. This preserves the A-running/B-compiling transaction: this
/// function never mutates the active Runtime Image slot.
pub fn build_native_runtime_image_pins(
    project_root: &Path,
    cache: &OidCache,
    image: &RuntimeImage,
    specs: Vec<NativeServiceBuildSpec>,
    evaluator_fallbacks: BTreeMap<SourceId, String>,
) -> Result<NativeRuntimeImageBuild, NativeServiceBuildError> {
    let mut seen = BTreeSet::new();
    let mut services = BTreeMap::new();
    let mut pins = BTreeMap::new();

    for spec in specs {
        if !seen.insert(spec.source_id.clone()) {
            return Err(NativeServiceBuildError::DuplicateService(spec.source_id));
        }
        let built = build_native_service_artifact(project_root, cache, image, spec)?;
        let source_id = built.pin.source_id.clone();
        pins.insert(source_id.clone(), built.pin.clone());
        services.insert(source_id, built);
    }

    let image_pins = NativeRuntimeImagePins {
        runtime_image_id: image.image_id.clone(),
        services: pins,
        evaluator_fallbacks,
    };
    image_pins
        .validate_for_image(image)
        .map_err(NativeServiceBuildError::Native)?;

    Ok(NativeRuntimeImageBuild {
        pins: image_pins,
        services,
    })
}

fn validate_service_target(
    image: &RuntimeImage,
    source_id: &SourceId,
) -> Result<(), NativeServiceBuildError> {
    let source = image.source(source_id).ok_or_else(|| {
        NativeServiceBuildError::InvalidService(format!(
            "Service {source_id} is not present in Runtime Image {}",
            image.image_id
        ))
    })?;
    if source.kind != RelSourceKind::Service {
        return Err(NativeServiceBuildError::InvalidService(format!(
            "native Service build target {source_id} is {}",
            source.kind
        )));
    }
    if !image.services.contains(source_id) {
        return Err(NativeServiceBuildError::InvalidService(format!(
            "Service {source_id} is not in the Runtime Image service set"
        )));
    }
    Ok(())
}

fn bind_package_dependencies(
    mut dependencies: BTreeMap<String, String>,
    packages: &[PackageArtifactPin],
    source_id: &SourceId,
) -> Result<BTreeMap<String, String>, NativeServiceBuildError> {
    let mut seen = BTreeSet::new();
    for package in packages {
        let key = package.dependency_key();
        if !seen.insert(key.clone()) {
            return Err(NativeServiceBuildError::DependencyConflict(format!(
                "Service {source_id} repeats package dependency {key}"
            )));
        }
        match dependencies.get(&key) {
            Some(existing) if existing != &package.artifact_sha256 => {
                return Err(NativeServiceBuildError::DependencyConflict(format!(
                    "Service {source_id} dependency {key} already pins {existing}, package pin requests {}",
                    package.artifact_sha256
                )))
            }
            _ => {
                dependencies.insert(key, package.artifact_sha256.clone());
            }
        }
    }
    Ok(dependencies)
}

#[derive(Debug)]
pub enum NativeServiceBuildError {
    Adapter(ServiceOidAdapterError),
    Native(ServiceNativeError),
    DuplicateService(SourceId),
    InvalidService(String),
    DependencyConflict(String),
    ArtifactDrift(String),
}

impl fmt::Display for NativeServiceBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Adapter(error) => {
                write!(formatter, "native Service compiler adapter failed: {error}")
            }
            Self::Native(error) => {
                write!(
                    formatter,
                    "native Service pin/cache validation failed: {error}"
                )
            }
            Self::DuplicateService(source) => {
                write!(
                    formatter,
                    "native Runtime Image build repeats Service {source}"
                )
            }
            Self::InvalidService(message)
            | Self::DependencyConflict(message)
            | Self::ArtifactDrift(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for NativeServiceBuildError {}
