use std::path::{Path, PathBuf};
use std::time::Duration;

use rbe_install_executor::{
    ArtifactDownloadPlan, DiskBudget, DiskBudgetInput, DiskBudgetPolicy, PromotionPlan,
    ResumeRequest, StreamingVerifier, VerifiedDownload,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::http::get_following_redirects;
use crate::InstallRuntimeError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactStage {
    pub verified: VerifiedDownload,
    pub promotion: PromotionPlan,
    pub resumed_from_bytes: u64,
}

pub async fn stage_artifact(
    plan: &ArtifactDownloadPlan,
) -> Result<ArtifactStage, InstallRuntimeError> {
    ensure_safe_directory(&plan.staging_dir)?;
    let mut partial_bytes = partial_size(&plan.partial_path)?;
    if partial_bytes > plan.limits.maximum_bytes
        || plan
            .expected_size_bytes
            .is_some_and(|expected| partial_bytes > expected)
    {
        tokio::fs::remove_file(&plan.partial_path).await?;
        partial_bytes = 0;
    }

    let artifact_budget_bytes = plan
        .expected_size_bytes
        .unwrap_or(plan.limits.maximum_bytes);
    let available_bytes = fs2::available_space(&plan.staging_dir)?;
    DiskBudget::plan(
        DiskBudgetInput {
            artifact_bytes: artifact_budget_bytes,
            reusable_partial_bytes: partial_bytes,
            available_bytes,
        },
        DiskBudgetPolicy::default(),
    )?;

    let mut verifier = plan.verifier()?;
    if partial_bytes > 0 {
        rehash_prefix(&plan.partial_path, &mut verifier).await?;
        if plan
            .expected_size_bytes
            .is_some_and(|expected| partial_bytes == expected)
        {
            return finish_stage(plan, verifier, partial_bytes);
        }
    }

    let resume = ResumeRequest::for_partial(plan, partial_bytes)?;
    let mut response = get_following_redirects(
        plan.source.clone(),
        resume.as_ref().map(|request| request.range_header.as_str()),
        plan.limits.connect_timeout_seconds,
        plan.limits.idle_timeout_seconds,
        plan.limits.maximum_redirects,
    )
    .await?;

    if let Some(resume) = &resume {
        if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            validate_content_range(
                response.headers().get(reqwest::header::CONTENT_RANGE),
                resume.offset_bytes,
                plan.expected_size_bytes,
            )?;
        } else if response.status() == reqwest::StatusCode::OK
            && plan.resume.restart_on_range_rejection
        {
            partial_bytes = 0;
            verifier = plan.verifier()?;
            response = get_following_redirects(
                plan.source.clone(),
                None,
                plan.limits.connect_timeout_seconds,
                plan.limits.idle_timeout_seconds,
                plan.limits.maximum_redirects,
            )
            .await?;
        } else {
            return Err(InstallRuntimeError::ResumeRejected);
        }
    }

    if !response.status().is_success() {
        return Err(InstallRuntimeError::HttpStatus(response.status().as_u16()));
    }
    validate_content_length(&response, plan.expected_size_bytes, partial_bytes)?;

    ensure_no_symlink_components(&plan.partial_path)?;
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).write(true);
    if partial_bytes > 0 {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut file = options.open(&plan.partial_path).await?;

    loop {
        let chunk = tokio::time::timeout(
            Duration::from_secs(plan.limits.idle_timeout_seconds.max(1)),
            response.chunk(),
        )
        .await
        .map_err(|_| InstallRuntimeError::IdleTimeout(plan.limits.idle_timeout_seconds))?
        .map_err(InstallRuntimeError::HttpBody)?;
        let Some(chunk) = chunk else {
            break;
        };
        verifier.update(&chunk)?;
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    finish_stage(plan, verifier, partial_bytes)
}

fn finish_stage(
    plan: &ArtifactDownloadPlan,
    verifier: StreamingVerifier,
    resumed_from_bytes: u64,
) -> Result<ArtifactStage, InstallRuntimeError> {
    let verified = verifier.finish()?;
    let promotion = plan.promotion(&verified)?;
    Ok(ArtifactStage {
        verified,
        promotion,
        resumed_from_bytes,
    })
}

async fn rehash_prefix(
    path: &Path,
    verifier: &mut StreamingVerifier,
) -> Result<(), InstallRuntimeError> {
    ensure_no_symlink_components(path)?;
    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        verifier.update(&buffer[..read])?;
    }
    Ok(())
}

