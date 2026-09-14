from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one Mother diagnostics anchor, found {count}")
    p.write_text(text.replace(old, new, 1))


path = "engine/crates/backend/src/service_mother.rs"
replace_once(
    path,
    'const MOTHER_STDOUT_LINE_MAX_BYTES: usize = 64 * 1024;\n',
    'const MOTHER_STDOUT_LINE_MAX_BYTES: usize = 64 * 1024;\nconst SERVICE_COMPAT_TIMEOUT: Duration = Duration::from_secs(5);\n',
)
replace_once(
    path,
    '''    if !expected_runtime_digest.eq_ignore_ascii_case(&actual_runtime_digest) {
        anyhow::bail!(
            "Service Mother executable digest mismatch; refusing unverified service runtime"
        );
    }
''',
    '''    if !expected_runtime_digest.eq_ignore_ascii_case(&actual_runtime_digest) {
        return Err(incompatible_service_runtime_error(
            &current_exe,
            "Service runtime bytes changed after parent verification",
        ));
    }
''',
)
replace_once(
    path,
    '''        if !expected.eq_ignore_ascii_case(&actual_fingerprint) {
            anyhow::bail!(
                "Service Mother catalog changed after parent validation (expected {expected}, compiled {actual_fingerprint})"
            );
        }
''',
    '''        if !expected.eq_ignore_ascii_case(&actual_fingerprint) {
            let service_root = match application_root.as_deref() {
                Some(root) => crate::service_boot::resolve_runtime_path_from(
                    root,
                    &config.services.directory,
                ),
                None => crate::service_boot::resolve_runtime_path(&config.services.directory),
            };
            anyhow::bail!(
                "SVC5002 Service Mother compiled a different Service catalog than the backend validated.\\n\\n  service_root:\\n    {}\\n\\n  service_binary:\\n    {}\\n\\n  expected_fingerprint:\\n    {}\\n\\n  compiled_fingerprint:\\n    {}\\n\\n  action:\\n    Ensure .service files and Service settings are not changing during startup and restart RBE.\\n\\n    If the mismatch persists, rebuild RBE and replace the generated Service binary at:\\n    {}",
                service_root.display(),
                current_exe.display(),
                expected,
                actual_fingerprint,
                current_exe.display(),
            );
        }
''',
)

