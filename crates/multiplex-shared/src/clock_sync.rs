//! NTP-style clock sync math and drift correction helpers.
//!
//! Given a four-way handshake between controller and client:
//!
//! ```text
//! t1 ──sync──▶ t2
//!            ...
//! t4 ◀─reply── t3
//! ```
//!
//! the client's clock offset relative to the controller is
//! `offset = ((t2 - t1) + (t3 - t4)) / 2`.
//!
//! During playback, a rolling median of recent offsets feeds a proportional
//! rate controller that nudges playback speed to eliminate drift.

use std::collections::VecDeque;

/// Compute the one-shot clock offset from a single 4-way handshake.
///
/// All inputs are absolute milliseconds. `t1`/`t4` are controller UTC, `t2`/`t3`
/// are client-local. Returned offset is added to a client-local timestamp to
/// convert it to controller UTC.
pub fn compute_offset(t1_utc_ms: u64, t2_local_ms: u64, t3_local_ms: u64, t4_utc_ms: u64) -> i64 {
    let a = t2_local_ms as i64 - t1_utc_ms as i64;
    let b = t3_local_ms as i64 - t4_utc_ms as i64;
    (a + b) / 2
}

/// Rolling median filter over the last N offset samples.
///
/// Median rejects RTT outliers better than a mean without needing a threshold.
#[derive(Debug, Clone)]
pub struct OffsetFilter {
    window: VecDeque<i64>,
    capacity: usize,
}

impl OffsetFilter {
    pub fn new(capacity: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, offset_ms: i64) {
        if self.window.len() == self.capacity {
            self.window.pop_front();
        }
        self.window.push_back(offset_ms);
    }

    /// Median of the current window, or `None` if empty.
    pub fn median(&self) -> Option<i64> {
        if self.window.is_empty() {
            return None;
        }
        let mut buf: Vec<i64> = self.window.iter().copied().collect();
        buf.sort_unstable();
        let mid = buf.len() / 2;
        Some(if buf.len() % 2 == 0 {
            (buf[mid - 1] + buf[mid]) / 2
        } else {
            buf[mid]
        })
    }

    pub fn len(&self) -> usize {
        self.window.len()
    }

    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }
}

/// Parameters that shape drift correction behaviour, mirroring the show file's
/// `[show.sync.correction]` block.
#[derive(Debug, Clone, Copy)]
pub struct CorrectionParams {
    pub rate_min: f32,
    pub rate_max: f32,
    pub hard_seek_threshold_ms: u32,
    /// Drift magnitude at which the rate saturates at `rate_min`/`rate_max`.
    /// Below this, rate scales proportionally; above it, we hard-seek instead.
    pub saturation_ms: u32,
}

impl Default for CorrectionParams {
    fn default() -> Self {
        Self {
            rate_min: 0.95,
            rate_max: 1.05,
            hard_seek_threshold_ms: 300,
            saturation_ms: 200,
        }
    }
}

/// What the drift-correction loop should do this tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Correction {
    /// Within tolerance — leave the pipeline alone.
    Hold,
    /// Adjust playback rate to this value.
    Rate(f32),
    /// Drift is too large; hard-seek by this many ms (positive = forward).
    HardSeek(i64),
}

/// Decide how to correct a measured drift.
///
/// `drift_ms` is positive when the client is *ahead* of the master clock
/// (playing too fast). We slow down (rate < 1.0) to catch back to the master.
pub fn correction_for(drift_ms: i64, params: &CorrectionParams) -> Correction {
    let abs = drift_ms.unsigned_abs();
    if abs >= params.hard_seek_threshold_ms as u64 {
        return Correction::HardSeek(-drift_ms);
    }
    if abs < 5 {
        return Correction::Hold;
    }
    let sat = params.saturation_ms.max(1) as f32;
    let signed = (drift_ms as f32 / sat).clamp(-1.0, 1.0);
    // ahead => signed > 0 => rate < 1.0
    let rate = 1.0 - signed * ((params.rate_max - params.rate_min) / 2.0);
    Correction::Rate(rate.clamp(params.rate_min, params.rate_max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_offset_when_symmetric() {
        // Controller clock == client clock, zero RTT.
        assert_eq!(compute_offset(100, 100, 100, 100), 0);
    }

    #[test]
    fn client_ahead_gives_positive_offset() {
        // Client's clock is 50ms ahead of controller.
        // Controller sent at t1=1000, client saw at t2=1050 (client clock).
        // Client replied at t3=1060, controller saw at t4=1010.
        // Offset should be +50.
        assert_eq!(compute_offset(1000, 1050, 1060, 1010), 50);
    }

    #[test]
    fn median_filters_outliers() {
        let mut f = OffsetFilter::new(5);
        for v in [10, 12, 11, 500, 9] {
            f.push(v);
        }
        assert_eq!(f.median(), Some(11));
    }

    #[test]
    fn median_even_window() {
        let mut f = OffsetFilter::new(4);
        for v in [10, 20, 30, 40] {
            f.push(v);
        }
        assert_eq!(f.median(), Some(25));
    }

    #[test]
    fn hold_within_deadband() {
        let p = CorrectionParams::default();
        assert_eq!(correction_for(3, &p), Correction::Hold);
        assert_eq!(correction_for(-4, &p), Correction::Hold);
    }

    #[test]
    fn proportional_rate_below_threshold() {
        let p = CorrectionParams::default();
        match correction_for(100, &p) {
            Correction::Rate(r) => {
                assert!(r < 1.0 && r >= p.rate_min);
            }
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[test]
    fn hard_seek_above_threshold() {
        let p = CorrectionParams::default();
        match correction_for(500, &p) {
            Correction::HardSeek(delta) => assert_eq!(delta, -500),
            other => panic!("expected HardSeek, got {other:?}"),
        }
    }
}
