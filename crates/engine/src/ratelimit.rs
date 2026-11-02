//! Shared pacing of source reads (`--read-limit`), so a backup does not starve a busy
//! filesystem. All reader threads draw from one budget.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimit {
    bytes_per_sec: f64,
    state: Mutex<(Instant, f64)>,
}

impl RateLimit {
    pub fn new(bytes_per_sec: u64) -> Self {
        RateLimit { bytes_per_sec: bytes_per_sec.max(1) as f64, state: Mutex::new((Instant::now(), 0.0)) }
    }

    /// Account for `n` bytes about to be read; sleeps until the average rate allows it.
    pub fn take(&self, n: usize) {
        let wait = {
            let mut st = self.state.lock().unwrap();
            // Time not used (before the first read, or while idle) is not made up for with
            // a burst: allow at most a quarter of a second's worth ahead.
            let now = Instant::now();
            let due = st.0 + Duration::from_secs_f64(st.1 / self.bytes_per_sec);
            if let Some(idle) = now.checked_duration_since(due + Duration::from_millis(250)) {
                st.0 += idle;
            }
            st.1 += n as f64;
            let due = st.0 + Duration::from_secs_f64(st.1 / self.bytes_per_sec);
            due.saturating_duration_since(Instant::now())
        };
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paces_reads() {
        let r = RateLimit::new(10_000_000);
        let t = Instant::now();
        for _ in 0..10 {
            r.take(500_000);
        }
        let s = t.elapsed().as_secs_f64();
        assert!((0.45..1.5).contains(&s), "5 MB at 10 MB/s took {s}s");
    }
}
