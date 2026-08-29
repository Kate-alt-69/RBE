use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

const MAX_FFMPEG_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_FFMPEG_LOG_BYTES: usize = 4 * 1024 * 1024;
const ERROR_TEXT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct FfmpegPolicy {
    pub executable: PathBuf,
    pub timeout: Duration,
    pub max_log_bytes: usize,
}

impl FfmpegPolicy {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            timeout: Duration::from_secs(15 * 60),
            max_log_bytes: 1024 * 1024,
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        if !self.executable.is_absolute() {
            anyhow::bail!("Video Manager FFmpeg executable path must be absolute");
        }
        let metadata = std::fs::symlink_metadata(&self.executable).with_context(|| {
            format!(
                "inspect configured FFmpeg executable {}",
                self.executable.display()
            )
        })?;
        if !metadata.file_type().is_file() {
            anyhow::bail!("Video Manager FFmpeg executable is not a regular file");
        }
        if self.timeout.is_zero() || self.timeout > MAX_FFMPEG_TIMEOUT {
            anyhow::bail!(
                "Video Manager FFmpeg timeout must be greater than zero and at most {:?}",
                MAX_FFMPEG_TIMEOUT
            );
        }
        if self.max_log_bytes == 0 || self.max_log_bytes > MAX_FFMPEG_LOG_BYTES {
            anyhow::bail!(
                "Video Manager FFmpeg log cap must be between 1 and {MAX_FFMPEG_LOG_BYTES} bytes"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedMedia {
    pub profile: &'static str,
    pub container: &'static str,
    pub video_codec: &'static str,
    pub audio_codec: &'static str,
    pub size_bytes: u64,
}

pub async fn run_ffmpeg_normalize(
    input_path: &Path,
    output_path: &Path,
    policy: &FfmpegPolicy,
) -> anyhow::Result<NormalizedMedia> {
    policy.validate()?;
    validate_paths(input_path, output_path).await?;

    let args = normalization_args(input_path, output_path);
    let mut command = Command::new(&policy.executable);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn configured FFmpeg {}", policy.executable.display()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("Video Manager FFmpeg stderr pipe is unavailable"))?;
    let stderr_task = tokio::spawn(read_bounded_output(stderr, policy.max_log_bytes));

    let status = match tokio::time::timeout(policy.timeout, child.wait()).await {
        Ok(result) => result.context("wait for Video Manager FFmpeg")?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stderr_task.abort();
            let _ = tokio::fs::remove_file(output_path).await;
            anyhow::bail!(
                "Video Manager FFmpeg exceeded timeout of {:?}",
                policy.timeout
            );
        }
    };

    let stderr = stderr_task
        .await
        .context("join Video Manager FFmpeg stderr reader")??;
    if !status.success() {
        let _ = tokio::fs::remove_file(output_path).await;
        let detail = bounded_error_text(&stderr);
        anyhow::bail!("Video Manager FFmpeg exited with {status}: {detail}");
    }

    let metadata = tokio::fs::symlink_metadata(output_path)
        .await
        .with_context(|| {
            format!(
                "inspect Video Manager normalized output {}",
                output_path.display()
            )
        })?;
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        let _ = tokio::fs::remove_file(output_path).await;
        anyhow::bail!("Video Manager FFmpeg did not produce a non-empty regular output file");
    }

    Ok(NormalizedMedia {
        profile: "standard",
        container: "mp4",
        video_codec: "h264",
        audio_codec: "aac",
        size_bytes: metadata.len(),
    })
}

async fn validate_paths(input_path: &Path, output_path: &Path) -> anyhow::Result<()> {
    if !input_path.is_absolute() || !output_path.is_absolute() {
        anyhow::bail!("Video Manager FFmpeg input and output paths must be absolute");
    }
    let input_metadata = tokio::fs::symlink_metadata(input_path)
        .await
        .with_context(|| format!("inspect Video Manager FFmpeg input {}", input_path.display()))?;
    if !input_metadata.file_type().is_file() || input_metadata.len() == 0 {
        anyhow::bail!("Video Manager FFmpeg input must be a non-empty regular file");
    }
    if tokio::fs::symlink_metadata(output_path).await.is_ok() {
        anyhow::bail!("Video Manager FFmpeg output already exists; refusing to overwrite it");
    }
    let output_parent = output_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Video Manager FFmpeg output has no parent directory"))?;
    let parent_metadata = tokio::fs::symlink_metadata(output_parent)
        .await
        .with_context(|| {
            format!(
                "inspect Video Manager FFmpeg output directory {}",
                output_parent.display()
            )
        })?;
    if !parent_metadata.file_type().is_dir() {
        anyhow::bail!("Video Manager FFmpeg output parent is not a directory");
    }
    let canonical_input = tokio::fs::canonicalize(input_path).await?;
    let canonical_parent = tokio::fs::canonicalize(output_parent).await?;
    let output_name = output_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Video Manager FFmpeg output has no filename"))?;
    if canonical_input == canonical_parent.join(output_name) {
        anyhow::bail!("Video Manager FFmpeg input and output must be different files");
    }
    Ok(())
}

fn normalization_args(input_path: &Path, output_path: &Path) -> Vec<OsString> {
    [
        OsString::from("-nostdin"),
        OsString::from("-hide_banner"),
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-n"),
        OsString::from("-protocol_whitelist"),
        OsString::from("file,crypto,data"),
        OsString::from("-i"),
        input_path.as_os_str().to_os_string(),
        OsString::from("-map"),
        OsString::from("0:v:0"),
        OsString::from("-map"),
        OsString::from("0:a:0?"),
        OsString::from("-sn"),
        OsString::from("-dn"),
        OsString::from("-c:v"),
        OsString::from("libx264"),
        OsString::from("-preset"),
        OsString::from("medium"),
        OsString::from("-crf"),
        OsString::from("23"),
        OsString::from("-pix_fmt"),
        OsString::from("yuv420p"),
        OsString::from("-c:a"),
        OsString::from("aac"),
        OsString::from("-b:a"),
        OsString::from("160k"),
        OsString::from("-movflags"),
        OsString::from("+faststart"),
        OsString::from("-f"),
        OsString::from("mp4"),
        output_path.as_os_str().to_os_string(),
    ]
    .into_iter()
    .collect()
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
            .context("read Video Manager FFmpeg pipe")?;
        if count == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(output.len());
        if count <= remaining {
            output.extend_from_slice(&buffer[..count]);
        } else {
            output.extend_from_slice(&buffer[..remaining]);
            overflow = true;
        }
    }
    if overflow {
        anyhow::bail!("Video Manager FFmpeg log output exceeded {max_bytes} bytes");
    }
    Ok(output)
}

