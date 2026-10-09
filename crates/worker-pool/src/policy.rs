//! Browser-independent decisions consumed by protocol adapters.

use std::time::{Duration, Instant};

/// A quiet window must be continuous: any observation above the threshold
/// resets it. Adapters supply observations; this policy owns the clock decision.
#[derive(Debug, Default)]
pub struct QuietWindow {
    since: Option<Instant>,
}

impl QuietWindow {
    pub fn observe(
        &mut self,
        now: Instant,
        in_flight: usize,
        max_in_flight: usize,
        idle: Duration,
    ) -> bool {
        if in_flight > max_in_flight {
            self.since = None;
            return false;
        }
        let since = self.since.get_or_insert(now);
        now.saturating_duration_since(*since) >= idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_observation_resets_the_entire_quiet_window() {
        let start = Instant::now();
        let idle = Duration::from_millis(100);
        let mut window = QuietWindow::default();
        assert!(!window.observe(start, 1, 1, idle));
        assert!(!window.observe(start + idle / 2, 2, 1, idle));
        assert!(!window.observe(start + idle, 1, 1, idle));
        assert!(!window.observe(start + idle + idle / 2, 0, 1, idle));
        assert!(window.observe(start + idle * 2, 0, 1, idle));
    }

    #[test]
    fn zero_idle_still_requires_an_observation_within_the_threshold() {
        let now = Instant::now();
        let mut window = QuietWindow::default();
        assert!(!window.observe(now, 1, 0, Duration::ZERO));
        assert!(window.observe(now, 0, 0, Duration::ZERO));
    }
}
