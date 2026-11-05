//! Outbound request pacing for the Pawchive source.
//!
//! The source answers a burst with `429`, and then keeps answering `403` for a
//! long window afterwards. A round that walks a few thousand resources at full
//! speed spends that window being refused: the files that could have been
//! fetched are rejected along with the rest, and the panel fills with failures
//! the round itself provoked.
//!
//! Two rules are enforced here, for every request the runtime sends to the
//! source — page reads, detail reads and file downloads alike:
//!
//! * consecutive requests are at least `interval` apart, and
//! * once the source answers `429`, the round stops asking until the wait it
//!   named has passed, instead of walking on down the queue.
//!
//! The state is process-wide because the runtime runs one round at a time (the
//! round slot is single) and a manual attempt is traffic to the same host;
//! pacing them against one another is the point, not an accident.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Spacing between two outbound requests to the source.
///
/// One second is enough to keep a full round inside the source's allowance
/// without turning a thousand-file backlog into an hour of waiting.
pub const PAWCHIVE_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1000);

/// Stand-down used when a `429` carries no `Retry-After`.
const DEFAULT_RATE_LIMIT_WAIT_SECS: i64 = 60;

/// Stand-down used after consecutive access denials (403 Forbidden).
const ACCESS_DENIED_STAND_DOWN_SECS: i64 = 900;

/// How many consecutive access denials trip the breaker.
const ACCESS_DENIED_MAX_CONSECUTIVE: u32 = 3;

/// Request pacing plus the rate-limit stand-down shared by one process.
pub struct PawchiveThrottle {
    interval: Duration,
    last_request: Mutex<Option<Instant>>,
    rate_limited_until: Mutex<Option<Instant>>,
    consecutive_denied: Mutex<u32>,
    /// Whether a `429` is allowed to stop the callers.
    ///
    /// The test build turns this off: one suite shares one process, and a single
    /// simulated `429` would otherwise stand every later test down for a
    /// minute. The behaviour itself is covered by this module's own tests, which
    /// build their own throttle.
    honour_rate_limit: bool,
}

impl PawchiveThrottle {
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_request: Mutex::new(None),
            rate_limited_until: Mutex::new(None),
            consecutive_denied: Mutex::new(0),
            honour_rate_limit: true,
        }
    }


    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Wait until the next request is allowed to go out.
    ///
    /// The slot is taken before the sleep, so two callers that arrive together
    /// do not both decide the same moment is free.
    pub async fn wait(&self) {
        if self.interval.is_zero() {
            return;
        }
        loop {
            let sleep_for = {
                let mut guard = self.last_request.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                match *guard {
                    Some(last) => {
                        let due = last + self.interval;
                        if now >= due {
                            *guard = Some(now);
                            return;
                        }
                        due - now
                    }
                    None => {
                        *guard = Some(now);
                        return;
                    }
                }
            };
            tokio::time::sleep(sleep_for).await;
        }
    }

    /// Record a `429` from the source.
    ///
    /// Returns the number of seconds the stand-down lasts, so the caller can
    /// report the same figure it enforced.
    pub fn note_rate_limit(&self, retry_after_secs: Option<i64>) -> i64 {
        if !self.honour_rate_limit {
            return 0;
        }
        let seconds = retry_after_secs
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_RATE_LIMIT_WAIT_SECS)
            .clamp(1, 24 * 60 * 60);
        let until = Instant::now() + Duration::from_secs(seconds as u64);
        let mut guard = self
            .rate_limited_until
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // A second 429 while standing down must not shorten the window.
        if guard.is_none_or(|current| until > current) {
            *guard = Some(until);
        }
        seconds
    }

    /// Record a successful response from the source, resetting access denial strikes.
    pub fn note_success(&self) {
        let mut guard = self
            .consecutive_denied
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = 0;
    }

    /// Record an access-denied response (401 / 403).
    ///
    /// If consecutive denials reach the threshold without an intervening success,
    /// stands the process down to avoid hammering the host. Returns true if stand-down was tripped.
    pub fn note_access_denied(&self) -> bool {
        if !self.honour_rate_limit {
            return false;
        }
        let mut guard = self
            .consecutive_denied
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard += 1;
        if *guard >= ACCESS_DENIED_MAX_CONSECUTIVE {
            *guard = 0;
            self.note_rate_limit(Some(ACCESS_DENIED_STAND_DOWN_SECS));
            true
        } else {
            false
        }
    }

    /// Whether the source has asked this process to stand down.
    pub fn rate_limited(&self) -> bool {
        self.honour_rate_limit && self.seconds_until_allowed().is_some()
    }

    /// Remaining stand-down, if one is in force.
    pub fn seconds_until_allowed(&self) -> Option<i64> {
        let guard = self
            .rate_limited_until
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.and_then(|until| {
            let now = Instant::now();
            if until > now {
                Some((until - now).as_secs().max(1) as i64)
            } else {
                None
            }
        })
    }

}

/// The throttle every Pawchive request goes through.
pub fn pawchive_throttle() -> &'static PawchiveThrottle {
    static THROTTLE: std::sync::OnceLock<PawchiveThrottle> = std::sync::OnceLock::new();
    THROTTLE.get_or_init(|| {

        #[cfg(not(test))]
        {
            PawchiveThrottle::new(PAWCHIVE_MIN_REQUEST_INTERVAL)
        }
    })
}
