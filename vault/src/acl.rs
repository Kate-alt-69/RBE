//! Access control for credentials: which named "callers" (a string
//! identifying the calling subsystem — `"email_service"`,
//! `"uac"`, etc., not a per-request identity) may access which
//! credential. **Fails closed**: a credential with no ACL entry, or a
//! caller not in that entry's list, is denied — matching the
//! handbook's ACL-gated secret access design (§8 in the migration
//! plan). There is no default-allow path.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct Acl {
    /// credential name -> set of caller names permitted to read it.
    entries: HashMap<String, HashSet<String>>,
}

impl Acl {
    /// Loads `vault-acl.json` from `dir` if present. Missing file is
    /// NOT an error — it just means an empty (fully closed) ACL, which
    /// is a safe default; every credential access will be denied until
    /// entries are added. A malformed file (present but unparsable)
    /// IS an error — silently treating a broken ACL file as "empty" is
    /// the kind of failure mode that should be loud, not quiet.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join("vault-acl.json");
        if !path.exists() {
            return Ok(Self {
                entries: HashMap::new(),
            });
        }
        let raw = fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display()))
    }

    pub fn is_allowed(&self, credential_name: &str, caller: &str) -> bool {
        self.entries
            .get(credential_name)
            .map(|allowed| allowed.contains(caller))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_credential_is_denied() {
        let acl = Acl {
            entries: HashMap::new(),
        };
        assert!(!acl.is_allowed("db.password", "email_service"));
    }

    #[test]
    fn unlisted_caller_is_denied() {
        let mut entries = HashMap::new();
        entries.insert(
            "db.password".to_string(),
            HashSet::from(["uac".to_string()]),
        );
        let acl = Acl { entries };
        assert!(!acl.is_allowed("db.password", "email_service"));
        assert!(acl.is_allowed("db.password", "uac"));
    }
}
