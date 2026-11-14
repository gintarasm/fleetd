//! Reconnect delays: 250 ms, doubled after each failure, capped at 5 s.

use std::time::Duration;

const FIRST: Duration = Duration::from_millis(250);
const MAX: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { next: FIRST }
    }
}

impl Backoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// How long to wait before the next attempt; each call doubles the
    /// following one, up to the cap.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (d * 2).min(MAX);
        d
    }

    /// After a successful connect: the next failure waits the first delay.
    pub fn reset(&mut self) {
        self.next = FIRST;
    }
}
