from pathlib import Path


Path("engine/crates/video-manager/src/ffmpeg_capabilities.rs").write_text(r'''use std::collections::HashSet;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::FfmpegPolicy;

const MAX_CAPABILITY_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const CAPABILITY_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const HARDWARE_PROBE_BUDGET: Duration = Duration::from_secs(10);
const HARDWARE_ENCODER_TIMEOUT: Duration = Duration::from_secs(3);
const HARDWARE_ENCODER_LOG_BYTES: usize = 64 * 1024;
const ERROR_TEXT_BYTES: usize = 8 * 1024;
const HARDWARE_H264_ENCODERS: &[&str] = &[
    "h264_nvenc",
    "h264_qsv",
    "h264_amf",
    "h264_videotoolbox",
    "h264_vaapi",
    "h264_v4l2m2m",
    "h264_rkmpp",
    "h264_mf",
];
const AUTO_VERIFIABLE_H264_ENCODERS: &[&str] = &[
    "h264_nvenc",
    "h264_qsv",
    "h264_amf",
    "h264_videotoolbox",
    "h264_mf",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FfmpegCapabilities {
    pub software_h264: bool,
    pub aac: bool,
    pub hardware_h264_encoders: Vec<String>,
    pub verified_hardware_h264_encoders: Vec<String>,
}

pub async fn probe_ffmpeg_capabilities(
    policy: &FfmpegPolicy,
) -> anyhow::Result<FfmpegCapabilities> {
    policy.validate()?;
    let output_cap = policy.max_log_bytes.clamp(1, MAX_CAPABILITY_OUTPUT_BYTES);
    let timeout = policy.timeout.min(CAPABILITY_PROBE_TIMEOUT);
    let listing = run_encoder_listing(policy, timeout, output_cap).await?;
    let mut capabilities = parse_encoder_listing(&listing);
    validate_required_capabilities(&capabilities)?;
    capabilities.verified_hardware_h264_encoders = verify_hardware_encoders(
        policy,
        &capabilities.hardware_h264_encoders,
    )
    .await;
    Ok(capabilities)
}

async fn run_encoder_listing(
    policy: &FfmpegPolicy,
    timeout: Duration,
    max_bytes: usize,
) -> anyhow::Result<String> {
    let mut command = Command::new(&policy.executable);
    command
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-encoders")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().with_context(|| {
        format!(
            "spawn configured FFmpeg capability probe {}",
            policy.executable.display()
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Video Manager FFmpeg capability stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("Video Manager FFmpeg capability stderr unavailable"))?;
    let stdout_task = tokio::spawn(read_bounded_output(stdout, max_bytes));
    let stderr_task = tokio::spawn(read_bounded_output(stderr, max_bytes));

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.context("wait for Video Manager FFmpeg capability probe")?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            anyhow::bail!(
                "Video Manager FFmpeg capability probe exceeded timeout of {:?}",
                timeout
            );
        }
    };

    let stdout = stdout_task
        .await
        .context("join Video Manager FFmpeg capability stdout reader")??;
    let stderr = stderr_task
        .await
        .context("join Video Manager FFmpeg capability stderr reader")??;
    if !status.success() {
        let detail = bounded_text(if stderr.is_empty() { &stdout } else { &stderr });
        anyhow::bail!("Video Manager FFmpeg capability probe exited with {status}: {detail}");
    }

    let mut combined = stdout;
    combined.extend_from_slice(&stderr);
    Ok(String::from_utf8_lossy(&combined).into_owned())
}

async fn verify_hardware_encoders(
    policy: &FfmpegPolicy,
    advertised: &[String],
) -> Vec<String> {
    let started = Instant::now();
    let mut verified = Vec::new();
    for encoder in advertised
        .iter()
        .filter(|encoder| auto_verifiable_hardware_encoder(encoder))
    {
        let remaining = HARDWARE_PROBE_BUDGET.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let timeout = remaining.min(HARDWARE_ENCODER_TIMEOUT);
        match smoke_test_hardware_encoder(policy, encoder, timeout).await {
            Ok(true) => verified.push(encoder.clone()),
            Ok(false) => {}
            Err(error) => tracing::debug!(
                encoder = %encoder,
                error = %error,
                "Video Manager hardware encoder smoke test failed"
            ),
        }
    }
    verified
}

fn auto_verifiable_hardware_encoder(encoder: &str) -> bool {
    AUTO_VERIFIABLE_H264_ENCODERS.contains(&encoder)
}

async fn smoke_test_hardware_encoder(
    policy: &FfmpegPolicy,
    encoder: &str,
    timeout: Duration,
) -> anyhow::Result<bool> {
    if !auto_verifiable_hardware_encoder(encoder) {
        return Ok(false);
    }

    let mut command = Command::new(&policy.executable);
    command
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-v")
        .arg("error")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("color=c=black:s=64x64:r=1")
        .arg("-frames:v")
        .arg("1")
        .arg("-an")
        .arg("-c:v")
        .arg(encoder)
        .arg("-f")
        .arg("null")
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().with_context(|| {
        format!(
            "spawn configured FFmpeg hardware probe {}",
            policy.executable.display()
        )
    })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("Video Manager FFmpeg hardware probe stderr unavailable"))?;
    let stderr_task = tokio::spawn(read_bounded_output(stderr, HARDWARE_ENCODER_LOG_BYTES));

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.context("wait for Video Manager FFmpeg hardware probe")?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stderr_task.abort();
            tracing::debug!(
                encoder,
                timeout_ms = timeout.as_millis(),
                "Video Manager hardware encoder smoke test timed out"
            );
            return Ok(false);
        }
    };

    let stderr = stderr_task
        .await
        .context("join Video Manager FFmpeg hardware probe stderr reader")??;
    if status.success() {
        Ok(true)
    } else {
        tracing::debug!(
            encoder,
            detail = %bounded_text(&stderr),
            "Video Manager hardware encoder is advertised but unusable"
        );
        Ok(false)
    }
}

fn parse_encoder_listing(listing: &str) -> FfmpegCapabilities {
    let mut names = HashSet::new();
    for line in listing.lines() {
        let mut fields = line.split_whitespace();
        let Some(flags) = fields.next() else {
            continue;
        };
        if flags.len() < 6 || !matches!(flags.as_bytes().first(), Some(b'V' | b'A')) {
            continue;
        }
        if let Some(name) = fields.next() {
            names.insert(name);
        }
    }

    FfmpegCapabilities {
        software_h264: names.contains("libx264"),
        aac: names.contains("aac"),
        hardware_h264_encoders: HARDWARE_H264_ENCODERS
            .iter()
            .filter(|encoder| names.contains(**encoder))
            .map(|encoder| (*encoder).to_string())
            .collect(),
        verified_hardware_h264_encoders: Vec::new(),
    }
}

fn validate_required_capabilities(capabilities: &FfmpegCapabilities) -> anyhow::Result<()> {
    if !capabilities.software_h264 {
        anyhow::bail!(
            "configured FFmpeg does not provide required libx264 encoder for Video Manager standard normalization"
        );
    }
    if !capabilities.aac {
        anyhow::bail!(
            "configured FFmpeg does not provide required AAC encoder for Video Manager standard normalization"
        );
    }
    Ok(())
}

async fn read_bounded_output<R>(mut reader: R, max_bytes: usize) -> anyhow::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(max_bytes.min(64 * 1024));
    let mut buffer = [0u8; 16 * 1024];
    let mut overflow = false;
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .context("read Video Manager FFmpeg capability output")?;
        if count == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(output.len());
        let accepted = count.min(remaining);
        output.extend_from_slice(&buffer[..accepted]);
        overflow |= accepted < count;
    }
    if overflow {
        anyhow::bail!("Video Manager FFmpeg capability output exceeded {max_bytes} bytes");
    }
    Ok(output)
}

fn bounded_text(bytes: &[u8]) -> String {
    let end = bytes.len().min(ERROR_TEXT_BYTES);
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_required_and_hardware_encoders_from_ffmpeg_listing() {
        let listing = r#"
Encoders:
 V..... = Video
 A..... = Audio
 V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder
 V....D h264_qsv             H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 (Intel Quick Sync Video acceleration)
 A..... aac                  AAC (Advanced Audio Coding)
"#;
        let capabilities = parse_encoder_listing(listing);
        assert!(capabilities.software_h264);
        assert!(capabilities.aac);
        assert_eq!(
            capabilities.hardware_h264_encoders,
            vec!["h264_nvenc".to_string(), "h264_qsv".to_string()]
        );
        assert!(capabilities.verified_hardware_h264_encoders.is_empty());
        validate_required_capabilities(&capabilities).unwrap();
    }

    #[test]
    fn only_direct_hardware_encoders_are_auto_verifiable() {
        for encoder in [
            "h264_nvenc",
            "h264_qsv",
            "h264_amf",
            "h264_videotoolbox",
            "h264_mf",
        ] {
            assert!(auto_verifiable_hardware_encoder(encoder));
        }
        for encoder in ["h264_vaapi", "h264_v4l2m2m", "h264_rkmpp"] {
            assert!(!auto_verifiable_hardware_encoder(encoder));
        }
    }

    #[test]
    fn required_profile_validation_fails_closed() {
        let capabilities = FfmpegCapabilities {
            software_h264: false,
            aac: true,
            hardware_h264_encoders: Vec::new(),
            verified_hardware_h264_encoders: Vec::new(),
        };
        let error = validate_required_capabilities(&capabilities).unwrap_err();
        assert!(error.to_string().contains("libx264"));
    }
}
''')

