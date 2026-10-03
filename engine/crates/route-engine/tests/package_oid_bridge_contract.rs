use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use route_engine::package_native_link::prepare_package_native_link;
use route_engine::relc::{
    PackageExportLink, PackageLinkContext, PackageRootLink, PACKAGE_LINK_FORMAT,
};
use route_engine::{OidCache, OID_PACKAGE_END, OID_PACKAGE_START};

fn project_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rbe-package-oid-contract-{label}-{}-{nonce}",
        std::process::id()
    ))
}

#[test]
fn verified_package_context_prepares_single_index_native_transaction() {
    let root = project_root("prepare");
    let cache = OidCache::open_or_rebuild(&root).expect("open project OID cache");
    let links = PackageLinkContext {
        format: PACKAGE_LINK_FORMAT,
        roots: BTreeMap::from([(
            "mail".to_string(),
            PackageRootLink {
                version: "1.0.0".to_string(),
                artifact_sha256: "a".repeat(64),
                exports: BTreeMap::from([(
                    "send".to_string(),
                    PackageExportLink {
                        entry: "components/send/send.ts".to_string(),
                        language: "typescript".to_string(),
                    },
                )]),
            },
        )]),
    };

    let prepared = prepare_package_native_link(&cache, &links, &BTreeSet::new())
        .expect("verified package graph should prepare against the live OID bridge");

    assert!(prepared.delta().changed());
    assert_eq!(prepared.materialize_bindings().len(), 1);
    let oid = prepared.materialize_bindings()[&(String::from("mail"), String::from("lib_mail_send"))];
    assert!((OID_PACKAGE_START..=OID_PACKAGE_END).contains(&oid));
    assert_eq!(cache.index().generation, 0, "prepare must not publish the next index");

    let _ = std::fs::remove_dir_all(root);
}
