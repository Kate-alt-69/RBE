//! Installation-owned Control Room credentials. Downloaded executables contain
//! no default password; local initialization persists only a bounded verifier.
//! Files live outside the four replaceable runtime binaries and survive updates.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};

pub(crate) mod legacy {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/admin_hash.rs"));
}
mod compiled {
    include!(concat!(env!("OUT_DIR"), "/dashboard_auth.rs"));
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scheme", deny_unknown_fields)]
enum Record {
    #[serde(rename = "argon2id-v1")]
    Argon2 { hash: String },
    #[serde(rename = "pbkdf2-sha256-v1")]
    Legacy {
        rounds: u32,
        salt: String,
        verifier: String,
    },
}
static RUNTIME_RECORD: OnceLock<Record> = OnceLock::new();

pub fn deferred() -> bool {
    compiled::ADMIN_SETUP_DEFERRED
}
pub fn configured() -> bool {
    RUNTIME_RECORD.get().is_some() || compiled::ADMIN_PASSWORD_CONFIGURED
}
pub fn directory() -> PathBuf {
    std::env::var_os("RBE_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(runtime_paths::default_admin_dir)
        .join("control-room")
}
fn compiled_record() -> Option<Record> {
    compiled::ADMIN_PASSWORD_CONFIGURED.then(|| Record::Legacy {
        rounds: compiled::ADMIN_PASSWORD_ROUNDS,
        salt: compiled::ADMIN_PASSWORD_SALT_HEX.into(),
        verifier: compiled::ADMIN_PASSWORD_VERIFIER_HEX.into(),
    })
}

/// Protect the directory before creating records or update journals. Fail rather
/// than accepting a symlink or an OS command that failed to apply its ACL.
pub fn private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        let meta = fs::symlink_metadata(path)?;
        // SAFETY: geteuid has no pointer arguments or preconditions.
        let owner = unsafe { libc::geteuid() };
        if !meta.is_dir() || meta.file_type().is_symlink() || meta.uid() != owner {
            bail!("private RBE directory must be a real directory owned by this OS user");
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        use std::{os::windows::fs::MetadataExt, process::Command};
        fs::create_dir_all(path)?;
        if fs::symlink_metadata(path)?.file_attributes() & 0x400 != 0 {
            bail!("private RBE directory cannot be a reparse point");
        }
        // Replace the DACL, including pre-existing explicit grants. Merely
        // disabling inheritance would leave explicit broad grants intact.
        let script = r#"$ErrorActionPreference='Stop';
$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User;
$acl=[Security.AccessControl.DirectorySecurity]::new();
$acl.SetOwner($sid); $acl.SetAccessRuleProtection($true,$false);
foreach($identity in @($sid,([Security.Principal.SecurityIdentifier]::new('S-1-5-18')))) {
 $rule=[Security.AccessControl.FileSystemAccessRule]::new($identity,'FullControl','ContainerInherit,ObjectInherit','None','Allow');
 $acl.AddAccessRule($rule);
}
Set-Acl -LiteralPath $env:RBE_PRIVATE_DIRECTORY -AclObject $acl"#;
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("RBE_PRIVATE_DIRECTORY", path)
            .output()?;
        if !output.status.success() {
            bail!("could not protect the RBE private directory ACL");
        }
    }
    Ok(())
}

fn open_private(path: &Path, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    if create {
        options.write(true).create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("credential record must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no pointer arguments or preconditions.
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            bail!("credential record must be owned by this user with mode 0600");
        }
    }
    Ok(file)
}
fn valid(record: &Record) -> bool {
    match record {
        Record::Argon2 { hash } => PasswordHash::new(hash).ok().is_some_and(|h| {
            h.algorithm.as_str() == "argon2id"
                && h.version == Some(19)
                && h.params.get_decimal("m") == Some(19456)
                && h.params.get_decimal("t") == Some(2)
                && h.params.get_decimal("p") == Some(1)
                && h.salt.is_some()
                && h.hash.is_some_and(|v| v.len() == 32)
        }),
        Record::Legacy {
            rounds,
            salt,
            verifier,
        } => {
            (10000..=1_000_000).contains(rounds)
                && salt.len() == 16
                && verifier.len() == 64
                && salt
                    .bytes()
                    .chain(verifier.bytes())
                    .all(|b| b.is_ascii_hexdigit())
        }
    }
}
fn read_record(dir: &Path) -> Result<Option<Record>> {
    let path = dir.join("credentials.json");
    if !path.try_exists()? {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    open_private(&path, false)?
        .take(2049)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 2048 {
        bail!("credential record exceeds its size limit");
    }
    let record: Record = serde_json::from_slice(&bytes).context("invalid credential record")?;
    if !valid(&record) {
        bail!("unsupported credential verifier or KDF parameters");
    }
    Ok(Some(record))
}
fn write_new(dir: &Path, record: &Record) -> Result<()> {
    private_directory(dir)?;
    if !valid(record) {
        bail!("refusing to store an invalid credential verifier");
    }
    let mut nonce = [0u8; 16];
    OsRng.fill_bytes(&mut nonce);
    let temporary = dir.join(format!(
        "setup-{}.tmp",
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let result = (|| -> Result<()> {
        let mut file = open_private(&temporary, true)?;
        file.write_all(&serde_json::to_vec(record)?)?;
        file.sync_all()?;
        // Exclusive publication: simultaneous first boots cannot overwrite the
        // first successful setup. The final record is never partially written.
        fs::hard_link(&temporary, dir.join("credentials.json"))
            .context("admin credentials already exist or could not be published")?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

/// Load before opening listeners. Corruption fails closed; it never starts a
/// replacement-password flow. A compiled verifier remains compatible until an
/// explicit deferred build loads the installation-owned record.
pub fn load() -> Result<bool> {
    if !deferred() {
        return Ok(configured());
    }
    let dir = directory();
    private_directory(&dir)?;
    if let Some(record) = read_record(&dir)? {
        let _ = RUNTIME_RECORD.set(record);
    }
    Ok(configured())
}
pub fn initialize(password: &str) -> Result<()> {
    if !deferred() {
        bail!("this build uses its compiled admin verifier");
    }
    if !(12..=1024).contains(&password.len()) {
        bail!("admin password must contain 12–1024 UTF-8 bytes");
    }
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
        .map_err(|_| anyhow::anyhow!("could not derive the admin password verifier"))?
        .to_string();
    let record = Record::Argon2 { hash };
    write_new(&directory(), &record)?;
    let _ = RUNTIME_RECORD.set(record);
    Ok(())
}
pub fn verify_password(password: &str) -> bool {
    if password.len() > 1024 {
        return false;
    }
    let legacy_record = compiled_record();
    let record = if deferred() {
        RUNTIME_RECORD.get()
    } else {
        legacy_record.as_ref()
    };
    match record {
        Some(Record::Argon2 { hash }) => PasswordHash::new(hash).ok().is_some_and(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        }),
        Some(Record::Legacy {
            rounds,
            salt,
            verifier,
        }) => match (legacy::hex_decode(salt), legacy::hex_decode(verifier)) {
            (Some(salt), Some(expected)) => {
                legacy::constant_time_eq(&legacy::derive(password, &salt, *rounds), &expected)
            }
            _ => false,
        },
        None => false,
    }
}
/// Preserve compiled credentials before switching to a public deferred build.
/// Never replace a different existing installation record without operator action.
pub fn prepare_for_update() -> Result<()> {
    if deferred() {
        if !configured() {
            bail!("initialize the administrator password before enabling updates");
        }
        return Ok(());
    }
    let record = compiled_record().context("admin credentials are not configured")?;
    let dir = directory();
    private_directory(&dir)?;
    match read_record(&dir)? {
		Some(stored) if stored == record => Ok(()),
		Some(_) => bail!("stored admin credentials differ from this compiled build; explicit migration is required"),
		None => write_new(&dir, &record),
	}
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temporary() -> PathBuf {
        let mut nonce = [0u8; 16];
        OsRng.fill_bytes(&mut nonce);
        std::env::temp_dir().join(format!("rbe-admin-{}", u128::from_le_bytes(nonce)))
    }
    #[test]
    fn generated_verifier_is_well_formed_when_configured() {
        if compiled::ADMIN_PASSWORD_CONFIGURED {
            assert_eq!(compiled::ADMIN_PASSWORD_SALT_HEX.len(), 16);
            assert_eq!(compiled::ADMIN_PASSWORD_VERIFIER_HEX.len(), 64);
            assert!(legacy::hex_decode(compiled::ADMIN_PASSWORD_SALT_HEX).is_some());
            assert!(legacy::hex_decode(compiled::ADMIN_PASSWORD_VERIFIER_HEX).is_some());
        }
    }
    #[test]
    fn first_boot_is_process_local_and_reloads_without_reset() {
        if !deferred() {
            return;
        }
        let mode = std::env::var("RBE_TEST_ADMIN_PHASE").ok();
        if let Some(phase) = mode {
            let password = "isolated first boot password";
            let configured = load().unwrap();
            if phase == "create" {
                assert!(!configured);
                initialize(password).unwrap();
            } else {
                assert!(configured);
            }
            assert!(verify_password(password));
            assert!(!verify_password("a wrong password"));
            assert!(initialize("a replacement password").is_err());
            assert!(verify_password(password));
            return;
        }
        let directory = temporary();
        for phase in ["create", "reload"] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
				.args(["--exact", "admin_credentials::tests::first_boot_is_process_local_and_reloads_without_reset"])
				.env("RBE_TEST_ADMIN_PHASE", phase).env("RBE_CONFIG_HOME", &directory)
				.status().unwrap();
            assert!(result.success());
        }
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn verifier_survives_reload_and_cannot_be_overwritten() {
        let dir = temporary();
        let hash = Argon2::default()
            .hash_password(
                b"local administrator password",
                &SaltString::generate(&mut OsRng),
            )
            .unwrap()
            .to_string();
        let record = Record::Argon2 { hash };
        write_new(&dir, &record).unwrap();
        assert_eq!(read_record(&dir).unwrap(), Some(record.clone()));
        assert!(write_new(&dir, &record).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join("credentials.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn corrupt_and_excessive_kdf_records_fail_closed() {
        let dir = temporary();
        private_directory(&dir).unwrap();
        let path = dir.join("credentials.json");
        open_private(&path, true)
            .unwrap()
            .write_all(b"{incomplete")
            .unwrap();
        assert!(read_record(&dir).is_err());
        assert!(!valid(&Record::Legacy {
            rounds: u32::MAX,
            salt: "00".repeat(8),
            verifier: "00".repeat(32)
        }));
        fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_and_public_records_are_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = temporary();
        private_directory(&dir).unwrap();
        let outside = dir.join("outside");
        fs::write(&outside, "{}").unwrap();
        symlink(&outside, dir.join("credentials.json")).unwrap();
        assert!(read_record(&dir).is_err());
        fs::remove_file(dir.join("credentials.json")).unwrap();
        fs::write(dir.join("credentials.json"), "{}").unwrap();
        fs::set_permissions(
            dir.join("credentials.json"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(read_record(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
