from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


# Shared parent bootstrap secret framing.
path = Path("crates/service-runtime/src/lib.rs")
source = path.read_text()
source = replace_once(
    source,
    "use std::io::{IsTerminal, Read, Write};",
    "use std::io::{BufRead, IsTerminal, Read, Write};",
    "stdio import",
)
anchor = "pub fn parent_liveness_signal_if_configured(\n) -> anyhow::Result<Option<tokio::sync::oneshot::Receiver<()>>> {"
helper = r'''fn validate_parent_bootstrap_secret(secret: &str) -> anyhow::Result<()> {
    if secret.len() != 64 || !secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("parent bootstrap secret must be a 256-bit hexadecimal value");
    }
    Ok(())
}

fn read_parent_bootstrap_secret<R: BufRead>(
    reader: &mut R,
    label: &str,
) -> anyhow::Result<String> {
    // 64 hex bytes plus optional CR and required LF. `take` prevents a
    // malformed parent from making the child allocate an unbounded line.
    let mut limited = reader.take(66);
    let mut line = String::new();
    let bytes = limited.read_line(&mut line)?;
    if bytes == 0 {
        anyhow::bail!("{label} parent bootstrap pipe closed before authentication");
    }
    if !line.ends_with('\n') {
        anyhow::bail!("{label} parent bootstrap secret is not newline terminated");
    }
    line.pop();
    if line.ends_with('\r') {
        line.pop();
    }
    validate_parent_bootstrap_secret(&line)?;
    Ok(line)
}

/// Read a one-time authentication value from the same inherited stdin pipe
/// that remains open afterward as the parent-liveness signal. Returns `None`
/// for direct/manual process launches that did not configure that pipe.
pub fn read_parent_bootstrap_secret_if_configured(
    label: &str,
) -> anyhow::Result<Option<String>> {
    if std::env::var_os("RBE_PARENT_LIVENESS_PIPE").is_none() {
        return Ok(None);
    }
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    read_parent_bootstrap_secret(&mut stdin, label).map(Some)
}

/// Send the one-time child authentication value without exposing it in the
/// process command line or environment. The caller must retain `writer` after
/// this returns so EOF continues to mean parent death to the child.
pub async fn write_parent_bootstrap_secret<W>(
    writer: &mut W,
    secret: &str,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    validate_parent_bootstrap_secret(secret)?;
    writer.write_all(secret.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

'''
if anchor not in source:
    raise SystemExit("parent liveness insertion anchor missing")
source = source.replace(anchor, helper + anchor, 1)

test_anchor = "    #[test]\n    fn service_token_compare_checks_content_and_length() {"
test = r'''    #[test]
    fn parent_bootstrap_secret_reader_is_bounded_and_validates_token() {
        let token = "ab".repeat(32);
        let mut valid = std::io::Cursor::new(format!("{token}\n").into_bytes());
        assert_eq!(
            read_parent_bootstrap_secret(&mut valid, "test").unwrap(),
            token
        );

        let mut short = std::io::Cursor::new(b"abcd\n".to_vec());
        assert!(read_parent_bootstrap_secret(&mut short, "test").is_err());

        let mut oversized = std::io::Cursor::new(format!("{}\n", "a".repeat(80)).into_bytes());
        assert!(read_parent_bootstrap_secret(&mut oversized, "test").is_err());
    }

    #[test]
    fn service_token_compare_checks_content_and_length() {'''
source = replace_once(source, test_anchor, test, "bootstrap secret test insertion")
path.write_text(source)


# .service child consumes pipe token when parent liveness is configured.
path = Path("crates/backend/src/service_boot.rs")
source = path.read_text()
old = '''    let token = value("--service-token")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("backend --service-host requires --service-token <token>")
        })?;
'''
new = '''    let token = match service_runtime::read_parent_bootstrap_secret_if_configured("service host")? {
        Some(token) => token,
        None => value("--service-token")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("backend --service-host requires parent authentication")
            })?,
    };
'''
source = replace_once(source, old, new, "service host token block")
path.write_text(source)


# Service Mother consumes the inherited token and parent sends it over stdin.
path = Path("crates/backend/src/service_mother.rs")
source = path.read_text()
old = '''    let token = flag_value(args, "--service-token")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("backend --service-mother requires --service-token <token>")
        })?;
'''
new = '''    let token = match service_runtime::read_parent_bootstrap_secret_if_configured("Service Mother")? {
        Some(token) => token,
        None => flag_value(args, "--service-token")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("backend --service-mother requires parent authentication"))?,
    };
'''
source = replace_once(source, old, new, "Service Mother token block")
source = replace_once(
    source,
    '''        .args(["--service-mother", "--launch-separate", "--service-token"])
        .arg(&token)
''',
    '''        .args(["--service-mother", "--launch-separate"])
''',
    "Service Mother command-line token",
)
source = replace_once(
    source,
    "    let liveness = match child.stdin.take() {",
    "    let mut liveness = match child.stdin.take() {",
    "Service Mother liveness mutability",
)
anchor = '''    };
    let stdout = match child.stdout.take() {
'''
insertion = '''    };
    if let Err(error) = service_runtime::write_parent_bootstrap_secret(&mut liveness, &token).await {
        cleanup_failed_spawn(&alias, &mut child).await;
        return Err(anyhow::anyhow!(
            "send Service Mother parent bootstrap secret: {error}"
        ));
    }
    let stdout = match child.stdout.take() {
'''
source = replace_once(source, anchor, insertion, "Service Mother bootstrap send")
path.write_text(source)


# Individual service children use the same private pipe handshake.
path = Path("crates/service-runtime/src/manager.rs")
source = path.read_text()
source = replace_once(
    source,
    '''        .args(["--service-host", "--service-file"])
        .arg(&file.path)
        .arg("--service-token")
        .arg(&token)
''',
    '''        .args(["--service-host", "--service-file"])
        .arg(&file.path)
''',
    "service command-line token",
)
source = replace_once(
    source,
    "    let liveness = match child.stdin.take() {",
    "    let mut liveness = match child.stdin.take() {",
    "service liveness mutability",
)
anchor = '''    };
    let stdout = match child.stdout.take() {
'''
insertion = '''    };
    if let Err(error) = super::write_parent_bootstrap_secret(&mut liveness, &token).await {
        cleanup_failed_spawn(&alias, &mut child).await;
        return Err(anyhow::anyhow!(
            "send service {:?} parent bootstrap secret: {error}",
            file.name
        ));
    }
    let stdout = match child.stdout.take() {
'''
source = replace_once(source, anchor, insertion, "service bootstrap send")
path.write_text(source)
