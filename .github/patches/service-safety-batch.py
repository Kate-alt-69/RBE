from pathlib import Path
import sys


def replace_once(source: str, old: str, new: str, label: str) -> str:
    count = source.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one anchor, found {count}")
    return source.replace(old, new, 1)


def patch_process_tree_liveness() -> None:
    # Shared parent-pipe watcher. It is only activated for children explicitly
    # launched with RBE_PARENT_LIVENESS_PIPE=1, so normal/tests don't consume stdin.
    path = Path("crates/service-runtime/src/lib.rs")
    source = path.read_text()
    source = replace_once(
        source,
        "use std::io::{IsTerminal, Write};",
        "use std::io::{IsTerminal, Read, Write};",
        "service-runtime std::io imports",
    )
    helper_anchor = "pub fn pause_for_interactive_exit() {\n"
    helper = '''pub fn parent_liveness_signal_if_configured(\n) -> anyhow::Result<Option<tokio::sync::oneshot::Receiver<()>>> {\n    if std::env::var_os("RBE_PARENT_LIVENESS_PIPE").is_none() {\n        return Ok(None);\n    }\n\n    let (sender, receiver) = tokio::sync::oneshot::channel();\n    std::thread::Builder::new()\n        .name("rbe-parent-liveness".into())\n        .spawn(move || {\n            let stdin = std::io::stdin();\n            let mut stdin = stdin.lock();\n            let mut buffer = [0u8; 64];\n            loop {\n                match stdin.read(&mut buffer) {\n                    Ok(0) | Err(_) => {\n                        let _ = sender.send(());\n                        return;\n                    }\n                    Ok(_) => {}\n                }\n            }\n        })\n        .map_err(|error| anyhow::anyhow!("spawn parent liveness watcher: {error}"))?;\n    Ok(Some(receiver))\n}\n\n'''
    source = replace_once(source, helper_anchor, helper + helper_anchor, "parent liveness helper insertion")

    # A service host watches its Service Mother through the inherited stdin pipe.
    loop_anchor = '''    loop {\n        let (stream, _) = listener.accept().await?;\n        let (read, mut write) = stream.into_split();'''
    loop_replacement = '''    let mut parent_liveness = parent_liveness_signal_if_configured()?;\n    loop {\n        let accepted = match parent_liveness.as_mut() {\n            Some(parent_liveness) => {\n                tokio::select! {\n                    accepted = listener.accept() => Some(accepted),\n                    _ = parent_liveness => None,\n                }\n            }\n            None => Some(listener.accept().await),\n        };\n        let Some(accepted) = accepted else {\n            tracing::warn!(\n                service = %file.name,\n                "service parent liveness pipe closed; stopping orphaned worker"\n            );\n            if let Err(error) = executor\n                .lifecycle(ServiceLifecycle::Stop, lifecycle_context(&file))\n                .await\n            {\n                tracing::warn!(\n                    service = %file.name,\n                    code = %error.code,\n                    message = %error.message,\n                    "service stop lifecycle failed after parent loss"\n                );\n            }\n            return Ok(());\n        };\n        let (stream, _) = accepted?;\n        let (read, mut write) = stream.into_split();'''
    source = replace_once(source, loop_anchor, loop_replacement, "service-host accept loop")
    path.write_text(source)

    # Service Mother keeps a write handle to every service-host stdin. If the
    # mother dies, the OS closes those handles and workers observe EOF.
    path = Path("crates/service-runtime/src/manager.rs")
    source = path.read_text()
    source = replace_once(
        source,
        "use tokio::process::{Child, Command};",
        "use tokio::process::{Child, ChildStdin, Command};",
        "manager child imports",
    )
    source = replace_once(
        source,
        '''struct ServiceProcess {\n    child: Child,\n    alias: PathBuf,''',
        '''struct ServiceProcess {\n    child: Child,\n    _liveness: ChildStdin,\n    alias: PathBuf,''',
        "ServiceProcess liveness field",
    )
    source = replace_once(
        source,
        '''        .current_dir(parent)\n        .stdout(Stdio::piped())''',
        '''        .current_dir(parent)\n        .env("RBE_PARENT_LIVENESS_PIPE", "1")\n        .stdin(Stdio::piped())\n        .stdout(Stdio::piped())''',
        "service process parent pipe spawn",
    )
    stdout_anchor = '''    let stdout = match child.stdout.take() {\n        Some(stdout) => stdout,'''
    liveness = '''    let liveness = match child.stdin.take() {\n        Some(stdin) => stdin,\n        None => {\n            cleanup_failed_spawn(&alias, &mut child).await;\n            anyhow::bail!("service {:?} parent liveness pipe unavailable", file.name);\n        }\n    };\n'''
    source = replace_once(source, stdout_anchor, liveness + stdout_anchor, "service process liveness take")
    source = replace_once(
        source,
        '''    Ok(ServiceProcess {\n        child,\n        alias,''',
        '''    Ok(ServiceProcess {\n        child,\n        _liveness: liveness,\n        alias,''',
        "ServiceProcess liveness initializer",
    )
    path.write_text(source)

    # Backend keeps the Service Mother stdin write handle alive. A hard backend
    # exit therefore closes the pipe even when Rust destructors never run.
    path = Path("crates/backend/src/service_mother.rs")
    source = path.read_text()
    source = replace_once(
        source,
        "use tokio::process::{Child, Command};",
        "use tokio::process::{Child, ChildStdin, Command};",
        "Service Mother child imports",
    )
    source = replace_once(
        source,
        '''pub struct ServiceMotherProcess {\n    manager: ServiceManager,\n    child: Child,\n    alias: PathBuf,''',
        '''pub struct ServiceMotherProcess {\n    manager: ServiceManager,\n    child: Child,\n    _liveness: ChildStdin,\n    alias: PathBuf,''',
        "ServiceMotherProcess liveness field",
    )
    source = replace_once(
        source,
        '''    run_service_mother(manager, token).await\n}''',
        '''    let mut parent_liveness = service_runtime::parent_liveness_signal_if_configured()?;\n    match parent_liveness.as_mut() {\n        Some(parent_liveness) => {\n            tokio::select! {\n                result = run_service_mother(manager.clone(), token) => result,\n                _ = parent_liveness => {\n                    tracing::warn!(\n                        "Service Mother parent liveness pipe closed; shutting down managed services"\n                    );\n                    manager.shutdown_all().await;\n                    Ok(())\n                }\n            }\n        }\n        None => run_service_mother(manager, token).await,\n    }\n}''',
        "Service Mother child parent watcher",
    )
    source = replace_once(
        source,
        '''        .current_dir(parent)\n        .env("SETTINGS_PATH", &settings_path)\n        .stdout(Stdio::piped())''',
        '''        .current_dir(parent)\n        .env("SETTINGS_PATH", &settings_path)\n        .env("RBE_PARENT_LIVENESS_PIPE", "1")\n        .stdin(Stdio::piped())\n        .stdout(Stdio::piped())''',
        "Service Mother parent pipe spawn",
    )
    stdout_anchor = '''    let stdout = child\n        .stdout\n        .take()\n        .ok_or_else(|| anyhow::anyhow!("Service Mother stdout unavailable"))?;'''
    liveness = '''    let liveness = match child.stdin.take() {\n        Some(stdin) => stdin,\n        None => {\n            cleanup_failed_spawn(&alias, &mut child).await;\n            anyhow::bail!("Service Mother parent liveness pipe unavailable");\n        }\n    };\n'''
    source = replace_once(source, stdout_anchor, liveness + stdout_anchor, "Service Mother liveness take")
    source = replace_once(
        source,
        '''    Ok(ServiceMotherProcess {\n        manager,\n        child,\n        alias,''',
        '''    Ok(ServiceMotherProcess {\n        manager,\n        child,\n        _liveness: liveness,\n        alias,''',
        "ServiceMotherProcess liveness initializer",
    )
    path.write_text(source)


