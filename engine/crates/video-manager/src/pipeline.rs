use crate::{
    DownloadPolicy, FfmpegPolicy, FfprobePolicy, QueuedDownload, VideoManager, VideoVariant,
};

impl VideoManager {
    /// Drive one already-created download job through the complete trusted
    /// quarantine pipeline. Scheduling/queue discovery stays outside this
    /// method so the mother process can decide when workers should be active.
    pub async fn process_queued_download(
        &self,
        queued: &QueuedDownload,
        download_policy: DownloadPolicy,
        ffprobe_policy: &FfprobePolicy,
        ffmpeg_policy: &FfmpegPolicy,
    ) -> anyhow::Result<VideoVariant> {
        self.run_queued_download(queued, download_policy).await?;
        self.inspect_download_container(queued).await?;
        self.probe_download_media(queued, ffprobe_policy).await?;
        self.normalize_download_media(queued, ffmpeg_policy).await
    }
}
