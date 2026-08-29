from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


# --- ffmpeg.rs: internal encoder enum + deterministic hardware/software fallback.
path = Path("crates/video-manager/src/ffmpeg.rs")
source = path.read_text()
source = replace_once(
    source,
    "#[derive(Debug, Clone)]\npub struct FfmpegPolicy {\n    pub executable: PathBuf,\n    pub timeout: Duration,\n    pub max_log_bytes: usize,\n}\n",
    '''#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FfmpegVideoEncoder {
    Software,
    NvidiaNvenc,
    IntelQsv,
    AmdAmf,
    AppleVideoToolbox,
    WindowsMediaFoundation,
}

impl FfmpegVideoEncoder {
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Software => "libx264",
            Self::NvidiaNvenc => "h264_nvenc",
            Self::IntelQsv => "h264_qsv",
            Self::AmdAmf => "h264_amf",
            Self::AppleVideoToolbox => "h264_videotoolbox",
            Self::WindowsMediaFoundation => "h264_mf",
        }
    }

    pub fn is_hardware(self) -> bool {
        self != Self::Software
    }
}

#[derive(Debug, Clone)]
pub struct FfmpegPolicy {
    pub executable: PathBuf,
    pub timeout: Duration,
    pub max_log_bytes: usize,
    pub video_encoder: FfmpegVideoEncoder,
}
''',
    "FFmpeg policy declaration",
)
source = replace_once(
    source,
    '''            executable: executable.into(),
            timeout: Duration::from_secs(15 * 60),
            max_log_bytes: 1024 * 1024,
        }
    }
''',
    '''            executable: executable.into(),
            timeout: Duration::from_secs(15 * 60),
            max_log_bytes: 1024 * 1024,
            video_encoder: FfmpegVideoEncoder::Software,
        }
    }

    pub fn with_video_encoder(mut self, video_encoder: FfmpegVideoEncoder) -> Self {
        self.video_encoder = video_encoder;
        self
    }
''',
    "FFmpeg policy constructor",
)
source = replace_once(
    source,
    '''pub struct NormalizedMedia {
    pub profile: &'static str,
    pub container: &'static str,
    pub video_codec: &'static str,
    pub audio_codec: &'static str,
    pub size_bytes: u64,
}
''',
    '''pub struct NormalizedMedia {
    pub profile: &'static str,
    pub container: &'static str,
    pub video_codec: &'static str,
    pub video_encoder: FfmpegVideoEncoder,
    pub audio_codec: &'static str,
    pub size_bytes: u64,
}
''',
    "NormalizedMedia model",
)
start = source.index("pub async fn run_ffmpeg_normalize(")
end = source.index("\nasync fn validate_paths", start)
new_fn = r'''pub async fn run_ffmpeg_normalize(
    input_path: &Path,
    output_path: &Path,
    policy: &FfmpegPolicy,
) -> anyhow::Result<NormalizedMedia> {
    policy.validate()?;
    validate_paths(input_path, output_path).await?;

    let selected = policy.video_encoder;
    match run_ffmpeg_once(input_path, output_path, policy, selected).await {
        Ok(media) => Ok(media),
        Err(hardware_error) if selected.is_hardware() => {
            let _ = tokio::fs::remove_file(output_path).await;
            tracing::warn!(
                encoder = selected.ffmpeg_name(),
                error = %hardware_error,
                "Video Manager hardware normalization failed; retrying with libx264"
            );
            match run_ffmpeg_once(
                input_path,
                output_path,
                policy,
                FfmpegVideoEncoder::Software,
            )
            .await
            {
                Ok(media) => Ok(media),
                Err(software_error) => Err(anyhow::anyhow!(
                    "Video Manager hardware normalization with {} failed: {hardware_error}; software fallback failed: {software_error}",
                    selected.ffmpeg_name()
                )),
            }
        }
        Err(error) => Err(error),
    }
}

async fn run_ffmpeg_once(
    input_path: &Path,
    output_path: &Path,
    policy: &FfmpegPolicy,
    encoder: FfmpegVideoEncoder,
) -> anyhow::Result<NormalizedMedia> {
    let args = normalization_args(input_path, output_path, encoder);
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
        anyhow::bail!(
            "Video Manager FFmpeg encoder {} exited with {status}: {detail}",
            encoder.ffmpeg_name()
        );
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
        video_encoder: encoder,
        audio_codec: "aac",
        size_bytes: metadata.len(),
    })
}
'''
source = source[:start] + new_fn + source[end:]
start = source.index("fn normalization_args(")
end = source.index("\nasync fn read_bounded_output", start)
new_args = r'''fn normalization_args(
    input_path: &Path,
    output_path: &Path,
    encoder: FfmpegVideoEncoder,
) -> Vec<OsString> {
    let mut args = vec![
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
        OsString::from(encoder.ffmpeg_name()),
    ];
    if encoder == FfmpegVideoEncoder::Software {
        args.extend([
            OsString::from("-preset"),
            OsString::from("medium"),
            OsString::from("-crf"),
            OsString::from("23"),
        ]);
    } else {
        // Generic AVCodec rate-control options are used intentionally instead
        // of exposing vendor-specific flags to callers.
        args.extend([
            OsString::from("-b:v"),
            OsString::from("5M"),
            OsString::from("-maxrate"),
            OsString::from("6M"),
            OsString::from("-bufsize"),
            OsString::from("10M"),
        ]);
    }
    args.extend([
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
    ]);
    args
}
'''
source = source[:start] + new_args + source[end:]
source = source.replace(
    'normalization_args(Path::new("/input.part"), Path::new("/output.mp4"))',
    'normalization_args(\n            Path::new("/input.part"),\n            Path::new("/output.mp4"),\n            FfmpegVideoEncoder::Software,\n        )',
    1,
)
test_anchor = "    #[test]\n    fn policy_rejects_relative_executable_before_spawn() {"
hw_test = r'''    #[test]
    fn hardware_profile_uses_only_fixed_internal_flags() {
        let args = normalization_args(
            Path::new("/input.part"),
            Path::new("/output.mp4"),
            FfmpegVideoEncoder::IntelQsv,
        );
        let rendered = args
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(rendered.windows(2).any(|pair| pair == ["-c:v", "h264_qsv"]));
        assert!(rendered.windows(2).any(|pair| pair == ["-b:v", "5M"]));
        assert!(!rendered.iter().any(|value| value == "-crf"));
        assert!(!rendered.iter().any(|value| value == "-preset"));
    }

    #[test]
    fn policy_rejects_relative_executable_before_spawn() {'''