def patch_service_ipc() -> None:
    path = Path("crates/service-runtime/src/lib.rs")
    source = path.read_text()
    source = replace_once(
        source,
        "use std::pin::Pin;\nuse std::sync::{Arc, RwLock};",
        "use std::pin::Pin;\nuse std::sync::{Arc, RwLock};\nuse std::time::Duration;",
        "service IPC Duration import",
    )
    source = replace_once(
        source,
        "use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};",
        "use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};",
        "service IPC tokio io imports",
    )
    marker = "mod manager;\nmod mother;\n"
    constants = '''mod manager;\nmod mother;\n\npub(crate) const SERVICE_IPC_TIMEOUT: Duration = Duration::from_secs(5);\npub(crate) const SERVICE_IPC_REQUEST_MAX_BYTES: usize = 4 * 1024 * 1024;\npub(crate) const SERVICE_IPC_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;\n'''
    source = replace_once(source, marker, constants, "service IPC constants")

    old = '''        let (stream, _) = accepted?;\n        let (read, mut write) = stream.into_split();\n        let mut reader = BufReader::new(read);\n        let mut line = String::new();\n        reader.read_line(&mut line).await?;\n        let request: ServiceRequest = serde_json::from_str(line.trim())?;\n        let (response, shutdown) =\n            dispatch(request, &file, &token, &memory, executor.as_ref()).await;\n        write\n            .write_all(format!("{}\\n", serde_json::to_string(&response)?).as_bytes())\n            .await?;\n        write.shutdown().await?;\n        if shutdown {\n            return Ok(());\n        }'''
    new = '''        let (stream, peer) = accepted?;\n        if !peer.ip().is_loopback() {\n            tracing::warn!(%peer, service = %file.name, "service IPC rejected non-loopback peer");\n            continue;\n        }\n        let (read, mut write) = stream.into_split();\n        let line = match tokio::time::timeout(\n            SERVICE_IPC_TIMEOUT,\n            read_bounded_line(read, SERVICE_IPC_REQUEST_MAX_BYTES, "service request"),\n        )\n        .await\n        {\n            Ok(Ok(line)) => line,\n            Ok(Err(error)) => {\n                tracing::warn!(service = %file.name, error = %error, "invalid service IPC frame");\n                continue;\n            }\n            Err(_) => {\n                tracing::warn!(service = %file.name, "service IPC request timed out");\n                continue;\n            }\n        };\n        let request: ServiceRequest = match serde_json::from_str(line.trim()) {\n            Ok(request) => request,\n            Err(error) => {\n                tracing::warn!(service = %file.name, error = %error, "invalid service IPC JSON");\n                continue;\n            }\n        };\n        let (response, shutdown) =\n            dispatch(request, &file, &token, &memory, executor.as_ref()).await;\n        let mut payload = serde_json::to_vec(&response)?;\n        if payload.len().saturating_add(1) > SERVICE_IPC_RESPONSE_MAX_BYTES {\n            payload = serde_json::to_vec(&ServiceResponse::Error {\n                code: "SVC4002".into(),\n                message: "service IPC response exceeded frame limit".into(),\n            })?;\n        }\n        payload.push(b'\\n');\n        if let Err(error) = tokio::time::timeout(SERVICE_IPC_TIMEOUT, async {\n            write.write_all(&payload).await?;\n            write.shutdown().await\n        })\n        .await\n        .map_err(|_| anyhow::anyhow!("service IPC response write timed out"))\n        .and_then(|result| result.map_err(anyhow::Error::from))\n        {\n            tracing::warn!(service = %file.name, error = %error, "service IPC response write failed");\n        }\n        if shutdown {\n            return Ok(());\n        }'''
    source = replace_once(source, old, new, "service host IPC loop")
    source = replace_once(
        source,
        '''    if request.token() != token {''',
        '''    if !constant_time_eq(request.token().as_bytes(), token.as_bytes()) {''',
        "service token comparison",
    )

    helper_anchor = "fn execution_error_response(\n"
    helper = '''pub(crate) async fn read_bounded_line<R>(\n    reader: R,\n    max_bytes: usize,\n    label: &str,\n) -> anyhow::Result<String>\nwhere\n    R: AsyncRead + Unpin,\n{\n    let limit = u64::try_from(max_bytes)\n        .unwrap_or(u64::MAX)\n        .saturating_add(1);\n    let mut reader = BufReader::new(reader).take(limit);\n    let mut line = String::new();\n    let bytes = reader.read_line(&mut line).await?;\n    if bytes == 0 {\n        anyhow::bail!("{label} is empty");\n    }\n    if bytes > max_bytes {\n        anyhow::bail!("{label} exceeded {max_bytes} bytes");\n    }\n    if !line.ends_with('\\n') {\n        anyhow::bail!("{label} is not newline terminated");\n    }\n    Ok(line)\n}\n\nfn constant_time_eq(left: &[u8], right: &[u8]) -> bool {\n    if left.len() != right.len() {\n        return false;\n    }\n    let mut diff = 0u8;\n    for (&left, &right) in left.iter().zip(right.iter()) {\n        diff |= left ^ right;\n    }\n    diff == 0\n}\n\n'''
    source = replace_once(source, helper_anchor, helper + helper_anchor, "service IPC helpers")

    tests_anchor = "    #[test]\n    fn memory_round_trip() {"
    tests = '''    #[tokio::test]\n    async fn service_frame_reader_rejects_oversized_and_unterminated_frames() {\n        assert!(read_bounded_line(&b"abcd\\n"[..], 3, "test").await.is_err());\n        assert!(read_bounded_line(&b"abc"[..], 3, "test").await.is_err());\n        assert_eq!(\n            read_bounded_line(&b"abc\\n"[..], 4, "test").await.unwrap(),\n            "abc\\n"\n        );\n    }\n\n    #[test]\n    fn service_token_compare_checks_content_and_length() {\n        assert!(constant_time_eq(b"secret", b"secret"));\n        assert!(!constant_time_eq(b"secret", b"secreu"));\n        assert!(!constant_time_eq(b"secret", b"short"));\n    }\n\n    #[test]\n    fn memory_round_trip() {'''
    source = replace_once(source, tests_anchor, tests, "service IPC tests")
    path.write_text(source)

    path = Path("crates/service-runtime/src/manager.rs")
    source = path.read_text()
    source = replace_once(
        source,
        '''use super::{\n    RestartPolicy, ServiceCatalog, ServiceFile, ServiceMode, ServiceReady, ServiceRequest,\n    ServiceResponse,\n};''',
        '''use super::{\n    read_bounded_line, RestartPolicy, ServiceCatalog, ServiceFile, ServiceMode, ServiceReady,\n    ServiceRequest, ServiceResponse, SERVICE_IPC_REQUEST_MAX_BYTES,\n    SERVICE_IPC_RESPONSE_MAX_BYTES, SERVICE_IPC_TIMEOUT,\n};''',
        "manager service IPC imports",
    )
    old_rpc = '''async fn rpc(address: SocketAddr, request: ServiceRequest) -> anyhow::Result<ServiceResponse> {\n    let stream = TcpStream::connect(address).await?;\n    let (read, mut write) = stream.into_split();\n    write\n        .write_all(format!("{}\\n", serde_json::to_string(&request)?).as_bytes())\n        .await?;\n    write.shutdown().await?;\n    let mut reader = BufReader::new(read);\n    let mut line = String::new();\n    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))\n        .await\n        .map_err(|_| anyhow::anyhow!("service IPC timeout"))??;\n    Ok(serde_json::from_str(line.trim())?)\n}'''
    new_rpc = '''async fn rpc(address: SocketAddr, request: ServiceRequest) -> anyhow::Result<ServiceResponse> {\n    if !address.ip().is_loopback() {\n        anyhow::bail!("service IPC endpoint must be loopback");\n    }\n    let mut payload = serde_json::to_vec(&request)?;\n    if payload.len().saturating_add(1) > SERVICE_IPC_REQUEST_MAX_BYTES {\n        anyhow::bail!(\n            "service IPC request exceeded {} bytes",\n            SERVICE_IPC_REQUEST_MAX_BYTES\n        );\n    }\n    payload.push(b'\\n');\n\n    let stream = tokio::time::timeout(SERVICE_IPC_TIMEOUT, TcpStream::connect(address))\n        .await\n        .map_err(|_| anyhow::anyhow!("service IPC connect timeout"))??;\n    let (read, mut write) = stream.into_split();\n    tokio::time::timeout(SERVICE_IPC_TIMEOUT, async {\n        write.write_all(&payload).await?;\n        write.shutdown().await\n    })\n    .await\n    .map_err(|_| anyhow::anyhow!("service IPC write timeout"))??;\n    let line = tokio::time::timeout(\n        SERVICE_IPC_TIMEOUT,\n        read_bounded_line(read, SERVICE_IPC_RESPONSE_MAX_BYTES, "service response"),\n    )\n    .await\n    .map_err(|_| anyhow::anyhow!("service IPC response timeout"))??;\n    Ok(serde_json::from_str(line.trim())?)\n}'''
    source = replace_once(source, old_rpc, new_rpc, "manager rpc hardening")
    path.write_text(source)


