#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::io;
use std::mem::size_of;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

const ACCESS_FS_EXECUTE: u64 = 1 << 0;
const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_FS_READ_FILE: u64 = 1 << 2;
const ACCESS_FS_READ_DIR: u64 = 1 << 3;
const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const ACCESS_FS_REFER: u64 = 1 << 13;
const ACCESS_FS_TRUNCATE: u64 = 1 << 14;

const READ_ONLY_RIGHTS: u64 = ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR;
const WRITE_BASE_RIGHTS: u64 = ACCESS_FS_WRITE_FILE
    | ACCESS_FS_REMOVE_DIR
    | ACCESS_FS_REMOVE_FILE
    | ACCESS_FS_MAKE_CHAR
    | ACCESS_FS_MAKE_DIR
    | ACCESS_FS_MAKE_REG
    | ACCESS_FS_MAKE_SOCK
    | ACCESS_FS_MAKE_FIFO
    | ACCESS_FS_MAKE_BLOCK
    | ACCESS_FS_MAKE_SYM;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

struct RawFd(i32);

impl Drop for RawFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

pub fn install_workspace_landlock(workspace: &Path, program: &Path) -> io::Result<()> {
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (workspace, program);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Landlock workspace sandbox currently supports x86_64 Linux only",
        ));
    }

    #[cfg(target_arch = "x86_64")]
    {
        let workspace = workspace.canonicalize()?;
        if !workspace.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Landlock workspace must be a directory",
            ));
        }
        let program = program.canonicalize()?;
        if !program.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Landlock program must be a regular file",
            ));
        }

        let abi = landlock_abi()?;
        let handled = supported_rights(abi);
        let ruleset = create_ruleset(handled)?;

        add_path_rule(&ruleset, &workspace, handled, true)?;
        if let Some(parent) = program.parent() {
            add_path_rule(&ruleset, parent, READ_ONLY_RIGHTS & handled, false)?;
        }
        add_path_rule(
            &ruleset,
            &program,
            (ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE) & handled,
            false,
        )?;

        for path in standard_runtime_read_paths() {
            if path.exists() {
                add_path_rule(&ruleset, &path, READ_ONLY_RIGHTS & handled, false)?;
            }
        }

        let rc = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.0, 0_u32) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
fn landlock_abi() -> io::Result<u32> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0_usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    u32::try_from(rc).map_err(|_| io::Error::other("invalid Landlock ABI version"))
}

#[cfg(target_arch = "x86_64")]
fn supported_rights(abi: u32) -> u64 {
    let mut rights = READ_ONLY_RIGHTS | WRITE_BASE_RIGHTS;
    if abi >= 2 {
        rights |= ACCESS_FS_REFER;
    }
    if abi >= 3 {
        rights |= ACCESS_FS_TRUNCATE;
    }
    rights
}

#[cfg(target_arch = "x86_64")]
fn create_ruleset(handled_access_fs: u64) -> io::Result<RawFd> {
    let attr = RulesetAttr { handled_access_fs };
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            size_of::<RulesetAttr>(),
            0_u32,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(RawFd(i32::try_from(rc).map_err(|_| {
        io::Error::other("invalid Landlock ruleset fd")
    })?))
}

#[cfg(target_arch = "x86_64")]
fn add_path_rule(ruleset: &RawFd, path: &Path, rights: u64, writable: bool) -> io::Result<()> {
    if rights == 0 {
        return Ok(());
    }
    let metadata = std::fs::metadata(path)?;
    let allowed_access = if writable || metadata.is_dir() {
        rights
    } else {
        rights
            & !(ACCESS_FS_READ_DIR
                | ACCESS_FS_REMOVE_DIR
                | ACCESS_FS_MAKE_CHAR
                | ACCESS_FS_MAKE_DIR
                | ACCESS_FS_MAKE_REG
                | ACCESS_FS_MAKE_SOCK
                | ACCESS_FS_MAKE_FIFO
                | ACCESS_FS_MAKE_BLOCK
                | ACCESS_FS_MAKE_SYM
                | ACCESS_FS_REFER)
    };
    let parent = open_path(path)?;
    let attr = PathBeneathAttr {
        allowed_access,
        parent_fd: parent.0,
    };
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.0,
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0_u32,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn open_path(path: &Path) -> io::Result<RawFd> {
    let value = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let fd = unsafe { libc::open(value.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(RawFd(fd))
}

#[cfg(target_arch = "x86_64")]
fn standard_runtime_read_paths() -> Vec<PathBuf> {
    [
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/usr/share/zoneinfo",
        "/etc/ld.so.cache",
        "/etc/localtime",
        "/dev/null",
        "/dev/urandom",
        "/dev/random",
        "/proc",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn newer_abis_add_rights_without_dropping_v1_rights() {
        let v1 = supported_rights(1);
        let v3 = supported_rights(3);
        assert_eq!(v1 & READ_ONLY_RIGHTS, READ_ONLY_RIGHTS);
        assert_eq!(v3 & v1, v1);
        assert_ne!(v3 & ACCESS_FS_REFER, 0);
        assert_ne!(v3 & ACCESS_FS_TRUNCATE, 0);
    }
}