source = replace_once(source, test_anchor, hw_test, "FFmpeg test insertion")
path.write_text(source)

# --- ffmpeg_capabilities.rs: translate verified names into a typed preferred encoder.
path = Path("crates/video-manager/src/ffmpeg_capabilities.rs")
source = path.read_text()
source = replace_once(
    source,
    "use crate::FfmpegPolicy;",
    "use crate::{FfmpegPolicy, FfmpegVideoEncoder};",
    "capability import",
)
impl_anchor = '''pub struct FfmpegCapabilities {
    pub software_h264: bool,
    pub aac: bool,
    pub hardware_h264_encoders: Vec<String>,
    pub verified_hardware_h264_encoders: Vec<String>,
}
'''
impl_replacement = impl_anchor + r'''
impl FfmpegCapabilities {
    pub fn preferred_video_encoder(&self) -> FfmpegVideoEncoder {
        for name in &self.verified_hardware_h264_encoders {
            if let Some(encoder) = typed_hardware_encoder(name) {
                return encoder;
            }
        }
        FfmpegVideoEncoder::Software
    }
}

fn typed_hardware_encoder(name: &str) -> Option<FfmpegVideoEncoder> {
    match name {
        "h264_nvenc" => Some(FfmpegVideoEncoder::NvidiaNvenc),
        "h264_qsv" => Some(FfmpegVideoEncoder::IntelQsv),
        "h264_amf" => Some(FfmpegVideoEncoder::AmdAmf),
        "h264_videotoolbox" => Some(FfmpegVideoEncoder::AppleVideoToolbox),
        "h264_mf" => Some(FfmpegVideoEncoder::WindowsMediaFoundation),
        _ => None,
    }
}
'''
source = replace_once(source, impl_anchor, impl_replacement, "FfmpegCapabilities impl")
tests_end = source.rfind("\n}")
source = source[:tests_end] + r'''

    #[test]
    fn prefers_first_verified_typed_hardware_encoder_and_falls_back_to_software() {
        let capabilities = FfmpegCapabilities {
            software_h264: true,
            aac: true,
            hardware_h264_encoders: vec!["h264_nvenc".into(), "h264_qsv".into()],
            verified_hardware_h264_encoders: vec!["h264_qsv".into()],
        };
        assert_eq!(
            capabilities.preferred_video_encoder(),
            FfmpegVideoEncoder::IntelQsv
        );
        let software = FfmpegCapabilities {
            verified_hardware_h264_encoders: Vec::new(),
            ..capabilities
        };
        assert_eq!(
            software.preferred_video_encoder(),
            FfmpegVideoEncoder::Software
        );
    }
''' + source[tests_end:]
path.write_text(source)

