use std::collections::BTreeMap;

use rbe_install_orchestrator::{gate_activation, InstallSession};
use rbe_install_request::RegistryPackageIndex;
use rbe_library_package::HostOs;
use rbe_package_attestation::{AttestationPolicy, PackageAttestationInput, ReportScope};

use crate::{InstallActivationProof, VerifiedRootGraph};

pub fn prepare_prebuilt_activation_proofs(
    graph: &VerifiedRootGraph,
    indexes: &BTreeMap<String, RegistryPackageIndex>,
) -> Result<BTreeMap<String, InstallActivationProof>, PrebuiltPreparationError> {
    let host_os = current_host_os();
    let host = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let mut proofs = BTreeMap::new();

    for package in &graph.install_order {
        let verified = graph.packages.get(package).ok_or_else(|| {
            PrebuiltPreparationError::VerifiedGraphPackageMissing {
                root: graph.root.clone(),
                package: package.clone(),
            }
        })?;
        let build_steps = verified.manifest.build.for_host(host_os);
        if !build_steps.is_empty() {
            return Err(PrebuiltPreparationError::ManagedBuildRequired {
                package: package.clone(),
                steps: build_steps.len(),
            });
        }

        let locked = graph
            .lock
            .locked_for_root(&graph.root, package)
            .ok_or_else(|| PrebuiltPreparationError::VerifiedGraphPackageMissing {
                root: graph.root.clone(),
                package: package.clone(),
            })?;
        let index = indexes
            .get(package)
            .ok_or_else(|| PrebuiltPreparationError::RegistryIndexMissing {
                package: package.clone(),
            })?;
        index.validate_for(package)?;
        let release = index
            .releases
            .iter()
            .find(|release| release.version == locked.version)
            .ok_or_else(|| PrebuiltPreparationError::RegistryReleaseMissing {
                package: package.clone(),
                version: locked.version.clone(),
            })?;

        let input = PackageAttestationInput {
            package: package.clone(),
            version: locked.version.clone(),
            scope: ReportScope::PublicRegistry,
            locked_artifact_sha256: locked.artifact_sha256.clone(),
            downloaded_artifact_sha256: verified.stage.verified.sha256.clone(),
            local_source_sha256: None,
            remote_source_sha256: release.artifact.source_sha256.clone(),
            publisher_signature_declared: release.artifact.signature.is_some(),
            publisher_signature_verified: false,
            reproducible_build: release.artifact.reproducible_build,
            shipped_binary_sha256: release.artifact.shipped_binary_sha256.clone(),
            rebuilt_binary_sha256: None,
            build_id: format!(
                "prebuilt:{package}:{}",
                &verified.stage.verified.sha256[..16]
            ),
            host: host.clone(),
        };
        let gate = gate_activation(&input, AttestationPolicy::default())?;
        let instance = if package == &graph.root {
            package.clone()
        } else {
            InstallSession::private_instance_id(&graph.root, package)?
        };
        if proofs
            .insert(
                instance.clone(),
                InstallActivationProof {
                    observed_artifact_sha256: verified.stage.verified.sha256.clone(),
                    build_id: None,
                    gate,
                },
            )
            .is_some()
        {
            return Err(PrebuiltPreparationError::DuplicateActivationProof(instance));
        }
    }

    if proofs.len() != graph.packages.len() {
        return Err(PrebuiltPreparationError::ActivationProofCountMismatch {
            expected: graph.packages.len(),
            actual: proofs.len(),
        });
    }
    Ok(proofs)
}

