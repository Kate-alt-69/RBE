use std::path::Path;

use anyhow::Context;
use serde::Serialize;
use tokio::io::AsyncReadExt;

const MAX_SIGNATURE_BYTES: usize = 4096;
const MIN_BMFF_BOX_BYTES: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoContainerKind {
    IsoBmff,
    Matroska,
    Avi,
    MpegTransportStream,
    Flv,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerProbe {
    pub kind: VideoContainerKind,
    pub size_bytes: u64,
    pub brand: Option<String>,
}

/// Inspect only a bounded file prefix and fail closed on unknown signatures.
///
/// This is intentionally a cheap pre-FFprobe gate. It establishes that the
/// quarantine object resembles a supported media container; it does not prove
/// that the file contains a decodable video stream.
pub async fn probe_quarantine_container(path: &Path) -> anyhow::Result<ContainerProbe> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .with_context(|| format!("inspect Video Manager quarantine file {}", path.display()))?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("Video Manager quarantine object is not a regular file");
    }
    if metadata.len() == 0 {
        anyhow::bail!("Video Manager quarantine file is empty");
    }

    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open Video Manager quarantine file {}", path.display()))?;
    let mut prefix = vec![0u8; MAX_SIGNATURE_BYTES.min(metadata.len() as usize)];
    let mut read = 0usize;
    while read < prefix.len() {
        let count = file
            .read(&mut prefix[read..])
            .await
            .context("read Video Manager quarantine signature")?;
        if count == 0 {
            break;
        }
        read += count;
    }
    prefix.truncate(read);
    sniff_video_container(&prefix, metadata.len())
}

pub fn sniff_video_container(prefix: &[u8], size_bytes: u64) -> anyhow::Result<ContainerProbe> {
    if prefix.is_empty() || size_bytes == 0 {
        anyhow::bail!("Video Manager quarantine file is empty");
    }

    if let Some(brand) = sniff_iso_bmff(prefix) {
        return Ok(ContainerProbe {
            kind: VideoContainerKind::IsoBmff,
            size_bytes,
            brand: Some(brand),
        });
    }
    if sniff_matroska(prefix) {
        return Ok(ContainerProbe {
            kind: VideoContainerKind::Matroska,
            size_bytes,
            brand: None,
        });
    }
    if sniff_avi(prefix) {
        return Ok(ContainerProbe {
            kind: VideoContainerKind::Avi,
            size_bytes,
            brand: None,
        });
    }
    if sniff_mpeg_transport_stream(prefix) {
        return Ok(ContainerProbe {
            kind: VideoContainerKind::MpegTransportStream,
            size_bytes,
            brand: None,
        });
    }
    if sniff_flv(prefix) {
        return Ok(ContainerProbe {
            kind: VideoContainerKind::Flv,
            size_bytes,
            brand: None,
        });
    }

    anyhow::bail!("Video Manager quarantine file has an unsupported or unrecognized container signature")
}

fn sniff_iso_bmff(prefix: &[u8]) -> Option<String> {
    if prefix.len() < MIN_BMFF_BOX_BYTES || &prefix[4..8] != b"ftyp" {
        return None;
    }
    let box_size = u32::from_be_bytes(prefix[0..4].try_into().ok()?) as usize;
    if box_size < MIN_BMFF_BOX_BYTES {
        return None;
    }
    let brand: [u8; 4] = prefix[8..12].try_into().ok()?;
    if !supported_bmff_brand(&brand) {
        return None;
    }
    Some(String::from_utf8_lossy(&brand).into_owned())
}

fn supported_bmff_brand(brand: &[u8; 4]) -> bool {
    const BRANDS: &[[u8; 4]] = &[
        *b"isom", *b"iso2", *b"iso3", *b"iso4", *b"iso5", *b"iso6", *b"mp41", *b"mp42",
        *b"avc1", *b"M4V ", *b"qt  ", *b"3gp4", *b"3gp5", *b"3gp6", *b"3g2a", *b"3g2b",
        *b"cmfc", *b"cmfs", *b"dash",
    ];
    BRANDS.contains(brand)
}