# --- pipeline + normalization: carry trusted probe metadata into the ready variant.
path = Path("crates/video-manager/src/pipeline.rs")
source = path.read_text()
source = replace_once(
    source,
    '''        self.inspect_download_container(queued).await?;
        self.probe_download_media(queued, ffprobe_policy).await?;
        self.normalize_download_media(queued, ffmpeg_policy).await
''',
    '''        self.inspect_download_container(queued).await?;
        let probe = self.probe_download_media(queued, ffprobe_policy).await?;
        self.normalize_download_media(queued, &probe, ffmpeg_policy)
            .await
''',
    "pipeline probe handoff",
)
path.write_text(source)

path = Path("crates/video-manager/src/normalization.rs")
source = path.read_text()
source = replace_once(
    source,
    '''use crate::{
    FfmpegPolicy, QueuedDownload, VideoManager, VideoVariant, PROGRESS_NORMALIZING, PROGRESS_PROBED,
};
''',
    '''use crate::{
    FfmpegPolicy, MediaProbe, QueuedDownload, VideoManager, VideoVariant, PROGRESS_NORMALIZING,
    PROGRESS_PROBED,
};
''',
    "normalization imports",
)
source = replace_once(
    source,
    '''        &self,
        queued: &QueuedDownload,
        policy: &FfmpegPolicy,
''',
    '''        &self,
        queued: &QueuedDownload,
        probe: &MediaProbe,
        policy: &FfmpegPolicy,
''',
    "normalization signature",
)
variant_old = '''        let now = crate::now_ms();
        let variant = VideoVariant {
            id: Uuid::new_v4().to_string(),
            asset_id: transitioned.asset_id.clone(),
            profile: normalized.profile.to_string(),
            codec: Some(normalized.video_codec.to_string()),
            width: None,
            height: None,
            fps: None,
            bitrate: None,
            size_bytes: normalized.size_bytes,
'''
variant_new = '''        let stream = probe.video_streams.first().ok_or_else(|| {
            anyhow::anyhow!("Video Manager normalization probe contains no video stream")
        })?;
        let output_bitrate = probe.duration_secs.and_then(|duration| {
            if duration.is_finite() && duration > 0.0 {
                let bits_per_second = (normalized.size_bytes as f64 * 8.0) / duration;
                (bits_per_second.is_finite() && bits_per_second > 0.0)
                    .then(|| bits_per_second.round() as u64)
            } else {
                None
            }
        });
        let now = crate::now_ms();
        let variant = VideoVariant {
            id: Uuid::new_v4().to_string(),
            asset_id: transitioned.asset_id.clone(),
            profile: normalized.profile.to_string(),
            codec: Some(normalized.video_codec.to_string()),
            width: Some(stream.width),
            height: Some(stream.height),
            fps: stream.frame_rate,
            bitrate: output_bitrate,
            size_bytes: normalized.size_bytes,
'''
source = replace_once(source, variant_old, variant_new, "variant probe metadata")
path.write_text(source)

# --- lib.rs/worker.rs: expose selected encoder in worker status.
path = Path("crates/video-manager/src/lib.rs")
source = path.read_text()
source = replace_once(
    source,
    "pub use ffmpeg::{FfmpegPolicy, NormalizedMedia};",
    "pub use ffmpeg::{FfmpegPolicy, FfmpegVideoEncoder, NormalizedMedia};",
    "FFmpeg public exports",
)
source = replace_once(
    source,
    '''pub struct VideoDownloadWorkerStatus {
    pub state: VideoWorkerState,
    pub queued_downloads: u64,
}
''',
    '''pub struct VideoDownloadWorkerStatus {
    pub state: VideoWorkerState,
    pub queued_downloads: u64,
    pub video_encoder: Option<FfmpegVideoEncoder>,
}
''',
    "worker status model",
)
source = replace_once(
    source,
    '''    work_notify: tokio::sync::Notify,
    worker_state: Mutex<VideoWorkerState>,
    live_idle_secs: u64,
''',
    '''    work_notify: tokio::sync::Notify,
    worker_state: Mutex<VideoWorkerState>,
    worker_encoder: Mutex<Option<FfmpegVideoEncoder>>,
    live_idle_secs: u64,
''',
    "VideoManager worker fields",
)
source = replace_once(
    source,
    '''            work_notify: tokio::sync::Notify::new(),
            worker_state: Mutex::new(VideoWorkerState::Disabled),
            live_idle_secs,
''',
    '''            work_notify: tokio::sync::Notify::new(),
            worker_state: Mutex::new(VideoWorkerState::Disabled),
            worker_encoder: Mutex::new(None),
            live_idle_secs,
''',
    "VideoManager worker initializer",
)
worker_state_anchor = '''    fn worker_state(&self) -> anyhow::Result<VideoWorkerState> {
        self.worker_state
            .lock()
            .map(|state| *state)
            .map_err(|_| anyhow::anyhow!("Video Manager worker state mutex is poisoned"))
    }
'''
worker_state_replacement = worker_state_anchor + '''
    fn set_worker_encoder(&self, encoder: Option<FfmpegVideoEncoder>) -> anyhow::Result<()> {
        let mut current = self
            .worker_encoder
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager worker encoder mutex is poisoned"))?;
        *current = encoder;
        Ok(())
    }

    fn worker_encoder(&self) -> anyhow::Result<Option<FfmpegVideoEncoder>> {
        self.worker_encoder
            .lock()
            .map(|encoder| *encoder)
            .map_err(|_| anyhow::anyhow!("Video Manager worker encoder mutex is poisoned"))
    }
'''
source = replace_once(source, worker_state_anchor, worker_state_replacement, "worker encoder methods")
source = replace_once(
    source,
    '''            download_worker: VideoDownloadWorkerStatus {
                state: worker_state,
                queued_downloads,
            },
''',
    '''            download_worker: VideoDownloadWorkerStatus {
                state: worker_state,
                queued_downloads,
                video_encoder: self.worker_encoder()?,
            },
''',
    "worker status output",
)
path.write_text(source)

