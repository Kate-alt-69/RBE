use anyhow::{bail, Context};
use core_lib::{
    ExpectedWorkerIdentity, LibraryCapabilityGrant, LibraryPackageIdentity, LibraryRuntimeIdentity,
    LibrarySdkIdentity, LibrarySessionBinding, LIBRARY_ABI_VERSION,
};
use rand::RngCore;
use rbe_install_runtime::VerifiedRpxRootSnapshot;

const SESSION_ID_BYTES: usize = 32;

/// Convert one fail-closed verified RPX root snapshot into the exact identity
/// Library Host expects the package worker to announce.
pub fn expected_worker(
    snapshot: &VerifiedRpxRootSnapshot,
) -> anyhow::Result<ExpectedWorkerIdentity> {
    let worker = &snapshot.worker;
    if worker.package != snapshot.package
        || worker.version != snapshot.version
        || !worker
            .artifact_sha256
            .eq_ignore_ascii_case(&snapshot.artifact_sha256)
    {
        bail!(
            "verified package worker identity drifted from RPX root snapshot for {:?}",
            snapshot.package
        );
    }
    if worker.rbe_abi_min == 0
        || worker.rbe_abi_min > worker.rbe_abi_max
        || LIBRARY_ABI_VERSION < worker.rbe_abi_min
        || LIBRARY_ABI_VERSION > worker.rbe_abi_max
    {
        bail!(
            "verified package {:?} supports RBE ABI {}..={} but Backend selected ABI {}",
            snapshot.package,
            worker.rbe_abi_min,
            worker.rbe_abi_max,
            LIBRARY_ABI_VERSION
        );
    }

    Ok(ExpectedWorkerIdentity {
        package: LibraryPackageIdentity {
            name: snapshot.package.clone(),
            version: snapshot.version.clone(),
            artifact_sha256: snapshot.artifact_sha256.to_ascii_lowercase(),
        },
        sdk: LibrarySdkIdentity {
            language: worker.sdk_language.clone(),
            name: worker.sdk_name.clone(),
            version: worker.sdk_version.clone(),
        },
        runtime: LibraryRuntimeIdentity {
            kind: worker.runtime_kind.clone(),
            version: worker.runtime_version.clone(),
        },
        abi: LIBRARY_ABI_VERSION,
    })
}

/// Create one Library Host session from verified worker identity plus an already
/// admitted capability set. Requested manifest capabilities are deliberately not
/// accepted here: callers must provide trusted [`LibraryCapabilityGrant`]s.
pub fn bind_session(
    snapshot: &VerifiedRpxRootSnapshot,
    grants: impl IntoIterator<Item = LibraryCapabilityGrant>,
) -> anyhow::Result<LibrarySessionBinding> {
    let expected = expected_worker(snapshot)?;
    let mut nonce = [0u8; SESSION_ID_BYTES];
    let mut rng = rand::rngs::OsRng;
    rng.fill_bytes(&mut nonce);
    let capability_identity = format!("session:{}", hex::encode(nonce));

    LibrarySessionBinding::new(expected, grants, capability_identity)
        .context("bind verified package worker to Library Host session")
}

#[cfg(test)]
mod tests {
    use core_lib::{LibraryWorkerHello, LIBRARY_PROTOCOL_VERSION};
    use rbe_install_runtime::VerifiedPackageWorkerIdentity;

    use super::*;

    fn snapshot() -> VerifiedRpxRootSnapshot {
        VerifiedRpxRootSnapshot {
            package: "advancenet".into(),
            version: "2.0.0".into(),
            artifact_sha256: "a".repeat(64),
            index_json: "{}".into(),
            worker: VerifiedPackageWorkerIdentity {
                package: "advancenet".into(),
                version: "2.0.0".into(),
                artifact_sha256: "a".repeat(64),
                rbe_abi_min: LIBRARY_ABI_VERSION,
                rbe_abi_max: LIBRARY_ABI_VERSION,
                sdk_language: "bun".into(),
                sdk_name: "@rbe/sdk".into(),
                sdk_version: "0.1.9".into(),
                runtime_kind: "bun".into(),
                runtime_version: "1.3.7".into(),
                runtime_entry: "src/index.js".into(),
                runtime_managed: true,
            },
        }
    }

    fn hello(snapshot: &VerifiedRpxRootSnapshot) -> LibraryWorkerHello {
        let expected = expected_worker(snapshot).unwrap();
        LibraryWorkerHello {
            protocol: LIBRARY_PROTOCOL_VERSION,
            package: expected.package,
            sdk: expected.sdk,
            runtime: expected.runtime,
            abi_min: LIBRARY_ABI_VERSION,
            abi_max: LIBRARY_ABI_VERSION,
        }
    }

    #[test]
    fn verified_snapshot_becomes_exact_expected_worker_identity() {
        let snapshot = snapshot();
        let expected = expected_worker(&snapshot).unwrap();
        assert_eq!(expected.package.name, "advancenet");
        assert_eq!(expected.package.version, "2.0.0");
        assert_eq!(expected.package.artifact_sha256, "a".repeat(64));
        assert_eq!(expected.sdk.language, "bun");
        assert_eq!(expected.sdk.name, "@rbe/sdk");
        assert_eq!(expected.sdk.version, "0.1.9");
        assert_eq!(expected.runtime.kind, "bun");
        assert_eq!(expected.runtime.version, "1.3.7");
        assert_eq!(expected.abi, LIBRARY_ABI_VERSION);
    }

    #[test]
    fn session_uses_opaque_identity_and_only_supplied_grants() {
        let snapshot = snapshot();
        let mut session = bind_session(
            &snapshot,
            [LibraryCapabilityGrant::new(
                "net:http",
                "net:http",
                ["request".to_string()],
                1024,
                4096,
            )
            .unwrap()],
        )
        .unwrap();
        let info = session.accept_hello(&hello(&snapshot)).unwrap();
        assert!(info.capability_identity.starts_with("session:"));
        assert_eq!(info.capability_identity.len(), "session:".len() + 64);
        assert_eq!(
            info.granted_capabilities,
            std::collections::BTreeSet::from(["net:http".to_string()])
        );
    }

    #[test]
    fn unsupported_worker_abi_is_rejected_before_session_creation() {
        let mut snapshot = snapshot();
        snapshot.worker.rbe_abi_min = LIBRARY_ABI_VERSION + 1;
        snapshot.worker.rbe_abi_max = LIBRARY_ABI_VERSION + 1;
        let error = expected_worker(&snapshot).unwrap_err();
        assert!(error.to_string().contains("supports RBE ABI"));
    }

    #[test]
    fn snapshot_identity_drift_is_rejected() {
        let mut snapshot = snapshot();
        snapshot.worker.artifact_sha256 = "b".repeat(64);
        let error = expected_worker(&snapshot).unwrap_err();
        assert!(error.to_string().contains("identity drifted"));
    }
}