old_ensure = '''fn ensure_canonical_service_executable(
    backend: &Path,
    parent: &Path,
    expected_service_sha256: &str,
) -> anyhow::Result<PathBuf> {
    if expected_service_sha256.len() != 64
        || !expected_service_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!("backend was built without a valid standalone Service integrity binding");
    }

    let service = parent.join("dep").join(service_executable_name());
    if !service.is_file() {
        anyhow::bail!(
            "standalone Service runtime {} is missing; backend will not synthesize it from itself",
            service.display()
        );
    }

    let actual = file_sha256_hex(&service)?;
    if !actual.eq_ignore_ascii_case(expected_service_sha256) {
        anyhow::bail!(
            "standalone Service runtime {} failed build-time SHA-256 verification",
            service.display()
        );
    }

    // Regression guard: service.exe must be an independently linked runtime,
    // never backend.exe copied/hard-linked under another filename again.
    let backend_hash = file_sha256_hex(backend)?;
    if actual.eq_ignore_ascii_case(&backend_hash) {
        anyhow::bail!("standalone Service runtime unexpectedly matches backend executable bytes");
    }
    Ok(service)
}
'''
new_ensure = '''fn incompatible_service_runtime_error(
    path: &Path,
    reason: impl std::fmt::Display,
) -> anyhow::Error {
    anyhow::anyhow!(
        "SVC5001 Service binary is not compatible with the current backend.\\n\\n  expected_path:\\n    {}\\n\\n  reason:\\n    {}\\n\\n  action:\\n    Rebuild RBE for this target and replace the generated Service binary at:\\n    {}",
        path.display(),
        reason,
        path.display(),
    )
}

fn service_compat_protocol_error(path: &Path, reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "SVC5003 Service binary does not support the required compatibility protocol.\\n\\n  expected_path:\\n    {}\\n\\n  reason:\\n    {}\\n\\n  action:\\n    Rebuild RBE and replace the generated Service binary at:\\n    {}",
        path.display(),
        reason,
        path.display(),
    )
}

fn ensure_canonical_service_executable(
    backend: &Path,
    parent: &Path,
    expected_service_sha256: &str,
) -> anyhow::Result<PathBuf> {
    let service = parent.join("dep").join(service_executable_name());
    if expected_service_sha256.len() != 64
        || !expected_service_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(incompatible_service_runtime_error(
            &service,
            "Backend has no valid standalone Service integrity binding",
        ));
    }
    if !service.is_file() {
        return Err(incompatible_service_runtime_error(
            &service,
            "Standalone Service runtime is missing",
        ));
    }

    let actual = file_sha256_hex(&service).map_err(|error| {
        incompatible_service_runtime_error(
            &service,
            format!("Could not verify Service runtime bytes: {error:#}"),
        )
    })?;
    if !actual.eq_ignore_ascii_case(expected_service_sha256) {
        return Err(incompatible_service_runtime_error(
            &service,
            "Build-time SHA-256 binding does not match this Service binary",
        ));
    }

    // Regression guard: service.exe must be an independently linked runtime,
    // never backend.exe copied/hard-linked under another filename again.
    let backend_hash = file_sha256_hex(backend)?;
    if actual.eq_ignore_ascii_case(&backend_hash) {
        return Err(incompatible_service_runtime_error(
            &service,
            "Standalone Service runtime unexpectedly matches backend executable bytes",
        ));
    }
    Ok(service)
}

fn validate_service_compatibility(
    service: &Path,
    probe: &service_runtime::ServiceCompatibilityProbe,
) -> anyhow::Result<()> {
    if probe.protocol != service_runtime::SERVICE_COMPAT_PROTOCOL {
        return Err(service_compat_protocol_error(
            service,
            format!(
                "Service reports protocol {:?}; backend requires {:?}",
                probe.protocol,
                service_runtime::SERVICE_COMPAT_PROTOCOL,
            ),
        ));
    }

    let mut mismatches = Vec::new();
    if probe.service_runtime_abi != service_runtime::SERVICE_RUNTIME_ABI_VERSION {
        mismatches.push(format!(
            "runtime ABI {} != {}",
            probe.service_runtime_abi,
            service_runtime::SERVICE_RUNTIME_ABI_VERSION,
        ));
    }
    if probe.catalog_fingerprint_abi
        != service_runtime::SERVICE_CATALOG_FINGERPRINT_ABI_VERSION
    {
        mismatches.push(format!(
            "catalog fingerprint ABI {} != {}",
            probe.catalog_fingerprint_abi,
            service_runtime::SERVICE_CATALOG_FINGERPRINT_ABI_VERSION,
        ));
    }
    if probe.package_version != env!("CARGO_PKG_VERSION") {
        mismatches.push(format!(
            "package version {:?} != {:?}",
            probe.package_version,
            env!("CARGO_PKG_VERSION"),
        ));
    }
    if probe.build_id != crate::service_integrity::SERVICE_BUILD_ID {
        mismatches.push(format!(
            "build ID {:?} != {:?}",
            probe.build_id,
            crate::service_integrity::SERVICE_BUILD_ID,
        ));
    }
    if probe.target != crate::service_integrity::SERVICE_TARGET {
        mismatches.push(format!(
            "target {:?} != {:?}",
            probe.target,
            crate::service_integrity::SERVICE_TARGET,
        ));
    }

    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(incompatible_service_runtime_error(
            service,
            format!("Compatibility check failed: {}", mismatches.join("; ")),
        ))
    }
}

async fn probe_service_compatibility(service: &Path) -> anyhow::Result<()> {
    let mut command = Command::new(service);
    command.env_clear();
    for name in [
        "SYSTEMROOT", "WINDIR", "TEMP", "TMP", "TMPDIR", "LANG", "LC_ALL",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .arg("--service-compat-probe")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(|error| {
        service_compat_protocol_error(
            service,
            format!("Could not launch compatibility probe: {error}"),
        )
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        service_compat_protocol_error(service, "Compatibility probe stdout is unavailable")
    })?;
    let mut reader = BufReader::new(stdout);

    let result = tokio::time::timeout(SERVICE_COMPAT_TIMEOUT, async {
        let line = read_bounded_buffered_line(
            &mut reader,
            service_runtime::SERVICE_COMPAT_PROBE_MAX_BYTES,
            "Service compatibility probe",
        )
        .await
        .map_err(|error| service_compat_protocol_error(service, error))?
        .ok_or_else(|| {
            service_compat_protocol_error(service, "Service exited without a compatibility response")
        })?;
        let status = child.wait().await.map_err(|error| {
            service_compat_protocol_error(
                service,
                format!("Could not wait for compatibility probe: {error}"),
            )
        })?;
        if !status.success() {
            return Err(service_compat_protocol_error(
                service,
                format!("Compatibility probe exited with {status}"),
            ));
        }
        let probe: service_runtime::ServiceCompatibilityProbe =
            serde_json::from_str(line.trim()).map_err(|error| {
                service_compat_protocol_error(
                    service,
                    format!("Compatibility response is not valid JSON: {error}"),
                )
            })?;
        validate_service_compatibility(service, &probe)
    })
    .await;

    match result {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(service_compat_protocol_error(
                service,
                format!(
                    "Compatibility probe did not finish within {} ms",
                    SERVICE_COMPAT_TIMEOUT.as_millis()
                ),
            ))
        }
    }
}
'''
replace_once(path, old_ensure, new_ensure)
replace_once(
    path,
    '''    let service_exe = ensure_canonical_service_executable(&exe, parent, expected_service_sha256)?;

    let settings_path = std::fs::canonicalize(settings_path.as_ref()).with_context(|| {
''',
    '''    let service_exe = ensure_canonical_service_executable(&exe, parent, expected_service_sha256)?;
    probe_service_compatibility(&service_exe).await?;

    let settings_path = std::fs::canonicalize(settings_path.as_ref()).with_context(|| {
''',
)
replace_once(
    path,
    '''    let initial = spawn_process(
        &settings_path,
        &expected_catalog_fingerprint,
        &expected_service_sha256,
        runtime_env.as_ref(),
        er_control_key.as_ref(),
        None,
    )
    .await?;
''',
    '''    let initial = match spawn_process(
        &settings_path,
        &expected_catalog_fingerprint,
        &expected_service_sha256,
        runtime_env.as_ref(),
        er_control_key.as_ref(),
        None,
    )
    .await
    {
        Ok(initial) => initial,
        Err(error) => {
            let details = format!("{error:#}");
            if details.contains("SVC5001") || details.contains("SVC5003") {
                logging::Logger::new("SERVICE").child("MOTHER").fatal(details);
            }
            return Err(error);
        }
    };
''',
)
replace_once(
    path,
    '''mod tests {
    use super::*;

''',
    '''mod tests {
    use super::*;

    fn current_compat_probe() -> service_runtime::ServiceCompatibilityProbe {
        service_runtime::ServiceCompatibilityProbe {
            protocol: service_runtime::SERVICE_COMPAT_PROTOCOL.to_string(),
            service_runtime_abi: service_runtime::SERVICE_RUNTIME_ABI_VERSION,
            catalog_fingerprint_abi:
                service_runtime::SERVICE_CATALOG_FINGERPRINT_ABI_VERSION,
            package_version: env!("CARGO_PKG_VERSION").to_string(),
            build_id: crate::service_integrity::SERVICE_BUILD_ID.to_string(),
            target: crate::service_integrity::SERVICE_TARGET.to_string(),
        }
    }

    #[test]
    fn compatibility_probe_distinguishes_protocol_and_runtime_mismatches() {
        let service = Path::new("dep/service.exe");
        let probe = current_compat_probe();
        validate_service_compatibility(service, &probe)
            .expect("current Service compatibility response must validate");

        let mut protocol = probe.clone();
        protocol.protocol = "RBE-SERVICE-COMPAT/999".into();
        let error = validate_service_compatibility(service, &protocol)
            .expect_err("protocol mismatch must fail closed");
        assert!(error.to_string().contains("SVC5003"));

        let mut runtime = probe;
        runtime.service_runtime_abi += 1;
        let error = validate_service_compatibility(service, &runtime)
            .expect_err("runtime ABI mismatch must fail closed");
        assert!(error.to_string().contains("SVC5001"));
        assert!(error.to_string().contains("runtime ABI"));
    }

''',
)
