use std::time::{Duration, Instant};

/// Exponential backoff bounded by a total time window. Shared by host and
/// guest for everything that waits on the other side to come up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max_delay: Duration,
    /// Retries stop once the next delay would end past this window.
    pub window: Duration,
}

impl Backoff {
    /// Waiting for a peer that is booting or an entrypoint that is starting.
    pub const STARTUP: Self = Self {
        initial: Duration::from_millis(50),
        max_delay: Duration::from_millis(500),
        window: Duration::from_secs(10),
    };

    /// Starts a retry sequence now.
    pub fn start(self) -> Retry {
        Retry { backoff: self, deadline: Instant::now() + self.window, next: self.initial }
    }
}

/// One running retry sequence of a [`Backoff`].
#[derive(Debug, Clone)]
pub struct Retry {
    backoff: Backoff,
    deadline: Instant,
    next: Duration,
}

impl Retry {
    /// How long to sleep before the next attempt, or `None` when the window
    /// is exhausted and the last error should be returned.
    pub fn next_delay(&mut self) -> Option<Duration> {
        let delay = self.next;
        if Instant::now() + delay >= self.deadline {
            return None;
        }
        self.next = (delay * 2).min(self.backoff.max_delay);
        Some(delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_up_to_the_cap_and_stop_at_the_window() {
        let backoff = Backoff {
            initial: Duration::from_millis(10),
            max_delay: Duration::from_millis(40),
            window: Duration::from_millis(100),
        };
        let mut retry = backoff.start();
        let delays: Vec<_> = std::iter::from_fn(|| retry.next_delay()).take(10).collect();
        // The window is measured in wall time; nothing sleeps here, so only the cap shapes the delays.
        assert_eq!(&delays[..4], &[10, 20, 40, 40].map(Duration::from_millis));

        let mut expired = Backoff { window: Duration::from_millis(5), ..backoff }.start();
        assert_eq!(expired.next_delay(), None, "a delay past the window is never handed out");
    }
}
