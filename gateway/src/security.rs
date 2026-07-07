//! Per-pubkey rate limiting and NIP-98 replay protection.
//!
//! The gateway moves real value (custodial sats + operator fees), so two cheap,
//! in-process guards sit on the authenticated path, after signature verification:
//!
//! - **Replay**: a NIP-98 event id may be spent once. The freshness window
//!   ([`crate::state::AuthConfig::max_skew_secs`]) otherwise lets a captured
//!   `Authorization` header be replayed until it goes stale — for a charged
//!   `mint`/`send` that means a duplicate op. We remember spent ids until they
//!   expire (`created_at + skew`) and reject duplicates.
//! - **Rate limit**: a token bucket per pubkey caps how fast one identity can hit
//!   the shared node, so a single user can't starve the others.
//!
//! Both are process-local (the gateway is a single process) and self-pruning, so
//! they add no external dependency and bounded memory.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Tunables, from [`crate::config::Config`].
#[derive(Debug, Clone, Copy)]
pub struct SecurityConfig {
    /// Token-bucket capacity — the largest burst of requests one pubkey may make.
    /// `<= 0` disables rate limiting.
    pub burst: f64,
    /// Tokens refilled per second — the sustained per-pubkey request rate.
    pub refill_per_sec: f64,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Default)]
struct Maps {
    /// Spent NIP-98 event ids (hex) → expiry (unix secs). Pruned lazily.
    replay: HashMap<String, u64>,
    /// Per-pubkey token buckets. Pruned by idle time when the map grows.
    buckets: HashMap<String, Bucket>,
}

/// Shared rate-limit + replay guard, held in [`crate::state::AppState`].
pub struct Security {
    cfg: SecurityConfig,
    maps: Mutex<Maps>,
}

/// Above this many tracked pubkeys, drop buckets idle longer than [`BUCKET_TTL`].
const BUCKET_PRUNE_THRESHOLD: usize = 1024;
/// Above this many spent ids, sweep the expired ones.
const REPLAY_PRUNE_THRESHOLD: usize = 256;
/// Idle buckets older than this are dropped when pruning (a returning pubkey just
/// gets a fresh full bucket, which only ever favors the caller).
const BUCKET_TTL: Duration = Duration::from_secs(600);

impl Security {
    pub fn new(cfg: SecurityConfig) -> Self {
        Self {
            cfg,
            maps: Mutex::new(Maps::default()),
        }
    }

    /// Try to consume one token for `pubkey`. Returns `false` when the bucket is
    /// empty (the caller should answer 429). Disabled (always `true`) when either
    /// tunable is non-positive.
    pub fn allow_rate(&self, pubkey: &str) -> bool {
        if self.cfg.refill_per_sec <= 0.0 || self.cfg.burst <= 0.0 {
            return true;
        }
        let now = Instant::now();
        let mut m = self.maps.lock().unwrap();
        if m.buckets.len() > BUCKET_PRUNE_THRESHOLD {
            let cutoff = now.checked_sub(BUCKET_TTL).unwrap_or(now);
            m.buckets.retain(|_, b| b.last >= cutoff);
        }
        let burst = self.cfg.burst;
        let refill = self.cfg.refill_per_sec;
        let b = m.buckets.entry(pubkey.to_string()).or_insert(Bucket {
            tokens: burst,
            last: now,
        });
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * refill).min(burst);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Record NIP-98 event id `event_id` as spent until `expiry` (unix secs).
    /// Returns `false` if it was already spent and not yet expired (a replay).
    pub fn check_replay(&self, event_id: &str, expiry: u64, now: u64) -> bool {
        let mut m = self.maps.lock().unwrap();
        if m.replay.len() > REPLAY_PRUNE_THRESHOLD {
            m.replay.retain(|_, exp| *exp > now);
        }
        if let Some(&exp) = m.replay.get(event_id) {
            if exp > now {
                return false; // still-valid spent id -> replay
            }
        }
        m.replay.insert(event_id.to_string(), expiry);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec(burst: f64, refill: f64) -> Security {
        Security::new(SecurityConfig {
            burst,
            refill_per_sec: refill,
        })
    }

    #[test]
    fn replay_rejects_second_use_then_allows_after_expiry() {
        let s = sec(0.0, 0.0);
        let now = 1_000_000;
        let expiry = now + 60;
        assert!(s.check_replay("id-a", expiry, now)); // first use ok
        assert!(!s.check_replay("id-a", expiry, now)); // replay within window
        assert!(s.check_replay("id-b", expiry, now)); // a different id is fine

        // Once the window passes, the id is free again (it would fail the skew
        // check upstream anyway, but the guard must not grow forever).
        assert!(s.check_replay("id-a", now + 120 + 60, now + 120));
    }

    #[test]
    fn rate_limit_allows_burst_then_denies() {
        // burst 3 with a negligible refill (positive so limiting is ON): the
        // first 3 requests in one instant pass, the 4th is denied.
        let s = sec(3.0, 0.000_001);
        assert!(s.allow_rate("pk"));
        assert!(s.allow_rate("pk"));
        assert!(s.allow_rate("pk"));
        assert!(!s.allow_rate("pk"));
        // A different pubkey has its own bucket.
        assert!(s.allow_rate("other"));
    }

    #[test]
    fn rate_limit_disabled_when_non_positive() {
        let s = sec(0.0, 0.0);
        for _ in 0..100 {
            assert!(s.allow_rate("pk")); // disabled -> always allowed
        }
    }
}
