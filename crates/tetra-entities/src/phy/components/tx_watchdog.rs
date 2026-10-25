//! Recovery from a stalled TX timeline.
//!
//! The modulator gets new slot bits only once per demodulated RX slot, so TX can stay ahead of
//! the hardware clock only while RX is close to real time. After a USB/SDR hiccup (an RX buffer
//! overrun) the driver can keep a backlog of RX samples; the PHY loop then reads at exactly the
//! real-time rate, the backlog never drains, and every TX block is produced too late and skipped
//! ("Too late to produce TX block N") — the station is on air with nothing to send, radios show
//! "No service", and only a restart helped.
//!
//! [`TxLateWatchdog`] notices that TX has produced nothing for [`RESYNC_AFTER`] while being late,
//! and asks the RX side to drop its backlog and continue from the current hardware time
//! ([`rx_catch_up_samples`]). Losing ~100 ms of uplink once is far better than a dead cell.
//! It also rate-limits the "too late" warning, whose flood (hundreds of lines per second) costs
//! the PHY thread time by itself.

use std::time::{Duration, Instant};

/// TX late for this long without producing a single block = stalled timeline.
pub const RESYNC_AFTER: Duration = Duration::from_millis(500);
/// Minimum time between two resyncs (gives the timeline time to settle).
pub const RESYNC_COOLDOWN: Duration = Duration::from_secs(2);
/// How often the aggregated "too late" warning is logged.
pub const LATE_LOG_INTERVAL: Duration = Duration::from_secs(5);
/// After a catch-up, RX continues this many blocks behind the hardware clock.
pub const CATCH_UP_MARGIN_BLOCKS: i64 = 2;

/// What the caller should do after reporting a late TX block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LateOutcome {
    /// Log a warning now (with these many late events since the previous warning).
    pub log: Option<u64>,
}

#[derive(Debug)]
pub struct TxLateWatchdog {
    /// First late event since TX last produced a block.
    late_since: Option<Instant>,
    last_resync: Option<Instant>,
    resync_requested: bool,
    next_log: Option<Instant>,
    late_since_log: u64,
    /// Resyncs done since start (for the log).
    pub resyncs: u64,
}

impl Default for TxLateWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

impl TxLateWatchdog {
    pub fn new() -> Self {
        Self { late_since: None, last_resync: None, resync_requested: false, next_log: None, late_since_log: 0, resyncs: 0 }
    }

    /// TX had to skip blocks because it was behind the hardware clock.
    pub fn on_late(&mut self, now: Instant) -> LateOutcome {
        let since = *self.late_since.get_or_insert(now);
        self.late_since_log += 1;
        let cooled = self.last_resync.is_none_or(|t| now.duration_since(t) >= RESYNC_COOLDOWN);
        if now.duration_since(since) >= RESYNC_AFTER && cooled && !self.resync_requested {
            self.resync_requested = true;
        }
        let log = match self.next_log {
            Some(t) if now < t => None,
            _ => {
                self.next_log = Some(now + LATE_LOG_INTERVAL);
                let n = self.late_since_log;
                self.late_since_log = 0;
                Some(n)
            }
        };
        LateOutcome { log }
    }

    /// TX produced a block: the timeline is healthy.
    pub fn on_produced(&mut self) {
        self.late_since = None;
    }

    /// True once per stall: the RX side should drop its backlog now.
    pub fn take_resync(&mut self, now: Instant) -> bool {
        if !self.resync_requested {
            return false;
        }
        self.resync_requested = false;
        self.late_since = None;
        self.last_resync = Some(now);
        self.resyncs += 1;
        true
    }
}

/// How many RX samples to discard so the next RX block starts `CATCH_UP_MARGIN_BLOCKS` behind the
/// hardware clock. `hw_count`: current hardware time as a sample count; `rx_next`: sample count
/// of the next sample RX will read. 0 when RX is not behind.
pub fn rx_catch_up_samples(hw_count: i64, rx_next: i64, block_size: i64) -> i64 {
    let target = hw_count - CATCH_UP_MARGIN_BLOCKS * block_size;
    (target - rx_next).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_late_block_is_not_a_stall() {
        let t0 = Instant::now();
        let mut w = TxLateWatchdog::new();
        assert_eq!(w.on_late(t0).log, Some(1), "first warning is logged at once");
        w.on_produced();
        assert!(!w.take_resync(t0 + Duration::from_secs(1)));
        // Late again much later: a new episode, still nothing to do.
        w.on_late(t0 + Duration::from_secs(10));
        assert!(!w.take_resync(t0 + Duration::from_secs(10)));
    }

    #[test]
    fn half_a_second_without_tx_requests_one_resync() {
        let t0 = Instant::now();
        let mut w = TxLateWatchdog::new();
        let mut t = t0;
        let mut logs = 0;
        // ~1.5 ms per block, 0.6 s of late blocks without any produced block.
        for _ in 0..400 {
            if w.on_late(t).log.is_some() {
                logs += 1;
            }
            t += Duration::from_micros(1500);
        }
        assert_eq!(logs, 1, "the flood is collapsed into one warning");
        assert!(w.take_resync(t), "stall detected");
        assert!(!w.take_resync(t), "only once");
        assert_eq!(w.resyncs, 1);
    }

    #[test]
    fn resyncs_are_spaced_by_the_cooldown() {
        let t0 = Instant::now();
        let mut w = TxLateWatchdog::new();
        w.on_late(t0);
        w.on_late(t0 + RESYNC_AFTER);
        assert!(w.take_resync(t0 + RESYNC_AFTER));
        // Still stalled right after the resync: wait for the cooldown.
        let t1 = t0 + RESYNC_AFTER + Duration::from_millis(10);
        w.on_late(t1);
        w.on_late(t1 + RESYNC_AFTER);
        assert!(!w.take_resync(t1 + RESYNC_AFTER), "within the cooldown");
        let t2 = t0 + RESYNC_AFTER + RESYNC_COOLDOWN + Duration::from_millis(1);
        w.on_late(t2);
        assert!(w.take_resync(t2), "after the cooldown");
    }

    #[test]
    fn warnings_carry_the_number_of_late_blocks() {
        let t0 = Instant::now();
        let mut w = TxLateWatchdog::new();
        assert_eq!(w.on_late(t0).log, Some(1));
        for i in 1..10 {
            assert_eq!(w.on_late(t0 + Duration::from_millis(i)).log, None);
        }
        assert_eq!(w.on_late(t0 + LATE_LOG_INTERVAL).log, Some(10));
    }

    #[test]
    fn catch_up_amount() {
        // RX 100 blocks of 900 behind: drop 98 blocks.
        assert_eq!(rx_catch_up_samples(1_000_000, 1_000_000 - 90_000, 900), 90_000 - 1_800);
        // Not behind (or only within the margin): nothing.
        assert_eq!(rx_catch_up_samples(1_000_000, 999_000, 900), 0);
        assert_eq!(rx_catch_up_samples(1_000_000, 1_000_500, 900), 0);
    }
}
