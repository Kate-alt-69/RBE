use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

const MAX_FFPROBE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_FFPROBE_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const ERROR_TEXT_BYTES: usize = 8 * 1024;
const MAX_VIDEO_DIMENSION: u32 = 32_768;
const MAX_VIDEO_FRAME_RATE: f64 = 1_000.0;

#[derive(Debug, Clone)]
pub struct FfprobePolicy {
    pub executable: PathBuf,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl FfprobePolicy {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            timeout: Duration::from_secs(30),
            max_output_bytes: 1024 * 1024,
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        if !self.executable.is_absolute() {
            anyhow::bail!("Video Manager FFprobe executable path must be absolute");
        }
        let metadata = std::fs::symlink_metadata(&self.executable).with_context(|| {
            format!(
                "inspect configured FFprobe executable {}",
                self.executable.display()
            )
        })?;
        if !metadata.file_type().is_file() {
            anyhow::bail!("Video Manager FFprobe executable is not a regular file");
        }
        if self.timeout.is_zero() || self.timeout > MAX_FFPROBE_TIMEOUT {
            anyhow::bail!(
                "Video Manager FFprobe timeout must be greater than zero and at most {:?}",
                MAX_FFPROBE_TIMEOUT
            );
        }
        if self.max_output_bytes == 0 || self.max_output_bytes > MAX_FFPROBE_OUTPUT_BYTES {
            anyhow::bail!(
                "Video Manager FFprobe output cap must be between 1 and {MAX_FFPROBE_OUTPUT_BYTES} bytes"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaProbe {
    pub format_names: Vec<String>,
    pub duration_secs: Option<f64>,
    pub bit_rate: Option<u64>,
    pub video_streams: Vec<VideoStreamProbe>,
    pub audio_streams: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoStreamProbe {
    pub index: u32,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub pixel_format: Option<String>,
    pub frame_rate: Option<f64>,
    pub bit_rate: Option<u64>,
    pub duration_secs: Option<f64>,
}

pub async fn run_ffprobe(
    quarantine_path: &Path,
    policy: &FfprobePolicy,
) -> anyhow::Result<MediaProbe> {
    policy.validate()?;
    let input_metadata = tokio::fs::symlink_metadata(quarantine_path)
        .await
        .with_context(|| {
            format!(
                "inspect Video Manager FFprobe input {}",
                quarantine_path.display()
            )
        })?;
    if !input_metadata.file_type().is_file() || input_metadata.len() == 0 {
        anyhow::bail!("Video Manager FFprobe input must be a non-empty regular file");
    }

    let mut command = Command::new(&policy.executable);
    command
        .arg("-v")
        .arg("error")
        .arg("-hide_banner")
        .arg("-protocol_whitelist")
        .arg("file,crypto,data")
        .arg("-probesize")
        .arg("5000000")
        .arg("-analyzeduration")
        .arg("10000000")
        .arg("-show_entries")
        .arg("format=format_name,duration,size,bit_rate:stream=index,codec_type,codec_name,width,height,pix_fmt,r_frame_rate,avg_frame_rate,bit_rate,duration")
        .arg("-of")
        .arg("json")
        .arg(quarantine_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn configured FFprobe {}", policy.executable.display()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Video Manager FFprobe stdout pipe is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("Video Manager FFprobe stderr pipe is unavailable"))?;
    let stdout_cap = policy.max_output_bytes;
    let stderr_cap = policy.max_output_bytes.min(256 * 1024);
    let stdout_task = tokio::spawn(read_bounded_output(stdout, stdout_cap));
    let stderr_task = tokio::spawn(read_bounded_output(stderr, stderr_cap));

    let status = match tokio::time::timeout(policy.timeout, child.wait()).await {
        Ok(result) => result.context("wait for Video Manager FFprobe")?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            anyhow::bail!(
                "Video Manager FFprobe exceeded timeout of {:?}",
                policy.timeout
            );
        }
    };

    let stdout = stdout_task
        .await
        .context("join Video Manager FFprobe stdout reader")??;
    let stderr = stderr_task
        .await
        .context("join Video Manager FFprobe stderr reader")??;
    if !status.success() {
        let detail = bounded_error_text(&stderr);
        anyhow::bail!("Video Manager FFprobe exited with {status}: {detail}");
    }

    let raw: RawProbe = serde_json::from_slice(&stdout)
        .context("parse Video Manager FFprobe JSON output")?;
    build_media_probe(raw)
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
            .context("read Video Manager FFprobe pipe")?;
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
        anyhow::bail!("Video Manager FFprobe output exceeded {max_bytes} bytes");
    }
    Ok(output)
}

fn bounded_error_text(bytes: &[u8]) -> String {
    let end = bytes.len().min(ERROR_TEXT_BYTES);
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[derive(Debug, Deserialize)]
struct RawProbe {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: Option<RawFormat>,
}

#[derive(Debug, Deserialize)]
struct RawStream {
    index: Option<u32>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    bit_rate: Option<String>,
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    format_name: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
}

fn build_media_probe(raw: RawProbe) -> anyhow::Result<MediaProbe> {
    let audio_streams = raw
        .streams
        .iter()
        .filter(|stream| stream.codec_type.as_deref() == Some("audio"))
        .count();
    let mut video_streams = Vec::new();
    for stream in raw
        .streams
        .into_iter()
        .filter(|stream| stream.codec_type.as_deref() == Some("video"))
    {
        let codec = stream
            .codec_name
            .filter(|codec| !codec.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Video Manager FFprobe video stream has no codec"))?;
        let width = stream
            .width
            .filter(|value| *value > 0 && *value <= MAX_VIDEO_DIMENSION)
            .ok_or_else(|| anyhow::anyhow!("Video Manager FFprobe reported invalid video width"))?;
        let height = stream
            .height
            .filter(|value| *value > 0 && *value <= MAX_VIDEO_DIMENSION)
            .ok_or_else(|| anyhow::anyhow!("Video Manager FFprobe reported invalid video height"))?;
        let frame_rate = stream
            .avg_frame_rate
            .as_deref()
            .and_then(parse_ratio)
            .or_else(|| stream.r_frame_rate.as_deref().and_then(parse_ratio));
        if frame_rate.is_some_and(|rate| !rate.is_finite() || rate <= 0.0 || rate > MAX_VIDEO_FRAME_RATE)
        {
            anyhow::bail!("Video Manager FFprobe reported invalid video frame rate");
        }
        video_streams.push(VideoStreamProbe {
            index: stream.index.unwrap_or(video_streams.len() as u32),
            codec,
            width,
            height,
            pixel_format: stream.pix_fmt.filter(|value| !value.trim().is_empty()),
            frame_rate,
            bit_rate: parse_u64(stream.bit_rate.as_deref()),
            duration_secs: parse_nonnegative_f64(stream.duration.as_deref()),
        });
    }
    if video_streams.is_empty() {
        anyhow::bail!("Video Manager FFprobe found no video streams");
    }

    let format = raw.format.unwrap_or(RawFormat {
        format_name: None,
        duration: None,
        bit_rate: None,
    });
    let format_names = format
        .format_name
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();

    Ok(MediaProbe {
        format_names,
        duration_secs: parse_nonnegative_f64(format.duration.as_deref()),
        bit_rate: parse_u64(format.bit_rate.as_deref()),
        video_streams,
        audio_streams,
    })
}

fn parse_u64(value: Option<&str>) -> Option<u64> {
    value?.parse().ok()
}

fn parse_nonnegative_f64(value: Option<&str>) -> Option<f64> {
    let value = value?.parse::<f64>().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn parse_ratio(value: &str) -> Option<f64> {
    let (numerator, denominator) = value.split_once('/')?;
    let numerator = numerator.parse::<f64>().ok()?;
    let denominator = denominator.parse::<f64>().ok()?;
    if !numerator.is_finite() || !denominator.is_finite() || denominator == 0.0 {
        return None;
    }
    Some(numerator / denominator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typed_video_probe_output() {
        let raw: RawProbe = serde_json::from_value(serde_json::json!({
            "streams": [
                {
                    "index": 0,
                    "codec_type": "video",
                    "codec_name": "h264",
                    "width": 1920,
                    "height": 1080,
                    "pix_fmt": "yuv420p",
                    "avg_frame_rate": "30000/1001",
                    "bit_rate": "4000000",
                    "duration": "12.5"
                },
                {"index": 1, "codec_type": "audio", "codec_name": "aac"}
            ],
            "format": {
                "format_name": "mov,mp4,m4a,3gp,3g2,mj2",
                "duration": "12.5",
                "bit_rate": "4200000"
            }
        }))
        .unwrap();
        let probe = build_media_probe(raw).unwrap();
        assert_eq!(probe.video_streams.len(), 1);
        assert_eq!(probe.audio_streams, 1);
        assert_eq!(probe.video_streams[0].codec, "h264");
        assert_eq!(probe.video_streams[0].width, 1920);
        assert!(probe.video_streams[0]
            .frame_rate
            .is_some_and(|rate| (rate - 29.97).abs() < 0.01));
    }

    #[test]
    fn rejects_probe_output_without_video_or_with_absurd_dimensions() {
        let audio_only: RawProbe = serde_json::from_value(serde_json::json!({
            "streams": [{"index": 0, "codec_type": "audio", "codec_name": "aac"}],
            "format": {"format_name": "mp4"}
        }))
        .unwrap();
        assert!(build_media_probe(audio_only).is_err());

        let absurd: RawProbe = serde_json::from_value(serde_json::json!({
            "streams": [{
                "index": 0,
                "codec_type": "video",
                "codec_name": "h264",
                "width": 100000,
                "height": 1080,
                "avg_frame_rate": "30/1"
            }],
            "format": {"format_name": "mp4"}
        }))
        .unwrap();
        assert!(build_media_probe(absurd).is_err());
    }

    #[test]
    fn output_reader_caps_memory_while_draining() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let input = vec![b'x'; 1024];
            let result = read_bounded_output(input.as_slice(), 128).await;
            assert!(result.is_err());
        });
    }
}
