//! Media timing helpers.

use std::time::Duration;

/// Converts monotonic capture timestamps into 90 kHz presentation timestamps (the MPEG
/// clock used by [`crate`]'s downstream TS muxer), anchored so the first sample is PTS 0.
///
/// Capture backends report a frame's time as a monotonic [`Duration`] (elapsed since some
/// fixed origin); this maps that onto the shared 90 kHz timeline, relative to the first
/// sample seen.
#[derive(Debug, Default)]
pub struct PtsClock {
    base: Option<Duration>,
}

impl PtsClock {
    /// A fresh clock that anchors on its first sample.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 90 kHz PTS for a sample captured at monotonic time `t`. The first call anchors the
    /// zero point; subsequent values are measured relative to it. Robust to a `t` that
    /// momentarily goes backwards (clamped to the anchor).
    pub fn pts(&mut self, t: Duration) -> u64 {
        let base = *self.base.get_or_insert(t);
        let rel = t.saturating_sub(base);
        // ns * 90_000 / 1_000_000_000  ==  ns * 9 / 100_000
        u64::try_from(rel.as_nanos() * 9 / 100_000).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_anchors_to_zero() {
        let mut clock = PtsClock::new();
        // First sample is PTS 0 regardless of its absolute time.
        assert_eq!(clock.pts(Duration::from_secs(5)), 0);
    }

    #[test]
    fn ninety_khz_scale() {
        let mut clock = PtsClock::new();
        clock.pts(Duration::from_secs(10)); // anchor
        assert_eq!(clock.pts(Duration::from_secs(11)), 90_000); // +1s
        assert_eq!(clock.pts(Duration::from_millis(10_500)), 45_000); // +0.5s
        assert_eq!(clock.pts(Duration::from_secs(12)), 180_000); // +2s
    }

    #[test]
    fn monotonic_nondecreasing_and_backwards_clamped() {
        let mut clock = PtsClock::new();
        clock.pts(Duration::from_secs(1));
        let a = clock.pts(Duration::from_secs(2));
        let b = clock.pts(Duration::from_secs(1)); // went backwards
        assert_eq!(a, 90_000);
        assert_eq!(
            b, 0,
            "a sample before the anchor clamps to 0, never underflows"
        );
    }
}