path = Path("engine/crates/backend/src/main.rs")
source = path.read_text()
old = '''                hardware_h264_encoders = ?ffmpeg_capabilities.hardware_h264_encoders,
                recovery_scan_secs = config.video_manager.worker_recovery_scan_secs,
'''
new = '''                hardware_h264_encoders = ?ffmpeg_capabilities.hardware_h264_encoders,
                verified_hardware_h264_encoders = ?ffmpeg_capabilities.verified_hardware_h264_encoders,
                recovery_scan_secs = config.video_manager.worker_recovery_scan_secs,
'''
if old not in source:
    raise SystemExit("backend FFmpeg capability logging anchor missing")
path.write_text(source.replace(old, new, 1))

path = Path("docs/video-manager.md")
source = path.read_text()
old = "Hardware acceleration is not selected yet; the current profile intentionally uses the deterministic software encoder."
new = """At worker boot, RBE parses the configured FFmpeg encoder table and requires the software `libx264` and AAC encoders used by the standard profile. Advertised H.264 hardware candidates are recorded from a strict RBE allowlist. Directly testable candidates (currently NVENC, Quick Sync, AMF, VideoToolbox, and Media Foundation) then receive a bounded one-frame synthetic smoke test; only encoders that actually initialize and encode successfully are reported as verified. Device-bound candidates such as VAAPI, V4L2 M2M, and RKMPP remain advertised-only until explicit device handling lands.

Hardware acceleration is still not selected for real normalization yet; the current profile intentionally uses deterministic `libx264`. The verified candidate set is the safety foundation for automatic selection rather than trusting FFmpeg's advertised encoder list."""
if old not in source:
    raise SystemExit("Video Manager hardware docs anchor missing")
source = source.replace(old, new, 1)
source = source.replace(
    "- FFmpeg hardware-acceleration probing and safe encoder selection",
    "- automatic selection/use of verified FFmpeg hardware encoders",
    1,
)
path.write_text(source)