def patch_mother_ipc() -> None:
    path = Path("crates/service-runtime/src/mother.rs")
    source = path.read_text()
    source = replace_once(
        source,
        "use std::sync::Arc;",
        "use std::sync::Arc;\nuse std::time::Duration;",
        "mother Duration import",
    )
    marker = "use crate::{ServiceCallError, ServiceManager, ServiceSnapshot};\n"
    constants = '''use crate::{ServiceCallError, ServiceManager, ServiceSnapshot};\n\nconst MOTHER_REQUEST_MAX_BYTES: usize = 4 * 1024 * 1024;\nconst MOTHER_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;\nconst MOTHER_FRAME_TIMEOUT: Duration = Duration::from_secs(5);\nconst MOTHER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);\nconst MAX_MOTHER_CONNECTIONS: usize = 128;\n'''
    source = replace_once(source, marker, constants, "mother IPC constants")
    source = replace_once(
        source,
        '''    let token: Arc<str> = Arc::<str>::from(token);\n    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);''',
        '''    let token: Arc<str> = Arc::<str>::from(token);\n    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_MOTHER_CONNECTIONS));\n    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);''',
        "mother connection limiter init",
    )
    accept_old = '''                let manager = manager.clone();\n                let token = token.clone();\n                let shutdown_tx = shutdown_tx.clone();\n                tokio::spawn(async move {\n                    if let Err(error) = handle_connection(stream, manager, token, shutdown_tx).await {\n                        tracing::warn!(error = %error, "Service Mother request failed");\n                    }\n                });'''
    accept_new = '''                let permit = match connections.clone().try_acquire_owned() {\n                    Ok(permit) => permit,\n                    Err(_) => {\n                        tracing::warn!(%peer, limit = MAX_MOTHER_CONNECTIONS, "Service Mother connection limit reached");\n                        drop(stream);\n                        continue;\n                    }\n                };\n                let manager = manager.clone();\n                let token = token.clone();\n                let shutdown_tx = shutdown_tx.clone();\n                tokio::spawn(async move {\n                    let _permit = permit;\n                    if let Err(error) = handle_connection(stream, manager, token, shutdown_tx).await {\n                        tracing::warn!(error = %error, "Service Mother request failed");\n                    }\n                });'''
    source = replace_once(source, accept_old, accept_new, "mother connection limiter")
    source = replace_once(
        source,
        '''    let line = read_bounded_line(read, 4 * 1024 * 1024, "request").await?;''',
        '''    let line = tokio::time::timeout(\n        MOTHER_FRAME_TIMEOUT,\n        read_bounded_line(read, MOTHER_REQUEST_MAX_BYTES, "request"),\n    )\n    .await\n    .map_err(|_| anyhow::anyhow!("Service Mother request frame timed out"))??;''',
        "mother inbound frame timeout",
    )

    old_write = '''async fn write_response(\n    write: &mut tokio::net::tcp::OwnedWriteHalf,\n    response: &ServiceMotherResponse,\n) -> anyhow::Result<()> {\n    write\n        .write_all(format!("{}\\n", serde_json::to_string(response)?).as_bytes())\n        .await?;\n    write.shutdown().await?;\n    Ok(())\n}'''
    new_write = '''async fn write_response(\n    write: &mut tokio::net::tcp::OwnedWriteHalf,\n    response: &ServiceMotherResponse,\n) -> anyhow::Result<()> {\n    let mut payload = serde_json::to_vec(response)?;\n    if payload.len().saturating_add(1) > MOTHER_RESPONSE_MAX_BYTES {\n        anyhow::bail!("Service Mother response exceeded {MOTHER_RESPONSE_MAX_BYTES} bytes");\n    }\n    payload.push(b'\\n');\n    tokio::time::timeout(MOTHER_FRAME_TIMEOUT, async {\n        write.write_all(&payload).await?;\n        write.shutdown().await\n    })\n    .await\n    .map_err(|_| anyhow::anyhow!("Service Mother response write timed out"))??;\n    Ok(())\n}'''
    source = replace_once(source, old_write, new_write, "mother response writer")

    old_rpc = '''    let stream = TcpStream::connect(address).await?;\n    let (read, mut write) = stream.into_split();\n    write\n        .write_all(format!("{}\\n", serde_json::to_string(&request)?).as_bytes())\n        .await?;\n    write.shutdown().await?;\n    let line = read_bounded_line(read, 8 * 1024 * 1024, "response").await?;\n    Ok(serde_json::from_str(line.trim())?)'''
    new_rpc = '''    let mut payload = serde_json::to_vec(&request)?;\n    if payload.len().saturating_add(1) > MOTHER_REQUEST_MAX_BYTES {\n        anyhow::bail!("Service Mother request exceeded {MOTHER_REQUEST_MAX_BYTES} bytes");\n    }\n    payload.push(b'\\n');\n    let stream = tokio::time::timeout(MOTHER_FRAME_TIMEOUT, TcpStream::connect(address))\n        .await\n        .map_err(|_| anyhow::anyhow!("Service Mother connect timed out"))??;\n    let (read, mut write) = stream.into_split();\n    tokio::time::timeout(MOTHER_FRAME_TIMEOUT, async {\n        write.write_all(&payload).await?;\n        write.shutdown().await\n    })\n    .await\n    .map_err(|_| anyhow::anyhow!("Service Mother request write timed out"))??;\n    let line = tokio::time::timeout(\n        MOTHER_RESPONSE_TIMEOUT,\n        read_bounded_line(read, MOTHER_RESPONSE_MAX_BYTES, "response"),\n    )\n    .await\n    .map_err(|_| anyhow::anyhow!("Service Mother response timed out"))??;\n    Ok(serde_json::from_str(line.trim())?)'''
    source = replace_once(source, old_rpc, new_rpc, "mother outbound rpc timeouts")
    path.write_text(source)


