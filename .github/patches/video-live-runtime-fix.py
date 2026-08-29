from pathlib import Path

path = Path("crates/video-manager/src/live_runtime.rs")
source = path.read_text()
old = '''    pub fn spawn_live_runtime(
        self: Arc<Self>,
        driver: Arc<dyn LiveRuntimeDriver>,
    ) -> anyhow::Result<LiveRuntimeHandle> {
        self.spawn_live_runtime_with_idle(driver, Duration::from_secs(self.live_idle_secs))
    }
'''
new = '''    pub fn spawn_live_runtime(
        self: Arc<Self>,
        driver: Arc<dyn LiveRuntimeDriver>,
    ) -> anyhow::Result<LiveRuntimeHandle> {
        let idle_timeout = Duration::from_secs(self.live_idle_secs);
        self.spawn_live_runtime_with_idle(driver, idle_timeout)
    }
'''
if old not in source:
    raise SystemExit("live runtime Arc ownership anchor missing")
path.write_text(source.replace(old, new, 1))