const fn current_host_os() -> HostOs {
    if cfg!(target_os = "windows") {
        HostOs::Windows
    } else if cfg!(target_os = "linux") {
        HostOs::Linux
    } else if cfg!(target_os = "macos") {
        HostOs::Macos
    } else {
        HostOs::Other
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PrebuiltPreparationError {
    #[error(transparent)]
    RegistryContract(#[from] rbe_install_request::RegistryContractError),
    #[error(transparent)]
    Orchestrator(#[from] rbe_install_orchestrator::OrchestratorError),
    #[error(transparent)]
    Session(#[from] rbe_install_orchestrator::SessionError),
    #[error("verified root graph {root:?} is missing staged package {package:?}")]
    VerifiedGraphPackageMissing { root: String, package: String },
    #[error("hydrated registry graph is missing package index {package:?}")]
    RegistryIndexMissing { package: String },
    #[error("registry index for {package:?} is missing resolved release {version:?}")]
    RegistryReleaseMissing { package: String, version: String },
    #[error("package {package:?} requires {steps} managed build step(s) on this host")]
    ManagedBuildRequired { package: String, steps: usize },
    #[error("duplicate activation proof for package instance {0:?}")]
    DuplicateActivationProof(String),
    #[error("activation proof count mismatch: expected {expected}, got {actual}")]
    ActivationProofCountMismatch { expected: usize, actual: usize },
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rbe_install_executor::{PromotionPlan, VerifiedDownload};
    use rbe_install_request::{
        RegistryArtifact, RegistryPackageRelease, REGISTRY_PACKAGE_INDEX_FORMAT,
    };
    use rbe_library_package::{
        BuildSpec, BuildStep, Language, LibraryManifest, RuntimeSpec, SdkSpec,
    };
    use rbe_project_package::{LockedProjectPackage, LockedToolchain, ProjectPackageLock};

    use super::*;
    use crate::{ArtifactStage, VerifiedRegistryPackage};

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn fixture(
        build: BuildSpec,
        signature: Option<String>,
    ) -> (VerifiedRootGraph, BTreeMap<String, RegistryPackageIndex>) {
        let manifest = LibraryManifest {
            name: "demo".into(),
            version: "1.0.0".into(),
            language: Language::Javascript,
            rbe_abi_min: 1,
            rbe_abi_max: 1,
            sdk: SdkSpec {
                family: "javascript".into(),
                package: "@rbe/sdk".into(),
                version: "0.1".into(),
            },
            runtime: RuntimeSpec {
                kind: "bun".into(),
                version: "1.3".into(),
                managed: true,
                entry: "src/index.js".into(),
            },
            exports: Default::default(),
            capabilities: Default::default(),
            build_capabilities: Default::default(),
            dependencies: Default::default(),
            build,
        };
        let locked = LockedProjectPackage {
            version: "1.0.0".into(),
            resolved_from: "registry:demo".into(),
            artifact_url: "https://example.com/demo.rbe".into(),
            artifact_sha256: SHA.into(),
            manifest_sha256: "b".repeat(64),
            source_sha256: None,
            dependencies: Default::default(),
            runtime: Some(LockedToolchain {
                kind: "bun".into(),
                version: "1.3".into(),
            }),
            sdk: Some(LockedToolchain {
                kind: "javascript".into(),
                version: "0.1".into(),
            }),
        };
        let stage = ArtifactStage {
            verified: VerifiedDownload {
                sha256: SHA.into(),
                size_bytes: 10,
            },
            promotion: PromotionPlan {
                verified_partial: PathBuf::from("/tmp/demo.part"),
                final_dir: PathBuf::from("/tmp/demo"),
                final_artifact: PathBuf::from("/tmp/demo/artifact.rbe"),
                create_final_dir: true,
                replace_existing: false,
                fsync_before_publish: true,
                fsync_parent_after_publish: true,
            },
            resumed_from_bytes: 0,
        };
        let verified = VerifiedRegistryPackage {
            stage,
            manifest,
            manifest_sha256: "b".repeat(64),
            locked: locked.clone(),
        };
        let mut lock = ProjectPackageLock::default();
        lock.packages.insert("demo".into(), locked);
        let graph = VerifiedRootGraph {
            root: "demo".into(),
            install_order: vec!["demo".into()],
            lock,
            packages: BTreeMap::from([("demo".into(), verified)]),
        };
        let release = RegistryPackageRelease {
            version: "1.0.0".into(),
            rbe_abi_min: 1,
            rbe_abi_max: 1,
            yanked: false,
            dependencies: Default::default(),
            artifact: RegistryArtifact {
                source: "https://example.com/demo.rbe".into(),
                sha256: SHA.into(),
                size_bytes: 10,
                source_sha256: None,
                publisher: None,
                signature,
                reproducible_build: false,
                shipped_binary_sha256: None,
            },
        };
        let indexes = BTreeMap::from([(
            "demo".into(),
            RegistryPackageIndex {
                format: REGISTRY_PACKAGE_INDEX_FORMAT,
                package: "demo".into(),
                releases: vec![release],
            },
        )]);
        (graph, indexes)
    }

    #[test]
    fn unsigned_prebuilt_package_gets_activation_ready_proof() {
        let (graph, indexes) = fixture(BuildSpec::default(), None);
        let proofs = prepare_prebuilt_activation_proofs(&graph, &indexes).unwrap();
        let proof = &proofs["demo"];
        assert_eq!(proof.observed_artifact_sha256, SHA);
        assert!(proof.build_id.is_none());
        assert!(proof.gate.can_activate());
    }

    #[test]
    fn host_build_steps_fail_closed_before_activation() {
        let build = BuildSpec {
            other: vec![BuildStep {
                program: "bun".into(),
                args: vec!["build".into()],
            }],
            ..Default::default()
        };
        let (graph, indexes) = fixture(build, None);
        assert!(matches!(
            prepare_prebuilt_activation_proofs(&graph, &indexes),
            Err(PrebuiltPreparationError::ManagedBuildRequired { package, steps: 1 }) if package == "demo"
        ));
    }

    #[test]
    fn declared_signature_is_never_assumed_verified() {
        let (graph, indexes) = fixture(BuildSpec::default(), Some("signature".into()));
        let proofs = prepare_prebuilt_activation_proofs(&graph, &indexes).unwrap();
        assert!(!proofs["demo"].gate.can_activate());
    }
}
