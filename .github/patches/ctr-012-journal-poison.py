from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "container-runtime/crates/container-runtime-core/src/runtime.rs",
    "use std::sync::{Arc, Condvar, Mutex};\n",
    "use std::sync::{Arc, Condvar, Mutex, MutexGuard};\n",
    "journal MutexGuard import",
)

replace_once(
    "container-runtime/crates/container-runtime-core/src/runtime.rs",
    '''impl Journal {
    fn open() -> Arc<Self> {
''',
    '''impl Journal {
    fn lock_guard(&self) -> MutexGuard<'_, ()> {
        match self.lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // The mutex serializes durable journal I/O but owns no
                // authoritative in-memory state. The journal parser already
                // ignores malformed/partial lines, so poison can be cleared
                // safely and later valid executions must remain recoverable.
                self.lock.clear_poison();
                poisoned.into_inner()
            }
        }
    }

    fn open() -> Arc<Self> {
''',
    "journal poison recovery helper",
)

replace_once(
    "container-runtime/crates/container-runtime-core/src/runtime.rs",
    '        let _guard = self.lock.lock().expect("journal lock poisoned");\n        let Ok(line) = serde_json::to_string(&event) else {\n',
    '        let _guard = self.lock_guard();\n        let Ok(line) = serde_json::to_string(&event) else {\n',
    "journal append poison recovery",
)

replace_once(
    "container-runtime/crates/container-runtime-core/src/runtime.rs",
    '        let _guard = self.lock.lock().expect("journal lock poisoned");\n        let Ok(contents) = read_to_string(&self.path) else {\n',
    '        let _guard = self.lock_guard();\n        let Ok(contents) = read_to_string(&self.path) else {\n',
    "journal recover poison recovery",
)

replace_once(
    "container-runtime/crates/container-runtime-core/src/runtime.rs",
    '''    #[test]
    fn journal_recovery_preserves_execution_provenance() {
''',
    '''    #[test]
    fn poisoned_journal_lock_recovers_and_keeps_durable_replay_working() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rbe-execution-journal-poison-{}-{nonce}.jsonl",
            std::process::id()
        ));
        let journal = Journal {
            path: path.clone(),
            lock: Mutex::new(()),
            io: atomic_io::AtomicIo::new(),
        };

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = journal.lock.lock().unwrap();
            panic!("intentional execution journal lock poison");
        }));
        assert!(poisoned.is_err());
        assert!(journal.lock.is_poisoned());

        journal.append(journal_event(false));
        assert!(!journal.lock.is_poisoned());
        let (pending, max_sequence) = journal.recover();
        assert_eq!(max_sequence, 9);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id.sequence(), 9);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn journal_recovery_preserves_execution_provenance() {
''',
    "journal poison regression",
)