path = Path("crates/video-manager/src/worker.rs")
source = path.read_text()
source = replace_once(
    source,
    '''        if let Err(error) = self.manager.set_worker_state(VideoWorkerState::Disabled) {
            tracing::error!(error = %error, "Video Manager worker shutdown telemetry failed");
        }
''',
    '''        if let Err(error) = self.manager.set_worker_state(VideoWorkerState::Disabled) {
            tracing::error!(error = %error, "Video Manager worker shutdown telemetry failed");
        }
        if let Err(error) = self.manager.set_worker_encoder(None) {
            tracing::error!(error = %error, "Video Manager worker encoder cleanup failed");
        }
''',
    "worker shutdown encoder cleanup",
)
source = replace_once(
    source,
    '''            *state = VideoWorkerState::Sleeping;
        }

        let manager = self.clone();
''',
    '''            *state = VideoWorkerState::Sleeping;
        }
        self.set_worker_encoder(Some(policy.ffmpeg.video_encoder))?;

        let manager = self.clone();
''',
    "worker selected encoder",
)
source = replace_once(
    source,
    '''            if let Err(error) = self.set_worker_state(VideoWorkerState::Disabled) {
                tracing::error!(error = %error, "Video Manager worker exit telemetry failed");
            }
''',
    '''            if let Err(error) = self.set_worker_state(VideoWorkerState::Disabled) {
                tracing::error!(error = %error, "Video Manager worker exit telemetry failed");
            }
            if let Err(error) = self.set_worker_encoder(None) {
                tracing::error!(error = %error, "Video Manager worker encoder exit cleanup failed");
            }
''',
    "worker task encoder cleanup",
)
source = replace_once(
    source,
    '''            worker_state: Mutex::new(VideoWorkerState::Disabled),
            live_idle_secs: 7200,
''',
    '''            worker_state: Mutex::new(VideoWorkerState::Disabled),
            worker_encoder: Mutex::new(None),
            live_idle_secs: 7200,
''',
    "worker test manager initializer",
)
path.write_text(source)

# --- backend: actually select the verified encoder.
path = Path("crates/backend/src/main.rs")
source = path.read_text()
old = '''            let ffmpeg_policy = video_manager::FfmpegPolicy::new(&ffmpeg);
            let ffmpeg_capabilities =
                video_manager::probe_ffmpeg_capabilities(&ffmpeg_policy).await?;
            let policy = video_manager::VideoWorkerPolicy {
                download,
                ffprobe: video_manager::FfprobePolicy::new(&ffprobe),
                ffmpeg: ffmpeg_policy,
'''
new = '''            let ffmpeg_policy = video_manager::FfmpegPolicy::new(&ffmpeg);
            let ffmpeg_capabilities =
                video_manager::probe_ffmpeg_capabilities(&ffmpeg_policy).await?;
            let selected_video_encoder = ffmpeg_capabilities.preferred_video_encoder();
            let ffmpeg_policy = ffmpeg_policy.with_video_encoder(selected_video_encoder);
            let policy = video_manager::VideoWorkerPolicy {
                download,
                ffprobe: video_manager::FfprobePolicy::new(&ffprobe),
                ffmpeg: ffmpeg_policy,
'''
source = replace_once(source, old, new, "backend encoder selection")
source = replace_once(
    source,
    '''                verified_hardware_h264_encoders = ?ffmpeg_capabilities.verified_hardware_h264_encoders,
                recovery_scan_secs = config.video_manager.worker_recovery_scan_secs,
''',
    '''                verified_hardware_h264_encoders = ?ffmpeg_capabilities.verified_hardware_h264_encoders,
                selected_video_encoder = ?selected_video_encoder,
                recovery_scan_secs = config.video_manager.worker_recovery_scan_secs,
''',
    "backend encoder log",
)
path.write_text(source)