fn partial_size(path: &Path) -> Result<u64, InstallRuntimeError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(InstallRuntimeError::SymlinkedPath(
                    path.display().to_string(),
                ));
            }
            if !metadata.is_file() {
                return Err(InstallRuntimeError::UnsafeStagingEntry(
                    path.display().to_string(),
                ));
            }
            Ok(metadata.len())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn ensure_safe_directory(path: &Path) -> Result<(), InstallRuntimeError> {
    ensure_no_symlink_components(path)?;
    std::fs::create_dir_all(path)?;
    ensure_no_symlink_components(path)?;
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(InstallRuntimeError::UnsafeStagingEntry(
            path.display().to_string(),
        ));
    }
    Ok(())
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), InstallRuntimeError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(InstallRuntimeError::SymlinkedPath(
                    current.display().to_string(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_content_length(
    response: &reqwest::Response,
    expected_size: Option<u64>,
    existing_bytes: u64,
) -> Result<(), InstallRuntimeError> {
    let Some(value) = response.headers().get(reqwest::header::CONTENT_LENGTH) else {
        return Ok(());
    };
    let value = value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(InstallRuntimeError::InvalidContentLength)?;
    if let Some(expected) = expected_size {
        let remaining = expected
            .checked_sub(existing_bytes)
            .ok_or(InstallRuntimeError::ContentLengthMismatch)?;
        if value != remaining {
            return Err(InstallRuntimeError::ContentLengthMismatch);
        }
    }
    Ok(())
}

fn validate_content_range(
    header: Option<&reqwest::header::HeaderValue>,
    expected_start: u64,
    expected_total: Option<u64>,
) -> Result<(), InstallRuntimeError> {
    let text = header
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| InstallRuntimeError::InvalidContentRange("<missing>".to_string()))?;
    let Some(rest) = text.strip_prefix("bytes ") else {
        return Err(InstallRuntimeError::InvalidContentRange(text.to_string()));
    };
    let Some((range, total)) = rest.split_once('/') else {
        return Err(InstallRuntimeError::InvalidContentRange(text.to_string()));
    };
    let Some((start, end)) = range.split_once('-') else {
        return Err(InstallRuntimeError::InvalidContentRange(text.to_string()));
    };
    let start = start
        .parse::<u64>()
        .map_err(|_| InstallRuntimeError::InvalidContentRange(text.to_string()))?;
    let end = end
        .parse::<u64>()
        .map_err(|_| InstallRuntimeError::InvalidContentRange(text.to_string()))?;
    if start != expected_start || end < start {
        return Err(InstallRuntimeError::InvalidContentRange(text.to_string()));
    }
    if let Some(expected_total) = expected_total {
        let total = total
            .parse::<u64>()
            .map_err(|_| InstallRuntimeError::InvalidContentRange(text.to_string()))?;
        if total != expected_total || end >= total {
            return Err(InstallRuntimeError::InvalidContentRange(text.to_string()));
        }
    } else if total != "*" && total.parse::<u64>().is_err() {
        return Err(InstallRuntimeError::InvalidContentRange(text.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbe_project_package::LockedArtifactFetch;
    use url::Url;

    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn content_range_validates_numeric_start_end_and_total() {
        let valid = reqwest::header::HeaderValue::from_static("bytes 40-99/100");
        validate_content_range(Some(&valid), 40, Some(100)).unwrap();
        for value in ["bytes 39-99/100", "bytes 40-100/100", "bytes 40-20/100"] {
            let value = reqwest::header::HeaderValue::from_str(value).unwrap();
            assert!(validate_content_range(Some(&value), 40, Some(100)).is_err());
        }
    }

    #[tokio::test]
    async fn complete_partial_is_rehashed_without_network_and_becomes_promotable() {
        let temp = tempfile::tempdir().unwrap();
        let cache_path = temp.path().join(".cache/library").join(ABC_SHA256);
        let fetch = LockedArtifactFetch {
            package: "advancenet".into(),
            version: "4.0.1".into(),
            artifact_url: Url::parse("https://example.com/advancenet.rbe").unwrap(),
            artifact_sha256: ABC_SHA256.into(),
            manifest_sha256: "b".repeat(64),
            source_sha256: None,
            cache_path,
        };
        let plan = ArtifactDownloadPlan::from_locked(&fetch, Some(3)).unwrap();
        std::fs::create_dir_all(&plan.staging_dir).unwrap();
        std::fs::write(&plan.partial_path, b"abc").unwrap();

        let staged = stage_artifact(&plan).await.unwrap();
        assert_eq!(staged.verified.sha256, ABC_SHA256);
        assert_eq!(staged.verified.size_bytes, 3);
        assert_eq!(staged.resumed_from_bytes, 3);
        assert_eq!(staged.promotion.verified_partial, plan.partial_path);
    }
}
