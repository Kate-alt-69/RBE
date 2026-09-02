from pathlib import Path

path = Path("../group-memory/src/lib.rs")
source = path.read_text()

create_anchor = '''        let file = FsOpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.set_len(total_len as u64)?;
'''
create_replacement = '''        let file = FsOpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        // Hold an exclusive advisory lock across sizing + header initialization.
        // A cooperating opener will block instead of observing the transient
        // zero-filled/truncated file between create_new() and header flush.
        let _initialization_lock = OwnedFileLock::exclusive(&file)?;
        file.set_len(total_len as u64)?;
'''
if create_anchor not in source:
    raise SystemExit("Group Memory create initialization anchor missing")
source = source.replace(create_anchor, create_replacement, 1)

open_anchor = '''        let file = match access {
            AccessMode::ReadOnly => FsOpenOptions::new().read(true).open(path)?,
            AccessMode::ReadWrite => FsOpenOptions::new().read(true).write(true).open(path)?,
        };
        let (payload_len, total_len) = read_and_validate_header(&file)?;
'''
open_replacement = '''        let file = match access {
            AccessMode::ReadOnly => FsOpenOptions::new().read(true).open(path)?,
            AccessMode::ReadWrite => FsOpenOptions::new().read(true).write(true).open(path)?,
        };
        // Serialize header validation with create-time initialization and any
        // future header migration performed under the same cooperative lock.
        let _header_lock = OwnedFileLock::shared(&file)?;
        let (payload_len, total_len) = read_and_validate_header(&file)?;
'''
if open_anchor not in source:
    raise SystemExit("Group Memory open header anchor missing")
source = source.replace(open_anchor, open_replacement, 1)

tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("Group Memory tests tail missing")
test = r'''

    #[test]
    fn open_waits_for_exclusive_header_initialization_lock() {
        use std::sync::{mpsc, Arc, Barrier};
        use std::thread;
        use std::time::Duration;

        let path = test_path("header-lock");
        let region = GroupMemoryRegion::create(&path, 16).unwrap();
        region.flush().unwrap();
        drop(region);

        let file = FsOpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let lock = OwnedFileLock::exclusive(&file).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let thread_barrier = barrier.clone();
        let thread_path = path.clone();
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            thread_barrier.wait();
            let opened = GroupMemoryRegion::open(&thread_path, AccessMode::ReadOnly).is_ok();
            sender.send(opened).unwrap();
        });

        barrier.wait();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(lock);
        assert!(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        handle.join().unwrap();
        let _ = fs::remove_file(path);
    }
'''
source = source[:tests_end] + test + source[tests_end:]
path.write_text(source)
