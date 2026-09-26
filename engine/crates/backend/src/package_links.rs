use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context};
use rbe_install_runtime::VerifiedRpxRootIndex;
use route_engine::relc::{
    PackageExportLink, PackageLinkContext, PackageRootLink, PACKAGE_LINK_FORMAT,
};
use serde::Deserialize;

const RPX_PACKAGE_INDEX_FORMAT: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpxPackageIndex {
    format: u32,
    package: RpxPackageIdentity,
    exports: Vec<RpxPackageExport>,
    #[serde(default)]
    private_dependencies: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpxPackageIdentity {
    name: String,
    version: String,
    language: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpxPackageExport {
    name: String,
    source: String,
    language: String,
}

/// Build RELC's package namespace from SHA-verified explicit package roots.
///
/// `read_verified_rpx_root_indexes` already walks only `package.lock.rbe.yaml`
/// `packages`, never the root-scoped `private` graphs. This second boundary
/// validates the RPX index identity against that trusted lock record and maps
/// only public `exports` into RELC. `private_dependencies` is parsed only to
/// validate the index shape; it never creates a namespace.
pub fn load(project_root: &Path) -> anyhow::Result<PackageLinkContext> {
    let indexes = rbe_install_runtime::read_verified_rpx_root_indexes(project_root)
        .context("load verified RPX package indexes")?;
    from_verified_indexes(indexes)
}

fn from_verified_indexes(indexes: Vec<VerifiedRpxRootIndex>) -> anyhow::Result<PackageLinkContext> {
    let mut roots = BTreeMap::new();

    for verified in indexes {
        let index: RpxPackageIndex =
            serde_json::from_str(&verified.index_json).with_context(|| {
                format!(
                    "parse RPX package index for explicit root {:?}",
                    verified.package
                )
            })?;

        if index.format != RPX_PACKAGE_INDEX_FORMAT {
            bail!(
                "unsupported RPX package index format {} for explicit root {:?}",
                index.format,
                verified.package
            );
        }
        if index.package.name != verified.package {
            bail!(
                "RPX package index identity mismatch for explicit root {:?}: index declares {:?}",
                verified.package,
                index.package.name
            );
        }
        if index.package.version != verified.version {
            bail!(
                "RPX package index version mismatch for explicit root {:?}: lock={}, index={}",
                verified.package,
                verified.version,
                index.package.version
            );
        }
        if !matches!(
            index.package.language.as_str(),
            "rust" | "javascript" | "typescript" | "python" | "global"
        ) {
            bail!(
                "unsupported RPX package language {:?} for explicit root {:?}",
                index.package.language,
                verified.package
            );
        }

        // Parsed deliberately, never linked. This keeps malformed indexes from
        // slipping through while making the visibility rule impossible to
        // accidentally widen through iteration over transitive dependency data.
        let _private_dependency_names = index.private_dependencies.keys().collect::<BTreeSet<_>>();

        let mut exports = BTreeMap::new();
        for export in index.exports {
            if export.language == "global" {
                bail!(
                    "RPX export {:?} from {:?} resolved to global instead of a concrete SDK language",
                    export.name,
                    verified.package
                );
            }
            if !matches!(
                export.language.as_str(),
                "rust" | "javascript" | "typescript" | "python"
            ) {
                bail!(
                    "unsupported RPX export language {:?} for {} from {}",
                    export.language,
                    export.name,
                    verified.package
                );
            }
            let name = export.name;
            if exports
                .insert(
                    name.clone(),
                    PackageExportLink {
                        entry: export.source,
                        language: export.language,
                    },
                )
                .is_some()
            {
                bail!(
                    "duplicate RPX public export {:?} in explicit root {:?}",
                    name,
                    verified.package
                );
            }
        }

        let package_name = verified.package;
        if roots
            .insert(
                package_name.clone(),
                PackageRootLink {
                    version: verified.version,
                    artifact_sha256: verified.artifact_sha256,
                    exports,
                },
            )
            .is_some()
        {
            bail!("duplicate verified RPX root package {package_name:?}");
        }
    }

    let context = PackageLinkContext {
        format: PACKAGE_LINK_FORMAT,
        roots,
    };
    context
        .validate()
        .context("validate RELC package-link context derived from verified roots")?;
    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verified(index_json: &str) -> VerifiedRpxRootIndex {
        VerifiedRpxRootIndex {
            package: "advancenet".into(),
            version: "2.0.0".into(),
            artifact_sha256: "a".repeat(64),
            index_json: index_json.into(),
        }
    }

    fn index(private_dependencies: &str, exports: &str) -> String {
        format!(
            r#"{{
                "format": 1,
                "package": {{
                    "name": "advancenet",
                    "version": "2.0.0",
                    "language": "typescript"
                }},
                "exports": {exports},
                "private_dependencies": {private_dependencies}
            }}"#
        )
    }

    #[test]
    fn verified_root_exports_become_relc_links() {
        let input = index(
            r#"{"secret-parser":"^9"}"#,
            r#"[{"name":"request","source":"components/request/request.ts","language":"typescript"}]"#,
        );
        let links = from_verified_indexes(vec![verified(&input)]).unwrap();
        let request = links.resolve("advancenet", "request").unwrap();
        assert_eq!(request.entry, "components/request/request.ts");
        assert_eq!(request.language, "typescript");
        assert!(links.root("secret-parser").is_none());
    }

    #[test]
    fn private_dependency_metadata_never_creates_rel_roots() {
        let input = index(
            r#"{"secret-parser":"^9","syntax":"^1"}"#,
            r#"[{"name":"request","source":"components/request/request.ts","language":"typescript"}]"#,
        );
        let links = from_verified_indexes(vec![verified(&input)]).unwrap();
        assert_eq!(links.roots.len(), 1);
        assert!(links.roots.contains_key("advancenet"));
        assert!(!links.roots.contains_key("secret-parser"));
        assert!(!links.roots.contains_key("syntax"));
    }

    #[test]
    fn package_identity_must_match_trusted_lock_root() {
        let input = index(
            "{}",
            r#"[{"name":"request","source":"components/request/request.ts","language":"typescript"}]"#,
        )
        .replace("\"name\": \"advancenet\"", "\"name\": \"evil-root\"");
        let error = from_verified_indexes(vec![verified(&input)]).unwrap_err();
        assert!(error.to_string().contains("identity mismatch"));
    }

    #[test]
    fn package_version_must_match_trusted_lock_root() {
        let input = index(
            "{}",
            r#"[{"name":"request","source":"components/request/request.ts","language":"typescript"}]"#,
        )
        .replace("\"version\": \"2.0.0\"", "\"version\": \"3.0.0\"");
        let error = from_verified_indexes(vec![verified(&input)]).unwrap_err();
        assert!(error.to_string().contains("version mismatch"));
    }

    #[test]
    fn duplicate_public_exports_are_rejected() {
        let input = index(
            "{}",
            r#"[
                {"name":"request","source":"components/request/request.ts","language":"typescript"},
                {"name":"request","source":"components/request/other.ts","language":"typescript"}
            ]"#,
        );
        let error = from_verified_indexes(vec![verified(&input)]).unwrap_err();
        assert!(error.to_string().contains("duplicate RPX public export"));
    }
}
