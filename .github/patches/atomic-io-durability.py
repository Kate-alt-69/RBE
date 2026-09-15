from pathlib import Path

cargo = Path('atomic-io/Cargo.toml')
text = cargo.read_text()
block = '''\n[target.'cfg(windows)'.dependencies]\nwindows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_Storage_FileSystem"] }\n'''
if "windows-sys" not in text:
    text = text.rstrip() + block
cargo.write_text(text)

lib = Path('atomic-io/src/lib.rs')
text = lib.read_text()
text = text.replace('use std::io::Write;\n', 'use std::io::{self, Write};\n', 1)

old_doc = '''//! - [`AtomicIo::write_atomic`]: full-file replace is genuinely
//!   atomic — write to a temp file in the same directory, `sync_all`,
//!   then `rename` over the target. `rename` is atomic at the OS level
//!   on both POSIX and Windows for same-volume renames, so a reader
//!   never observes a partially-written file, and a crash mid-write
//!   leaves the OLD file intact (or an orphaned temp file), never a
//!   corrupted target.
'''
new_doc = '''//! - [`AtomicIo::write_atomic`]: full-file replace is genuinely
//!   atomic — write to a temp file in the same directory, `sync_all`,
//!   then atomically replace the target. Unix additionally fsyncs the
//!   containing directory after namespace changes; Windows uses
//!   `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` so replacing an
//!   existing target works and the move is flushed before success is
//!   reported. A reader never observes a partially-written file, and
//!   a crash mid-write leaves either the old or the committed file,
//!   never a partially-written target.
'''
if old_doc not in text:
    raise SystemExit('atomic-io module guarantee anchor changed')
text = text.replace(old_doc, new_doc, 1)

old_write = '''        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = tmp_path_for(path);
        {
            let mut tmp_file = fs::File::create(&tmp_path)?;
            tmp_file.write_all(bytes)?;
            tmp_file.sync_all()?;
        }
        fs::rename(&tmp_path, path)?;
'''
new_write = '''        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            create_dir_all_durable(parent)?;
        }

        let tmp_path = tmp_path_for(path);
        {
            let mut tmp_file = fs::File::create(&tmp_path)?;
            tmp_file.write_all(bytes)?;
            tmp_file.sync_all()?;
        }
        if let Err(error) = replace_atomic(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(error);
        }
'''
if old_write not in text:
    raise SystemExit('write_atomic implementation anchor changed')
text = text.replace(old_write, new_write, 1)

marker = '''fn tmp_path_for(path: &Path) -> PathBuf {
'''
helpers = r'''fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn create_dir_all_durable(path: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match fs::metadata(&current) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a directory", current.display()),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(current.clone());
                let Some(parent) = current.parent() else {
                    break;
                };
                if parent.as_os_str().is_empty() {
                    break;
                }
                current = parent.to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }

    fs::create_dir_all(path)?;
    #[cfg(unix)]
    for directory in missing.iter().rev() {
        sync_directory(parent_dir(directory))?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(unix)]
fn replace_atomic(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)?;
    sync_directory(parent_dir(target))
}

#[cfg(windows)]
fn replace_atomic(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let mut value = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if value.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "atomic-io path contains an embedded NUL",
            ));
        }
        value.push(0);
        Ok(value)
    }

    let source = wide(source)?;
    let target = wide(target)?;
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    // SAFETY: both buffers are owned, NUL-terminated UTF-16 paths and
    // remain alive for the duration of the Win32 call.
    let result = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), flags) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(all(not(unix), not(windows)))]
fn replace_atomic(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

'''
if marker not in text:
    raise SystemExit('tmp_path_for anchor changed')
text = text.replace(marker, helpers + marker, 1)

marker_test = '''    #[test]
    fn append_locked_accumulates() {
'''
new_tests = '''    #[test]
    fn write_atomic_replaces_existing_target() {
        let dir = temp_dir("replace-existing");
        let io = AtomicIo::new();
        let path = dir.join("file.txt");
        io.write_atomic(&path, b"first").unwrap();
        io.write_atomic(&path, b"second").unwrap();
        assert_eq!(io.read(&path).unwrap(), b"second");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_creates_nested_parent_directories() {
        let dir = temp_dir("nested-parent");
        let io = AtomicIo::new();
        let path = dir.join("a/b/c/file.txt");
        io.write_atomic(&path, b"nested").unwrap();
        assert_eq!(io.read(&path).unwrap(), b"nested");
        let _ = fs::remove_dir_all(&dir);
    }

'''
if marker_test not in text:
    raise SystemExit('atomic-io test insertion anchor changed')
text = text.replace(marker_test, new_tests + marker_test, 1)
lib.write_text(text)

readme = Path('atomic-io/README.md')
text = readme.read_text()
old = '''- `write_atomic` — genuinely atomic full-file replace (temp file +
  `sync_all` + `rename`). A reader never observes a partial write; a
  crash mid-write leaves the old file intact.
'''
new = '''- `write_atomic` — genuinely atomic full-file replace. The temp file
  is flushed before commit; Unix also fsyncs directory namespace
  changes, while Windows uses a replace-existing, write-through move.
  Rewriting an existing target therefore keeps the same atomic-replace
  contract on both platforms.
'''
if old not in text:
    raise SystemExit('atomic-io README guarantee anchor changed')
readme.write_text(text.replace(old, new, 1))