def patch_readiness_verification() -> None:
    path = Path("crates/service-runtime/src/manager.rs")
    source = path.read_text()
    anchor = '''    if ready.service != file.name {\n        cleanup_failed_spawn(&alias, &mut child).await;\n        anyhow::bail!("service readiness identity mismatch");\n    }\n\n    let service_name = file.name.clone();'''
    replacement = '''    if ready.service != file.name {\n        cleanup_failed_spawn(&alias, &mut child).await;\n        anyhow::bail!("service readiness identity mismatch");\n    }\n    if !ready.address.ip().is_loopback() {\n        cleanup_failed_spawn(&alias, &mut child).await;\n        anyhow::bail!("service readiness advertised a non-loopback endpoint");\n    }\n    if child.id() != Some(ready.pid) {\n        cleanup_failed_spawn(&alias, &mut child).await;\n        anyhow::bail!("service readiness PID does not match child process");\n    }\n\n    let service_name = file.name.clone();'''
    source = replace_once(source, anchor, replacement, "service readiness validation")
    path.write_text(source)

    path = Path("crates/backend/src/service_mother.rs")
    source = path.read_text()
    source = replace_once(
        source,
        '''    if child.id().is_some_and(|pid| pid != ready.pid) {''',
        '''    if child.id() != Some(ready.pid) {''',
        "strict Service Mother readiness PID",
    )
    path.write_text(source)


