//! A small per-key fixed-window counter. Both rate limiting and IP
//! strike tracking are "count events for a key within a rolling
//! window, compare to a threshold" — same primitive, different config
//! and different reaction to the threshold being hit. Deliberately not
//! using an external rate-limiting crate for v1: this is simple enough
//! to hand-verify correctness on without a compiler, and a fixed
//! window is a fine v1 (a sliding/token-bucket window is a real
//! upgrade to make later if fixed-window's edge-of-window burst
//! behavior turns out to matter in practice — measure first).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Entry {
    window_start: Instant,
    count: u32,
}

pub struct WindowedCounter {
    entries: Mutex<HashMap<String, Entry>>,
}

impl Default for WindowedCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowedCounter {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Records one hit for `key` and returns the count within the
    /// current window (including this hit). If the previous window for
    /// this key has expired, it resets to 1.
    pub fn hit(&self, key: &str, window: Duration) -> u32 {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();

        match entries.get_mut(key) {
            Some(entry) if now.duration_since(entry.window_start) < window => {
                entry.count += 1;
                entry.count
            }
            _ => {
                entries.insert(
                    key.to_string(),
                    Entry {
                        window_start: now,
                        count: 1,
                    },
                );
                1
            }
        }
    }

    /// Opportunistic cleanup of entries whose window has long expired,
    /// so this doesn't grow unbounded under a wide spread of distinct
    /// keys (many distinct IPs). Call periodically from a supervised
    /// task, not per-request.
    pub fn sweep(&self, max_age: Duration) {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|_, entry| now.duration_since(entry.window_start) < max_age);
    }
}
