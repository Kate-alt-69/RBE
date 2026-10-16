//! Trusted root identity carried by Backend/Container host adapters.
//!
//! This contains identities only. Resolution to host `PathBuf`s is intentionally
//! outside REL-facing code so symbolic paths never become ambient filesystem access.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRootAuthority {
    pub project_root_id: String,
    pub temp_root_id: String,
    pub cache_root_id: String,
}

impl RuntimeRootAuthority {
    pub fn new(
        project_root_id: impl Into<String>,
        temp_root_id: impl Into<String>,
        cache_root_id: impl Into<String>,
    ) -> Result<Self, String> {
        let authority = Self {
            project_root_id: project_root_id.into(),
            temp_root_id: temp_root_id.into(),
            cache_root_id: cache_root_id.into(),
        };
        for (label, value) in [
            ("project", authority.project_root_id.as_str()),
            ("temp", authority.temp_root_id.as_str()),
            ("cache", authority.cache_root_id.as_str()),
        ] {
            if value.trim().is_empty() || value.contains(['\r', '\n', '\0']) {
                return Err(format!("invalid {label} runtime root identity"));
            }
        }
        Ok(authority)
    }
}
