//! Correct Builder's byte-based token estimate with the prompt size the
//! server actually counted.
//!
//! The estimate (serialized bytes ÷ 2 plus per-message overhead) is
//! deliberately conservative. Typical tokenizers average three to four bytes
//! per token on code and English, so on its own the estimate runs close to
//! twice the real prompt, and budgeting with it compacts long before the
//! window is full. Each response that reports its prompt size anchors the
//! estimate: whatever the server has already counted is exact, and only
//! growth since then is estimated, still at the conservative rate.
use std::sync::{Mutex, PoisonError};

/// The last request whose prompt size the server reported. Cloning copies
/// the current anchor, so a child agent starts from the parent's.
#[derive(Debug, Default)]
pub struct TokenCalibration {
    last: Mutex<Option<Sample>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
    estimate: usize,
    reported: usize,
}

impl TokenCalibration {
    /// Record that a request Builder estimated at `estimate` tokens was
    /// counted as `reported` prompt tokens by the server.
    ///
    /// A report implying more than eight bytes per token (a quarter of the
    /// estimate) is ignored: no tokenizer is that dense, so the server must
    /// have counted only part of the prompt, such as its uncached suffix.
    /// Trusting that would overfill the window.
    pub fn observe(&self, estimate: usize, reported: usize) {
        if estimate == 0 || reported.saturating_mul(4) < estimate {
            return;
        }
        *self.lock() = Some(Sample { estimate, reported });
    }

    /// What the server would count for a request Builder estimates at
    /// `estimate` tokens. Before any report this is the estimate itself.
    pub fn tokens(&self, estimate: usize) -> usize {
        let Some(Sample {
            estimate: anchor,
            reported,
        }) = *self.lock()
        else {
            return estimate;
        };
        if estimate >= anchor {
            reported + (estimate - anchor)
        } else {
            // Content was removed (a compaction, a smaller per-round packet).
            // What remains is assumed to tokenize like the measured request.
            let scaled = (estimate as u128 * reported as u128).div_ceil(anchor as u128);
            usize::try_from(scaled).unwrap_or(usize::MAX)
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Sample>> {
        self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Clone for TokenCalibration {
    fn clone(&self) -> Self {
        Self {
            last: Mutex::new(*self.lock()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TokenCalibration;

    #[test]
    fn uncalibrated_estimates_pass_through() {
        assert_eq!(TokenCalibration::default().tokens(93_400), 93_400);
    }

    #[test]
    fn the_measured_prefix_is_exact_and_growth_stays_conservative() {
        let calibration = TokenCalibration::default();
        calibration.observe(100_000, 52_000);
        assert_eq!(calibration.tokens(100_000), 52_000);
        // 8k of new tool output is still counted at the raw estimate.
        assert_eq!(calibration.tokens(108_000), 60_000);
    }

    #[test]
    fn removed_content_scales_by_the_measured_ratio() {
        let calibration = TokenCalibration::default();
        calibration.observe(100_000, 50_000);
        assert_eq!(calibration.tokens(46_000), 23_000);
        assert_eq!(calibration.tokens(1), 1, "rounds up, never to zero");
    }

    #[test]
    fn a_server_that_counts_more_than_the_estimate_raises_it() {
        let calibration = TokenCalibration::default();
        calibration.observe(10_000, 12_000);
        assert_eq!(calibration.tokens(10_000), 12_000);
        assert_eq!(calibration.tokens(5_000), 6_000);
    }

    #[test]
    fn implausibly_small_reports_are_ignored() {
        let calibration = TokenCalibration::default();
        calibration.observe(100_000, 50_000);
        // Only the uncached suffix, for example.
        calibration.observe(120_000, 2_000);
        assert_eq!(calibration.tokens(120_000), 70_000);
        calibration.observe(0, 10);
        assert_eq!(calibration.tokens(100_000), 50_000);
    }

    #[test]
    fn clones_start_from_the_same_anchor_and_then_diverge() {
        let parent = TokenCalibration::default();
        parent.observe(100_000, 50_000);
        let child = parent.clone();
        assert_eq!(child.tokens(10_000), 5_000);
        child.observe(10_000, 4_000);
        assert_eq!(child.tokens(10_000), 4_000);
        assert_eq!(parent.tokens(10_000), 5_000);
    }
}