def patch_parallel_snapshots() -> None:
    path = Path("crates/service-runtime/src/manager.rs")
    source = path.read_text()
    start = source.find("    pub async fn snapshot(&self) -> Vec<ServiceSnapshot> {")
    end = source.find("\n    pub async fn shutdown_all(&self) {", start)
    if start < 0 or end < 0:
        raise SystemExit("snapshot function boundaries missing")
    old = source[start:end]
    if "for handle in handles" not in old or "rpc(address, ServiceRequest::Health" not in old:
        raise SystemExit("snapshot function shape unexpected")
    new = '''    pub async fn snapshot(&self) -> Vec<ServiceSnapshot> {\n        if let Some(mother) = &self.mother {\n            return match mother.snapshot().await {\n                Ok(services) => services,\n                Err(error) => vec![ServiceSnapshot {\n                    name: "service-mother".into(),\n                    title: "RBE Service Mother".into(),\n                    pid: None,\n                    state: ServiceRuntimeState::Unknown,\n                    mode: ServiceMode::Resident,\n                    restart: RestartPolicy::OnFailure,\n                    restart_attempts: 0,\n                    idle_timeout_ms: 0,\n                    ready: false,\n                    health_checked: true,\n                    health: None,\n                    health_error: Some(error.to_string()),\n                }],\n            };\n        }\n        let handles = self\n            .services\n            .read()\n            .await\n            .values()\n            .cloned()\n            .collect::<Vec<_>>();\n        let mut snapshots = tokio::task::JoinSet::new();\n        for handle in handles {\n            snapshots.spawn(snapshot_managed_service(handle));\n        }\n        let mut out = Vec::new();\n        while let Some(result) = snapshots.join_next().await {\n            match result {\n                Ok(snapshot) => out.push(snapshot),\n                Err(error) => tracing::warn!(\n                    error = %error,\n                    "service snapshot task failed"\n                ),\n            }\n        }\n        out.sort_by(|left, right| left.name.cmp(&right.name));\n        out\n    }\n'''
    source = source[:start] + new + source[end:]

    helper_anchor = "fn health_value_ready(value: &Value) -> bool {\n"
    helper = '''async fn snapshot_managed_service(handle: Arc<Mutex<Managed>>) -> ServiceSnapshot {\n    let (mut snapshot, health_target) = {\n        let mut service = handle.lock().await;\n        let restarting = service.restarting;\n        let wakeable = service.wakeable();\n        let exit_observed = service.exit_observed;\n        let restart = service.file.restart;\n        let (pid, state, health_target) = match service.process.as_mut() {\n            None if restarting => (None, ServiceRuntimeState::Restarting, None),\n            None if wakeable && !exit_observed => (None, ServiceRuntimeState::Dormant, None),\n            None => (None, ServiceRuntimeState::Stopped, None),\n            Some(process) => match process.child.try_wait() {\n                Ok(None) => (\n                    process.child.id(),\n                    ServiceRuntimeState::Running,\n                    Some((process.ready.address, process.token.clone())),\n                ),\n                Ok(Some(_)) if restarting => (None, ServiceRuntimeState::Restarting, None),\n                Ok(Some(status)) if exit_observed || !should_restart(restart, status.success()) => {\n                    (None, ServiceRuntimeState::Stopped, None)\n                }\n                Ok(Some(_)) => (None, ServiceRuntimeState::Restarting, None),\n                Err(_) => (None, ServiceRuntimeState::Unknown, None),\n            },\n        };\n        if health_target.is_some() {\n            service.active_calls = service.active_calls.saturating_add(1);\n        }\n        (\n            ServiceSnapshot {\n                name: service.file.name.clone(),\n                title: service.file.title.clone(),\n                pid,\n                state,\n                mode: service.file.mode,\n                restart: service.file.restart,\n                restart_attempts: service.restart_attempts,\n                idle_timeout_ms: service.file.idle_timeout_ms,\n                ready: state == ServiceRuntimeState::Dormant,\n                health_checked: false,\n                health: None,\n                health_error: None,\n            },\n            health_target,\n        )\n    };\n\n    if let Some((address, token)) = health_target {\n        snapshot.health_checked = true;\n        let response = rpc(address, ServiceRequest::Health { token }).await;\n        {\n            let mut service = handle.lock().await;\n            service.active_calls = service.active_calls.saturating_sub(1);\n        }\n        match response {\n            Ok(ServiceResponse::Ok { value }) => {\n                snapshot.ready = health_value_ready(&value);\n                snapshot.health = Some(value);\n            }\n            Ok(ServiceResponse::Error { code, message }) => {\n                snapshot.health_error = Some(format!("{code}: {message}"));\n            }\n            Err(error) => {\n                snapshot.health_error = Some(error.to_string());\n            }\n        }\n    }\n    snapshot\n}\n\n'''
    source = replace_once(source, helper_anchor, helper + helper_anchor, "parallel snapshot helper")
    path.write_text(source)


MODES = {
    "process-tree": patch_process_tree_liveness,
    "service-ipc": patch_service_ipc,
    "mother-ipc": patch_mother_ipc,
    "readiness": patch_readiness_verification,
    "snapshots": patch_parallel_snapshots,
}

if len(sys.argv) != 2 or sys.argv[1] not in MODES:
    raise SystemExit(f"usage: {sys.argv[0]} <{'|'.join(MODES)}>")
MODES[sys.argv[1]]()
