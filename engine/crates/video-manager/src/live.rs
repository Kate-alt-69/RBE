use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{VideoAssetState, VideoManager, VideoSourceType};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoLiveSessionState {
    Reserved,
    Starting,
    Live,
    Stopping,
    Ended,
    Failed,
}

impl VideoLiveSessionState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Starting => "starting",
            Self::Live => "live",
            Self::Stopping => "stopping",
            Self::Ended => "ended",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "starting" => Ok(Self::Starting),
            "live" => Ok(Self::Live),
            "stopping" => Ok(Self::Stopping),
            "ended" => Ok(Self::Ended),
            "failed" => Ok(Self::Failed),
            other => anyhow::bail!("Video Manager stored invalid live session state {other:?}"),
        }
    }

    pub fn terminal(self) -> bool {
        matches!(self, Self::Ended | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoLiveSession {
    pub id: String,
    pub asset_id: String,
    pub database: String,
    pub state: VideoLiveSessionState,
    pub ingest_protocol: Option<String>,
    pub ingest_endpoint: Option<String>,
    pub playback_endpoint: Option<String>,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ReserveLiveSessionRequest {
    pub database: Option<String>,
    pub asset_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoLiveIngestProtocol {
    Rtmp,
    Whip,
}

impl VideoLiveIngestProtocol {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Rtmp => "rtmp",
            Self::Whip => "whip",
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoLiveBinding {
    pub ingest_protocol: VideoLiveIngestProtocol,
    pub ingest_endpoint: String,
    pub playback_endpoint: Option<String>,
}

fn validate_endpoint(label: &str, value: &str, allowed_schemes: &[&str]) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(value)
        .map_err(|error| anyhow::anyhow!("Video Manager {label} URL is invalid: {error}"))?;
    if parsed.host_str().is_none() {
        anyhow::bail!("Video Manager {label} URL must include a host");
    }
    if !allowed_schemes
        .iter()
        .any(|scheme| *scheme == parsed.scheme())
    {
        anyhow::bail!(
            "Video Manager {label} URL scheme {:?} is not allowed",
            parsed.scheme()
        );
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        anyhow::bail!("Video Manager {label} URL must not contain credentials or a fragment");
    }
    Ok(())
}

fn validate_live_binding(binding: &VideoLiveBinding) -> anyhow::Result<()> {
    match binding.ingest_protocol {
        VideoLiveIngestProtocol::Rtmp => {
            validate_endpoint("RTMP ingest", &binding.ingest_endpoint, &["rtmp", "rtmps"])?;
        }
        VideoLiveIngestProtocol::Whip => {
            validate_endpoint("WHIP ingest", &binding.ingest_endpoint, &["https"])?;
        }
    }
    if let Some(playback) = &binding.playback_endpoint {
        validate_endpoint("live playback", playback, &["https"])?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoLiveSessionCounts {
    pub reserved: u64,
    pub starting: u64,
    pub live: u64,
    pub stopping: u64,
}

impl VideoLiveSessionCounts {
    pub(crate) fn checked_add(self, other: Self) -> anyhow::Result<Self> {
        Ok(Self {
            reserved: self
                .reserved
                .checked_add(other.reserved)
                .ok_or_else(|| anyhow::anyhow!("Video Manager reserved live count overflowed"))?,
            starting: self
                .starting
                .checked_add(other.starting)
                .ok_or_else(|| anyhow::anyhow!("Video Manager starting live count overflowed"))?,
            live: self
                .live
                .checked_add(other.live)
                .ok_or_else(|| anyhow::anyhow!("Video Manager active live count overflowed"))?,
            stopping: self
                .stopping
                .checked_add(other.stopping)
                .ok_or_else(|| anyhow::anyhow!("Video Manager stopping live count overflowed"))?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoLiveRuntimeState {
    Disabled,
    Sleeping,
    Starting,
    Active,
    Draining,
    Degraded,
}

impl VideoLiveRuntimeState {
    pub fn healthy(self) -> bool {
        self != Self::Degraded
    }
}

pub(crate) fn validate_live_transition(
    from: VideoLiveSessionState,
    to: VideoLiveSessionState,
) -> anyhow::Result<()> {
    let allowed = matches!(
        (from, to),
        (
            VideoLiveSessionState::Reserved,
            VideoLiveSessionState::Starting
        ) | (
            VideoLiveSessionState::Reserved,
            VideoLiveSessionState::Ended
        ) | (
            VideoLiveSessionState::Reserved,
            VideoLiveSessionState::Failed
        ) | (VideoLiveSessionState::Starting, VideoLiveSessionState::Live)
            | (
                VideoLiveSessionState::Starting,
                VideoLiveSessionState::Stopping
            )
            | (
                VideoLiveSessionState::Starting,
                VideoLiveSessionState::Failed
            )
            | (VideoLiveSessionState::Live, VideoLiveSessionState::Stopping)
            | (VideoLiveSessionState::Live, VideoLiveSessionState::Failed)
            | (
                VideoLiveSessionState::Stopping,
                VideoLiveSessionState::Ended
            )
            | (
                VideoLiveSessionState::Stopping,
                VideoLiveSessionState::Failed
            )
    );
    if allowed {
        Ok(())
    } else {
        anyhow::bail!(
            "Video Manager live session transition {:?} -> {:?} is not allowed",
            from,
            to
        )
    }
}

impl VideoManager {
    pub fn reserve_live_session(
        &self,
        request: ReserveLiveSessionRequest,
    ) -> anyhow::Result<VideoLiveSession> {
        crate::validate_generated_uuid("live asset id", &request.asset_id)?;
        let (database_name, database) = self.resolve_database(request.database.as_deref())?;
        let asset = database
            .get_asset(&database_name, &request.asset_id)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Video Manager live asset {:?} does not exist",
                    request.asset_id
                )
            })?;
        if asset.source_type != VideoSourceType::Live {
            anyhow::bail!("Video Manager live session requires a live source asset");
        }
        if asset.state != VideoAssetState::Reserved {
            anyhow::bail!("Video Manager live asset must be reserved before session reservation");
        }
        let session = VideoLiveSession {
            id: Uuid::new_v4().to_string(),
            asset_id: asset.id,
            database: database_name.clone(),
            state: VideoLiveSessionState::Reserved,
            ingest_protocol: None,
            ingest_endpoint: None,
            playback_endpoint: None,
            started_at_ms: None,
            ended_at_ms: None,
        };
        database.insert_live_session(&database_name, &session)?;
        self.live_notify.notify_waiters();
        Ok(session)
    }

    pub fn get_live_session(
        &self,
        database: Option<&str>,
        session_id: &str,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        crate::validate_generated_uuid("live session id", session_id)?;
        let (database_name, database) = self.resolve_database(database)?;
        database.get_live_session(&database_name, session_id)
    }

    /// Request that a live reservation/session end. Reserved sessions can end
    /// immediately because no media runtime owns them yet. Starting/live
    /// sessions move to `stopping`; the trusted media runtime is responsible
    /// for the final `stopping -> ended` transition after draining resources.
    pub fn request_end_live_session(
        &self,
        database: Option<&str>,
        session_id: &str,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        let current = match self.get_live_session(database, session_id)? {
            Some(session) => session,
            None => return Ok(None),
        };
        match current.state {
            VideoLiveSessionState::Reserved => self.transition_live_session_trusted(
                Some(&current.database),
                session_id,
                VideoLiveSessionState::Reserved,
                VideoLiveSessionState::Ended,
            ),
            VideoLiveSessionState::Starting => self.transition_live_session_trusted(
                Some(&current.database),
                session_id,
                VideoLiveSessionState::Starting,
                VideoLiveSessionState::Stopping,
            ),
            VideoLiveSessionState::Live => self.transition_live_session_trusted(
                Some(&current.database),
                session_id,
                VideoLiveSessionState::Live,
                VideoLiveSessionState::Stopping,
            ),
            VideoLiveSessionState::Stopping
            | VideoLiveSessionState::Ended
            | VideoLiveSessionState::Failed => Ok(Some(current)),
        }
    }

    /// Trusted media-runtime binding. Language code never receives this API and
    /// therefore cannot forge ingest/playback endpoints or self-promote a
    /// reservation into an active transport.
    pub fn bind_live_session_trusted(
        &self,
        database: Option<&str>,
        session_id: &str,
        binding: VideoLiveBinding,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        crate::validate_generated_uuid("live session id", session_id)?;
        validate_live_binding(&binding)?;
        let (database_name, database) = self.resolve_database(database)?;
        let result = database.bind_live_session(&database_name, session_id, &binding)?;
        if result.is_some() {
            self.live_notify.notify_waiters();
        }
        Ok(result)
    }

    pub fn mark_live_session_ready_trusted(
        &self,
        database: Option<&str>,
        session_id: &str,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        self.transition_live_session_trusted(
            database,
            session_id,
            VideoLiveSessionState::Starting,
            VideoLiveSessionState::Live,
        )
    }

    pub fn mark_live_session_failed_trusted(
        &self,
        database: Option<&str>,
        session_id: &str,
        expected: VideoLiveSessionState,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        self.transition_live_session_trusted(
            database,
            session_id,
            expected,
            VideoLiveSessionState::Failed,
        )
    }

    /// Trusted Rust-only lifecycle transition used by the future ingest/live
    /// runtime. This method is deliberately not exposed as a language function.
    pub fn transition_live_session_trusted(
        &self,
        database: Option<&str>,
        session_id: &str,
        expected: VideoLiveSessionState,
        next: VideoLiveSessionState,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        crate::validate_generated_uuid("live session id", session_id)?;
        validate_live_transition(expected, next)?;
        let (database_name, database) = self.resolve_database(database)?;
        let result =
            database.transition_live_session(&database_name, session_id, expected, next)?;
        if result.is_some() {
            self.live_notify.notify_waiters();
        }
        Ok(result)
    }

    pub fn live_session_counts(&self) -> anyhow::Result<VideoLiveSessionCounts> {
        let mut counts = VideoLiveSessionCounts::default();
        for name in self.database_names()? {
            let (_, database) = self.resolve_database(Some(&name))?;
            counts = counts.checked_add(database.live_session_counts()?)?;
        }
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_state_machine_is_fail_closed() {
        assert!(validate_live_transition(
            VideoLiveSessionState::Reserved,
            VideoLiveSessionState::Starting
        )
        .is_ok());
        assert!(validate_live_transition(
            VideoLiveSessionState::Starting,
            VideoLiveSessionState::Live
        )
        .is_ok());
        assert!(validate_live_transition(
            VideoLiveSessionState::Live,
            VideoLiveSessionState::Stopping
        )
        .is_ok());
        assert!(validate_live_transition(
            VideoLiveSessionState::Stopping,
            VideoLiveSessionState::Ended
        )
        .is_ok());
        assert!(validate_live_transition(
            VideoLiveSessionState::Reserved,
            VideoLiveSessionState::Live
        )
        .is_err());
        assert!(validate_live_transition(
            VideoLiveSessionState::Ended,
            VideoLiveSessionState::Starting
        )
        .is_err());
    }

    #[test]
    fn trusted_live_binding_rejects_wrong_schemes_and_url_credentials() {
        assert!(validate_live_binding(&VideoLiveBinding {
            ingest_protocol: VideoLiveIngestProtocol::Rtmp,
            ingest_endpoint: "https://example.com/live/key".into(),
            playback_endpoint: None,
        })
        .is_err());
        assert!(validate_live_binding(&VideoLiveBinding {
            ingest_protocol: VideoLiveIngestProtocol::Whip,
            ingest_endpoint: "https://user:pass@example.com/whip".into(),
            playback_endpoint: None,
        })
        .is_err());
        assert!(validate_live_binding(&VideoLiveBinding {
            ingest_protocol: VideoLiveIngestProtocol::Rtmp,
            ingest_endpoint: "rtmps://ingest.example.com/live/key".into(),
            playback_endpoint: Some("https://cdn.example.com/live/index.m3u8".into()),
        })
        .is_ok());
    }
}
