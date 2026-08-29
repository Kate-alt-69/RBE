from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


path = Path("engine/crates/video-manager/src/lib.rs")
source = path.read_text()
source = replace_once(
    source,
    "mod ffmpeg;\nmod ffprobe;\n",
    "mod ffmpeg;\nmod ffmpeg_capabilities;\nmod ffprobe;\n",
    "Video Manager FFmpeg module anchor",
)
source = replace_once(
    source,
    "pub use ffmpeg::{FfmpegPolicy, NormalizedMedia};\n",
    "pub use ffmpeg::{FfmpegPolicy, NormalizedMedia};\npub use ffmpeg_capabilities::{probe_ffmpeg_capabilities, FfmpegCapabilities};\n",
    "Video Manager FFmpeg public export anchor",
)
path.write_text(source)

path = Path("engine/crates/backend/src/main.rs")
source = path.read_text()
old = '''            let download = video_manager::DownloadPolicy {
                max_bytes: config.video_manager.download_max_bytes,
                ..Default::default()
            };
            let policy = video_manager::VideoWorkerPolicy {
                download,
                ffprobe: video_manager::FfprobePolicy::new(&ffprobe),
                ffmpeg: video_manager::FfmpegPolicy::new(&ffmpeg),
                recovery_scan: Duration::from_secs(config.video_manager.worker_recovery_scan_secs),
            };
            let task = manager.clone().spawn_download_worker(policy)?;
            tracing::info!(
                ffprobe = %ffprobe.display(),
                ffmpeg = %ffmpeg.display(),
                recovery_scan_secs = config.video_manager.worker_recovery_scan_secs,
                max_download_bytes = config.video_manager.download_max_bytes,
                "Video Manager lazy download worker ready"
            );
'''
new = '''            let download = video_manager::DownloadPolicy {
                max_bytes: config.video_manager.download_max_bytes,
                ..Default::default()
            };
            let ffmpeg_policy = video_manager::FfmpegPolicy::new(&ffmpeg);
            let ffmpeg_capabilities =
                video_manager::probe_ffmpeg_capabilities(&ffmpeg_policy).await?;
            let policy = video_manager::VideoWorkerPolicy {
                download,
                ffprobe: video_manager::FfprobePolicy::new(&ffprobe),
                ffmpeg: ffmpeg_policy,
                recovery_scan: Duration::from_secs(config.video_manager.worker_recovery_scan_secs),
            };
            let task = manager.clone().spawn_download_worker(policy)?;
            tracing::info!(
                ffprobe = %ffprobe.display(),
                ffmpeg = %ffmpeg.display(),
                software_h264 = ffmpeg_capabilities.software_h264,
                aac = ffmpeg_capabilities.aac,
                hardware_h264_encoders = ?ffmpeg_capabilities.hardware_h264_encoders,
                recovery_scan_secs = config.video_manager.worker_recovery_scan_secs,
                max_download_bytes = config.video_manager.download_max_bytes,
                "Video Manager lazy download worker ready"
            );
'''
source = replace_once(source, old, new, "backend FFmpeg worker policy anchor")
path.write_text(source)
