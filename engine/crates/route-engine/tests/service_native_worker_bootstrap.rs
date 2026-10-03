use std::collections::BTreeMap;
use std::path::PathBuf;

use route_engine::service_native_build::{
    NativeServiceWorkerBootstrap, NativeServiceWorkerBootstrapEntry,
    NATIVE_SERVICE_WORKER_BOOTSTRAP_PROTOCOL,
};

#[test]
fn native_worker_bootstrap_json_round_trip_is_stable() {
    let large_offset = u64::from(u32::MAX) + 17;
    let bootstrap = NativeServiceWorkerBootstrap {
        protocol: NATIVE_SERVICE_WORKER_BOOTSTRAP_PROTOCOL.to_string(),
        runtime_image_id: "image-b".into(),
        source_id: "service:worker".into(),
        oid_index_generation: 42,
        plan_hash: "a".repeat(64),
        assembly_hash: "b".repeat(64),
        target_fingerprint: "linux-x86_64-sysv".into(),
        bin_path: PathBuf::from("/tmp/rbe-native/service.bin"),
        exports: BTreeMap::from([(
            "run".into(),
            NativeServiceWorkerBootstrapEntry {
                oid: 30_458,
                entry_offset: large_offset,
            },
        )]),
        lifecycle: BTreeMap::from([(
            "start".into(),
            NativeServiceWorkerBootstrapEntry {
                oid: 30_459,
                entry_offset: large_offset + 8,
            },
        )]),
    };

    let value = serde_json::to_value(&bootstrap).expect("serialize worker bootstrap");
    assert_eq!(value["protocol"], NATIVE_SERVICE_WORKER_BOOTSTRAP_PROTOCOL);
    assert_eq!(value["runtimeImageId"], "image-b");
    assert_eq!(value["sourceId"], "service:worker");
    assert_eq!(value["oidIndexGeneration"], 42);
    assert_eq!(value["exports"]["run"]["oid"], 30_458);
    assert_eq!(value["exports"]["run"]["entryOffset"], large_offset);
    assert_eq!(value["lifecycle"]["start"]["oid"], 30_459);
    assert_eq!(
        value["lifecycle"]["start"]["entryOffset"],
        large_offset + 8
    );
    assert!(value.get("runtime_image_id").is_none());
    assert!(value.get("entry_offset").is_none());

    let decoded: NativeServiceWorkerBootstrap =
        serde_json::from_value(value).expect("deserialize worker bootstrap");
    assert_eq!(decoded, bootstrap);
    assert_eq!(decoded.export("run").unwrap().entry_offset, large_offset);
    assert_eq!(
        decoded.lifecycle("start").unwrap().entry_offset,
        large_offset + 8
    );
}