fn bounded_error_text(bytes: &[u8]) -> String {
    let end = bytes.len().min(ERROR_TEXT_BYTES);
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_profile_is_fixed_and_local_only() {
        let args = normalization_args(Path::new("/input.part"), Path::new("/output.mp4"));
        let rendered = args
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(rendered.windows(2).any(|pair| pair == ["-c:v", "libx264"]));
        assert!(rendered.windows(2).any(|pair| pair == ["-c:a", "aac"]));
        assert!(rendered
            .windows(2)
            .any(|pair| pair == ["-protocol_whitelist", "file,crypto,data"]));
        assert!(!rendered.iter().any(|arg| arg.contains("http")));
        assert_eq!(rendered.last().map(|value| value.as_ref()), Some("/output.mp4"));
    }

    #[test]
    fn policy_rejects_relative_executable_before_spawn() {
        let policy = FfmpegPolicy::new("ffmpeg");
        let error = policy.validate().expect_err("relative executable must fail");
        assert!(error.to_string().contains("absolute"));
    }

    #[tokio::test]
    async fn path_validation_rejects_existing_output() {
        let root = std::env::temp_dir().join(format!("rbe-ffmpeg-paths-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let input = root.join("input.part");
        let output = root.join("output.mp4");
        std::fs::write(&input, b"input").unwrap();
        std::fs::write(&output, b"existing").unwrap();
        let error = validate_paths(&input, &output)
            .await
            .expect_err("existing output must fail");
        assert!(error.to_string().contains("already exists"));
        let _ = std::fs::remove_dir_all(root);
    }
}
