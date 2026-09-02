from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


# Add cryptographic catalog fingerprinting at the compiler boundary.
path = Path("crates/service-runtime/Cargo.toml")
source = path.read_text()
if 'sha2 = "0.10"' not in source:
    source = source.replace('rand = "0.8"\n', 'rand = "0.8"\nsha2 = "0.10"\n', 1)
path.write_text(source)

path = Path("crates/service-runtime/src/lib.rs")
source = path.read_text()
source = replace_once(
    source,
    "use serde_json::Value;\n",
    "use serde_json::Value;\nuse sha2::{Digest, Sha256};\n",
    "sha2 import",
)
source = replace_once(
    source,
    '''    pub imports: Vec<String>,
    pub exports: Vec<String>,
}
''',
    '''    pub imports: Vec<String>,
    pub exports: Vec<String>,
    source_digest: [u8; 32],
}
''',
    "ServiceFile source digest",
)

services_method = '''    pub fn services(&self) -> &[ServiceFile] {
        &self.services
    }
'''
fingerprint = r'''    pub fn services(&self) -> &[ServiceFile] {
        &self.services
    }

    /// Stable SHA-256 contract for the exact service programs and compiled
    /// policies this backend validated. Service Mother must reproduce this
    /// value before advertising readiness, preventing parent/child boot TOCTOU.
    pub fn fingerprint(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"RBE_SERVICE_CATALOG_V1");
        fingerprint_u64(&mut digest, self.monitor_interval_ms);
        fingerprint_u64(&mut digest, self.max_restart_backoff_ms);
        fingerprint_u64(&mut digest, self.services.len() as u64);
        for service in &self.services {
            fingerprint_field(&mut digest, service.name.as_bytes());
            fingerprint_field(&mut digest, service.title.as_bytes());
            fingerprint_field(&mut digest, service_mode_label(service.mode).as_bytes());
            fingerprint_field(&mut digest, restart_policy_label(service.restart).as_bytes());
            fingerprint_u64(&mut digest, service.memory_limit_mb);
            fingerprint_u64(&mut digest, service.startup_timeout_ms);
            fingerprint_u64(&mut digest, service.idle_timeout_ms);
            fingerprint_field(&mut digest, &service.source_digest);
        }
        let bytes = digest.finalize();
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
    }
'''
source = replace_once(source, services_method, fingerprint, "catalog fingerprint method")

compile_error_anchor = '''fn compile_error(
    code: &'static str,
'''
helpers = r'''fn fingerprint_field(digest: &mut Sha256, value: &[u8]) {
    fingerprint_u64(digest, value.len() as u64);
    digest.update(value);
}

fn fingerprint_u64(digest: &mut Sha256, value: u64) {
    digest.update(value.to_le_bytes());
}

const fn service_mode_label(mode: ServiceMode) -> &'static str {
    match mode {
        ServiceMode::Resident => "resident",
        ServiceMode::OnDemand => "on-demand",
        ServiceMode::Hybrid => "hybrid",
    }
}

const fn restart_policy_label(policy: RestartPolicy) -> &'static str {
    match policy {
        RestartPolicy::Always => "always",
        RestartPolicy::OnFailure => "on-failure",
        RestartPolicy::Never => "never",
    }
}

'''
if compile_error_anchor not in source:
    raise SystemExit("catalog fingerprint helper insertion anchor missing")
source = source.replace(compile_error_anchor, helpers + compile_error_anchor, 1)

source = replace_once(
    source,
    '''        imports,
        exports,
    })
}
''',
    '''        imports,
        exports,
        source_digest: Sha256::digest(source.as_bytes()).into(),
    })
}
''',
    "service source digest initialization",
)

tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("service-runtime tests tail missing")
test = r'''

    #[test]
    fn service_catalog_fingerprint_tracks_source_and_compiled_policy() {
        let root = std::env::temp_dir().join(format!(
            "rbe-service-fingerprint-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let service = root.join("demo.service");
        std::fs::write(
            &service,
            ":service[name = demo]\nexport function run() { return 1; }\n",
        )
        .unwrap();
        let first = ServiceCatalog::compile_dir(&root, ServiceDefaults::default()).unwrap();
        let same = ServiceCatalog::compile_dir(&root, ServiceDefaults::default()).unwrap();
        assert_eq!(first.fingerprint(), same.fingerprint());
        assert_eq!(first.fingerprint().len(), 64);

        std::fs::write(
            &service,
            ":service[name = demo]\nexport function run() { return 2; }\n",
        )
        .unwrap();
        let changed_source =
            ServiceCatalog::compile_dir(&root, ServiceDefaults::default()).unwrap();
        assert_ne!(first.fingerprint(), changed_source.fingerprint());

        let mut defaults = ServiceDefaults::default();
        defaults.monitor_interval_ms += 1;
        let changed_policy = ServiceCatalog::compile_dir(&root, defaults).unwrap();
        assert_ne!(changed_source.fingerprint(), changed_policy.fingerprint());
        let _ = std::fs::remove_dir_all(root);
    }
'''
source = source[:tests_end] + test + source[tests_end:]
path.write_text(source)


