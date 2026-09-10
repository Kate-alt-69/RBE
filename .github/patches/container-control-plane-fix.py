from pathlib import Path

path = Path("container-runtime/crates/container-runtime-core/src/runtime.rs")
text = path.read_text(encoding="utf-8")
old = '''    pub fn has_environment(&self, id: EnvironmentId) -> bool {
        self.environment(id).is_some()
    }
    pub fn global_queue_len(&self) -> usize {
'''
new = '''    pub fn has_environment(&self, id: EnvironmentId) -> bool {
        self.environment(id).is_some()
    }
    pub fn environment_storage(
        &self,
        id: EnvironmentId,
    ) -> Option<Arc<crate::storage::EnvironmentStorageManager>> {
        self.environment(id).map(EnvironmentRuntime::storage)
    }
    pub fn global_queue_len(&self) -> usize {
'''
if text.count(old) != 1:
    raise SystemExit("runtime environment storage anchor changed")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
