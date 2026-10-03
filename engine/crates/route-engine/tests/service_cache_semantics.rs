use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use atomic_io::AtomicIo;
use route_engine::service_bin::{
    assemble_service_bin, write_service_bin_atomic, AssemblyRecordKind, RequiredOid,
    ServiceAssemblyPlan, VerifiedAssemblyOidRecord, SERVICE_PLAN_FORMAT,
};
use route_engine::service_cache_invalidation::{
    invalidate_service_cache_for_dependency_hashes, ServiceCacheProtection,
};
use route_engine::service_oid_adapter::write_service_plan_atomic;

fn sha(ch: char) -> String {
    std::iter::repeat(ch).take(64).collect()
}

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rbe-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn plan(service: &str, dependency_key: &str, dependency_hash: String) -> ServiceAssemblyPlan {
    ServiceAssemblyPlan {
        format: SERVICE_PLAN_FORMAT,
        service_identity: service.into(),
        service_source_sha256: sha('a'),
        index_identity_sha256: sha('b'),
        target_fingerprint: "test-target".into(),
        entry_oids: vec![30_458],
        required_oids: vec![RequiredOid {
            oid: 30_458,
            record_hash: sha('c'),
            kind: AssemblyRecordKind::ServiceExport,
        }],
        placement_order: vec![30_458],
        call_graph: BTreeMap::new(),
        service_data: Vec::new(),
        data_alignment: 8,
        dependency_hashes: BTreeMap::from([(
            dependency_key.to_string(),
            dependency_hash,
        )]),
        compile_options: BTreeMap::new(),
    }
}

fn write_artifacts(root: &std::path::Path, plan: &ServiceAssemblyPlan) -> (String, String, PathBuf, PathBuf) {
    let (plan_hash, plan_path) = write_service_plan_atomic(root, plan).unwrap();
    let required = &plan.required_oids[0];
    let record = VerifiedAssemblyOidRecord {
        oid: required.oid,
        record_hash: required.record_hash.clone(),
        kind: required.kind,
        target_fingerprint: plan.target_fingerprint.clone(),
        alignment: 1,
        entry_offset: 0,
        frame_terminator: Some(0),
        diagnostics: Vec::new(),
        machine_code: vec![0xC3],
        relocations: Vec::new(),
    };
    let bin = assemble_service_bin(plan, &BTreeMap::from([(record.oid, record)])).unwrap();
    let assembly_hash = bin.assembly_hash.clone();
    let bin_path = write_service_bin_atomic(
        &AtomicIo::new(),
        &root.join(".cache/compiler"),
        &bin,
    )
    .unwrap();
    (plan_hash, assembly_hash, plan_path, bin_path)
}

#[test]
fn semantic_invalidation_removes_only_stale_plan_and_bin() {
    let root = temp_root("semantic-invalidation");
    let stale_key = "rel-semantic/module_used_run";
    let live_key = "rel-semantic/module_unrelated_run";

    let stale = plan("worker-stale", stale_key, sha('d'));
    let live = plan("worker-live", live_key, sha('f'));
    let (stale_plan_hash, stale_bin_hash, stale_plan_path, stale_bin_path) =
        write_artifacts(&root, &stale);
    let (live_plan_hash, live_bin_hash, live_plan_path, live_bin_path) =
        write_artifacts(&root, &live);

    let report = invalidate_service_cache_for_dependency_hashes(
        &root,
        "rel-semantic/",
        &BTreeMap::from([
            (stale_key.to_string(), sha('e')),
            (live_key.to_string(), sha('f')),
        ]),
        &ServiceCacheProtection::default(),
    )
    .unwrap();

    assert_eq!(
        report.affected_dependency_keys,
        std::collections::BTreeSet::from([stale_key.to_string()])
    );
    assert!(report.removed_plan_hashes.contains(&stale_plan_hash));
    assert!(report.removed_assembly_hashes.contains(&stale_bin_hash));
    assert!(!stale_plan_path.exists());
    assert!(!stale_bin_path.exists());

    assert!(!report.removed_plan_hashes.contains(&live_plan_hash));
    assert!(!report.removed_assembly_hashes.contains(&live_bin_hash));
    assert!(live_plan_path.exists());
    assert!(live_bin_path.exists());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn semantic_invalidation_retains_live_pinned_stale_artifacts() {
    let root = temp_root("semantic-invalidation-pinned");
    let key = "rel-semantic/service_worker_run";
    let stale = plan("worker-pinned", key, sha('d'));
    let (plan_hash, assembly_hash, plan_path, bin_path) = write_artifacts(&root, &stale);

    let protection = ServiceCacheProtection {
        plan_hashes: std::collections::BTreeSet::from([plan_hash.clone()]),
        assembly_hashes: std::collections::BTreeSet::from([assembly_hash.clone()]),
    };
    let report = invalidate_service_cache_for_dependency_hashes(
        &root,
        "rel-semantic/",
        &BTreeMap::from([(key.to_string(), sha('e'))]),
        &protection,
    )
    .unwrap();

    assert!(report.retained_plan_hashes.contains(&plan_hash));
    assert!(plan_path.exists());
    // Because the protected plan was not deleted, its bin is not selected for
    // deletion either. The artifact lifetime registry continues to protect both.
    assert!(!report.removed_assembly_hashes.contains(&assembly_hash));
    assert!(bin_path.exists());

    let _ = fs::remove_dir_all(root);
}