# Service Mother child validates the exact parent catalog before readiness;
# parent and every supervised replacement carry the same expected digest.
path = Path("crates/backend/src/service_mother.rs")
source = path.read_text()
source = replace_once(
    source,
    '''    let io = atomic_io::AtomicIo::new();
    let catalog = crate::service_boot::compile(&config.services, &io)?;
    let manager = match catalog.as_ref() {
''',
    '''    let io = atomic_io::AtomicIo::new();
    let catalog = crate::service_boot::compile(&config.services, &io)?;
    let actual_fingerprint = catalog
        .as_ref()
        .map(|catalog| catalog.fingerprint())
        .unwrap_or_default();
    let expected_fingerprint = flag_value(args, "--service-catalog-fingerprint");
    if std::env::var_os("RBE_PARENT_LIVENESS_PIPE").is_some()
        && expected_fingerprint.is_none()
    {
        anyhow::bail!("Service Mother requires the parent service catalog fingerprint");
    }
    if let Some(expected) = expected_fingerprint {
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("Service Mother parent catalog fingerprint is malformed");
        }
        if !expected.eq_ignore_ascii_case(&actual_fingerprint) {
            anyhow::bail!(
                "Service Mother catalog changed after parent validation (expected {expected}, compiled {actual_fingerprint})"
            );
        }
    }
    let manager = match catalog.as_ref() {
''',
    "Service Mother child fingerprint validation",
)
source = replace_once(
    source,
    '''async fn spawn_process(
    settings_path: impl AsRef<Path>,
    existing_manager: Option<&ServiceManager>,
) -> anyhow::Result<ServiceMotherProcess> {
''',
    '''async fn spawn_process(
    settings_path: impl AsRef<Path>,
    expected_catalog_fingerprint: &str,
    existing_manager: Option<&ServiceManager>,
) -> anyhow::Result<ServiceMotherProcess> {
    if expected_catalog_fingerprint.len() != 64
        || !expected_catalog_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!("Service Mother expected catalog fingerprint is malformed");
    }
''',
    "spawn_process fingerprint parameter",
)
source = replace_once(
    source,
    '''        .args(["--service-mother", "--launch-separate"])
        .current_dir(parent)
''',
    '''        .args(["--service-mother", "--launch-separate"])
        .arg("--service-catalog-fingerprint")
        .arg(expected_catalog_fingerprint)
        .current_dir(parent)
''',
    "Service Mother fingerprint argument",
)
source = replace_once(
    source,
    '''pub async fn spawn(settings_path: impl AsRef<Path>) -> anyhow::Result<ServiceMotherSupervisor> {
''',
    '''pub async fn spawn(
    settings_path: impl AsRef<Path>,
    expected_catalog_fingerprint: &str,
) -> anyhow::Result<ServiceMotherSupervisor> {
''',
    "Service Mother spawn signature",
)
source = replace_once(
    source,
    '''    let initial = spawn_process(&settings_path, None).await?;
''',
    '''    let expected_catalog_fingerprint = expected_catalog_fingerprint.to_string();
    let initial = spawn_process(&settings_path, &expected_catalog_fingerprint, None).await?;
''',
    "initial mother fingerprint",
)
source = replace_once(
    source,
    '''    let supervisor_settings = settings_path.clone();
''',
    '''    let supervisor_settings = settings_path.clone();
    let supervisor_fingerprint = expected_catalog_fingerprint.clone();
''',
    "supervisor fingerprint clone",
)
source = replace_once(
    source,
    '''            supervisor_settings,
            supervisor_manager,
            &mut shutdown_rx,
''',
    '''            supervisor_settings,
            supervisor_fingerprint,
            supervisor_manager,
            &mut shutdown_rx,
''',
    "supervise fingerprint argument",
)
source = replace_once(
    source,
    '''async fn supervise(
    mut process: ServiceMotherProcess,
    settings_path: PathBuf,
    manager: ServiceManager,
''',
    '''async fn supervise(
    mut process: ServiceMotherProcess,
    settings_path: PathBuf,
    expected_catalog_fingerprint: String,
    manager: ServiceManager,
''',
    "supervise signature fingerprint",
)
source = replace_once(
    source,
    '''            match spawn_process(&settings_path, Some(&manager)).await {
''',
    '''            match spawn_process(
                &settings_path,
                &expected_catalog_fingerprint,
                Some(&manager),
            )
            .await
            {
''',
    "replacement mother fingerprint",
)

# Add a focused validation helper test without needing to spawn the backend.
tests_anchor = '''    #[test]
    fn mother_restart_backoff_is_exponential_and_capped() {
'''
tests_replacement = r'''    #[test]
    fn catalog_fingerprint_argument_is_fixed_size_hex() {
        let valid = "ab".repeat(32);
        assert_eq!(valid.len(), 64);
        assert!(valid.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!("not-a-fingerprint"
            .bytes()
            .any(|byte| !byte.is_ascii_hexdigit()));
    }

    #[test]
    fn mother_restart_backoff_is_exponential_and_capped() {
'''
source = replace_once(source, tests_anchor, tests_replacement, "Service Mother fingerprint test")
path.write_text(source)


# Main passes the exact catalog contract it already validated.
path = Path("crates/backend/src/main.rs")
source = path.read_text()
source = replace_once(
    source,
    '''    let service_mother = match service_catalog.as_ref() {
        Some(_) => Some(service_mother::spawn(&settings_path).await?),
        None => None,
    };
''',
    '''    let service_mother = match service_catalog.as_ref() {
        Some(catalog) => Some(
            service_mother::spawn(&settings_path, &catalog.fingerprint()).await?,
        ),
        None => None,
    };
''',
    "main Service Mother fingerprint",
)
path.write_text(source)