fn sniff_matroska(prefix: &[u8]) -> bool {
    prefix.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
        && (contains_ascii_case_insensitive(prefix, b"webm")
            || contains_ascii_case_insensitive(prefix, b"matroska"))
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

fn sniff_avi(prefix: &[u8]) -> bool {
    prefix.len() >= 12 && &prefix[..4] == b"RIFF" && &prefix[8..12] == b"AVI "
}

fn sniff_mpeg_transport_stream(prefix: &[u8]) -> bool {
    has_transport_sync(prefix, 0, 188) || has_transport_sync(prefix, 4, 192)
}

fn has_transport_sync(prefix: &[u8], offset: usize, packet_size: usize) -> bool {
    let third = match offset.checked_add(packet_size.saturating_mul(2)) {
        Some(index) => index,
        None => return false,
    };
    prefix.get(offset) == Some(&0x47)
        && prefix.get(offset + packet_size) == Some(&0x47)
        && prefix.get(third) == Some(&0x47)
}

fn sniff_flv(prefix: &[u8]) -> bool {
    if prefix.len() < 9 || &prefix[..3] != b"FLV" || prefix[3] != 1 {
        return false;
    }
    let flags = prefix[4];
    let header_size = u32::from_be_bytes(prefix[5..9].try_into().unwrap_or([0; 4]));
    flags & 0x01 != 0 && flags & !0x05 == 0 && header_size >= 9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_supported_container_signatures() {
        let mp4 = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom";
        let probe = sniff_video_container(mp4, mp4.len() as u64).unwrap();
        assert_eq!(probe.kind, VideoContainerKind::IsoBmff);
        assert_eq!(probe.brand.as_deref(), Some("mp42"));

        let mut mkv = vec![0x1a, 0x45, 0xdf, 0xa3, 0x93, 0x42, 0x82, 0x88];
        mkv.extend_from_slice(b"matroska");
        assert_eq!(
            sniff_video_container(&mkv, mkv.len() as u64).unwrap().kind,
            VideoContainerKind::Matroska
        );

        let avi = b"RIFF\x20\x00\x00\x00AVI LIST";
        assert_eq!(
            sniff_video_container(avi, avi.len() as u64).unwrap().kind,
            VideoContainerKind::Avi
        );

        let flv = b"FLV\x01\x01\x00\x00\x00\x09";
        assert_eq!(
            sniff_video_container(flv, flv.len() as u64).unwrap().kind,
            VideoContainerKind::Flv
        );
    }

    #[test]
    fn recognizes_mpeg_ts_and_m2ts_sync_patterns() {
        let mut ts = vec![0u8; 377];
        ts[0] = 0x47;
        ts[188] = 0x47;
        ts[376] = 0x47;
        assert_eq!(
            sniff_video_container(&ts, ts.len() as u64).unwrap().kind,
            VideoContainerKind::MpegTransportStream
        );

        let mut m2ts = vec![0u8; 389];
        m2ts[4] = 0x47;
        m2ts[196] = 0x47;
        m2ts[388] = 0x47;
        assert_eq!(
            sniff_video_container(&m2ts, m2ts.len() as u64).unwrap().kind,
            VideoContainerKind::MpegTransportStream
        );
    }

    #[test]
    fn rejects_image_bmff_audio_flv_and_random_bytes() {
        let avif = b"\x00\x00\x00\x18ftypavif\x00\x00\x00\x00avifmif1";
        assert!(sniff_video_container(avif, avif.len() as u64).is_err());

        let audio_only_flv = b"FLV\x01\x04\x00\x00\x00\x09";
        assert!(sniff_video_container(audio_only_flv, audio_only_flv.len() as u64).is_err());
        assert!(sniff_video_container(b"not a media file", 16).is_err());
    }
}
