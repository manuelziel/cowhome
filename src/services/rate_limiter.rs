//! Monotonic sliding-window HTTP rate limiter (`RateLimiter`).
//!
//! Protects CowHome's REST API against IP request flooding (`120 req/min`) and
//! brute-force API-key guessing (`5 failed attempts/hour`) using `std::time::Instant`
//! so NTP wall-clock jumps never bypass or extend lockouts.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::core::constants::service_defaults::DEFAULT_RATE_LIMIT_WINDOW_SECS;
use crate::core::logger::Logger;

// =============================================================================
/// Sliding-window counter entry for a single client key (`ip:...` or `login:...`).
#[derive(Debug, Clone, Copy)]
struct WindowEntry {
    /// Number of recorded attempts inside the active window.
    attempts: u32,
    /// Monotonic timestamp of the first attempt in the window.
    first_attempt: Instant,
    /// Active window length in seconds.
    window_secs: u64,
}

// =============================================================================
/// Thread-safe monotonic sliding-window rate limiter shared via `Arc<RateLimiter>`.
#[derive(Debug)]
pub struct RateLimiter {
    /// Per-key sliding-window state guarded by a short-lived mutex.
    limits: Mutex<HashMap<String, WindowEntry>>,
    /// Application logger emitting `WARNING` lines when limits are exceeded.
    logger: Logger,
}

impl RateLimiter {
    /// Creates an empty [`RateLimiter`] wired to `logger`.
    #[must_use]
    pub fn new(logger: Logger) -> Self {
        Self {
            limits: Mutex::new(HashMap::new()),
            logger,
        }
    }

    /// Atomically checks whether `key` exceeds `max_attempts` within `window_secs` and increments.
    #[must_use]
    pub fn is_rate_limited(&self, key: &str, max_attempts: u32, window_secs: u64) -> bool {
        let Ok(mut map) = self.limits.lock() else {
            return false;
        };
        let now = Instant::now();
        let window = Duration::from_secs(window_secs);
        if let Some(entry) = map.get_mut(key) {
            if now.saturating_duration_since(entry.first_attempt) > window {
                *entry = WindowEntry {
                    attempts: 1,
                    first_attempt: now,
                    window_secs,
                };
                return false;
            }
            if entry.attempts >= max_attempts {
                self.logger.warning(&format!(
                    "Rate limit exceeded: {key} ({}/{max_attempts} in {window_secs}s)",
                    entry.attempts
                ));
                return true;
            }
            entry.attempts = entry.attempts.saturating_add(1);
            entry.window_secs = window_secs;
            return false;
        }
        map.insert(
            key.to_owned(),
            WindowEntry {
                attempts: 1,
                first_attempt: now,
                window_secs,
            },
        );
        false
    }

    /// Checks whether `key` is currently locked out without incrementing the counter.
    #[must_use]
    pub fn check_only(&self, key: &str, max_attempts: u32, window_secs: u64) -> bool {
        let Ok(mut map) = self.limits.lock() else {
            return false;
        };
        let Some(&entry) = map.get(key) else {
            return false;
        };
        let now = Instant::now();
        if now.saturating_duration_since(entry.first_attempt) > Duration::from_secs(window_secs) {
            map.remove(key);
            return false;
        }
        if entry.attempts >= max_attempts {
            self.logger.warning(&format!(
                "Rate limit exceeded: {key} ({}/{max_attempts} in {window_secs}s)",
                entry.attempts
            ));
            return true;
        }
        false
    }

    /// Records a failed attempt under `key`, starting a fresh window if expired.
    pub fn record(&self, key: &str, window_secs: u64) {
        let eff_window = if window_secs == 0 {
            DEFAULT_RATE_LIMIT_WINDOW_SECS
        } else {
            window_secs
        };
        let Ok(mut map) = self.limits.lock() else {
            return;
        };
        let now = Instant::now();
        if let Some(entry) = map.get_mut(key) {
            if now.saturating_duration_since(entry.first_attempt) > Duration::from_secs(eff_window)
            {
                *entry = WindowEntry {
                    attempts: 1,
                    first_attempt: now,
                    window_secs: eff_window,
                };
            } else {
                entry.attempts = entry.attempts.saturating_add(1);
                entry.window_secs = eff_window;
            }
        } else {
            map.insert(
                key.to_owned(),
                WindowEntry {
                    attempts: 1,
                    first_attempt: now,
                    window_secs: eff_window,
                },
            );
        }
    }

    /// Clears the counter for `key` (invoked immediately after valid authentication).
    pub fn reset(&self, key: &str) {
        if let Ok(mut map) = self.limits.lock() {
            map.remove(key);
        }
    }

    /// Purges expired entries from RAM and returns the number of evicted keys.
    pub fn cleanup_expired(&self, fallback_window_secs: Option<u64>) -> usize {
        let fallback = fallback_window_secs
            .filter(|&w| w > 0)
            .unwrap_or(DEFAULT_RATE_LIMIT_WINDOW_SECS);
        let Ok(mut map) = self.limits.lock() else {
            return 0;
        };
        let now = Instant::now();
        let before = map.len();
        map.retain(|_, entry| {
            let win = if entry.window_secs > 0 {
                entry.window_secs
            } else {
                fallback
            };
            now.saturating_duration_since(entry.first_attempt) <= Duration::from_secs(win)
        });
        let removed = before.saturating_sub(map.len());
        if removed > 0 {
            self.logger.debug(&format!(
                "Rate limiter cleaned up {removed} expired entries"
            ));
        }
        removed
    }

    /// Returns the number of currently tracked IP/login rate-limit keys.
    #[must_use]
    pub fn tracked_keys_count(&self) -> usize {
        self.limits.lock().map(|m| m.len()).unwrap_or(0)
    }
}
