//! Tile/video fallback policy for dirty-region display streaming.
//!
//! D-4 introduces the decision layer but keeps it pure: callers feed
//! measured dirty fractions into [`TilePolicy::evaluate`] and get back
//! the desired [`TileMode`]. The bridge is responsible for sending
//! `FallbackToVideo` / `FallbackToTile` control frames when this mode
//! changes.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Number of recent ticks included in the rolling dirty-fraction average.
pub const HISTORY_K: usize = 8;

/// Enter video mode when the rolling average dirties at least 25% of
/// the screen. Whole-frame VP8-q is the proven fallback for this case.
pub const ENTER_VIDEO_THRESHOLD: f32 = 0.25;

/// Return to tile mode only after the rolling average drops to 15%.
/// The gap from [`ENTER_VIDEO_THRESHOLD`] prevents mode flapping.
pub const EXIT_VIDEO_THRESHOLD: f32 = 0.15;

/// Minimum time spent in a mode before another transition is allowed.
pub const MIN_DWELL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileMode {
    Tiles,
    Video,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TilePolicyConfig {
    pub history_len: usize,
    pub enter_video_threshold: f32,
    pub exit_video_threshold: f32,
    pub min_dwell: Duration,
}

impl Default for TilePolicyConfig {
    fn default() -> Self {
        Self {
            history_len: HISTORY_K,
            enter_video_threshold: ENTER_VIDEO_THRESHOLD,
            exit_video_threshold: EXIT_VIDEO_THRESHOLD,
            min_dwell: MIN_DWELL,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TilePolicy {
    config: TilePolicyConfig,
    history: VecDeque<f32>,
    state: TileMode,
    last_transition: Instant,
}

impl TilePolicy {
    pub fn new(now: Instant) -> Self {
        Self::with_config(now, TilePolicyConfig::default())
    }

    pub fn with_config(now: Instant, config: TilePolicyConfig) -> Self {
        let history_len = config.history_len.max(1);
        Self {
            config: TilePolicyConfig {
                history_len,
                ..config
            },
            history: VecDeque::with_capacity(history_len),
            state: TileMode::Tiles,
            last_transition: now,
        }
    }

    pub fn mode(&self) -> TileMode {
        self.state
    }

    pub fn rolling_dirty_fraction(&self) -> f32 {
        if self.history.is_empty() {
            return 0.0;
        }
        self.history.iter().copied().sum::<f32>() / self.history.len() as f32
    }

    /// Add one dirty-fraction sample and return the desired mode.
    ///
    /// Fractions are clamped to `[0, 1]` so callers can pass counts
    /// computed before de-duplication without poisoning the policy.
    pub fn evaluate(&mut self, dirty_fraction: f32, now: Instant) -> TileMode {
        self.push_sample(dirty_fraction);
        let avg = self.rolling_dirty_fraction();
        let dwell_ok = now.saturating_duration_since(self.last_transition) >= self.config.min_dwell;

        let next = match self.state {
            TileMode::Tiles if dwell_ok && avg >= self.config.enter_video_threshold => {
                TileMode::Video
            }
            TileMode::Video if dwell_ok && avg <= self.config.exit_video_threshold => {
                TileMode::Tiles
            }
            current => current,
        };

        if next != self.state {
            self.state = next;
            self.last_transition = now;
        }

        self.state
    }

    fn push_sample(&mut self, dirty_fraction: f32) {
        let v = dirty_fraction.clamp(0.0, 1.0);
        self.history.push_back(v);
        while self.history.len() > self.config.history_len {
            self.history.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TilePolicyConfig {
        TilePolicyConfig {
            history_len: 4,
            enter_video_threshold: 0.25,
            exit_video_threshold: 0.15,
            min_dwell: Duration::from_millis(500),
        }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn starts_in_tile_mode_with_empty_average() {
        let now = t0();
        let policy = TilePolicy::with_config(now, cfg());
        assert_eq!(policy.mode(), TileMode::Tiles);
        assert_eq!(policy.rolling_dirty_fraction(), 0.0);
    }

    #[test]
    fn stays_in_tile_mode_below_enter_threshold() {
        let now = t0();
        let mut policy = TilePolicy::with_config(now, cfg());
        let later = now + Duration::from_secs(1);
        assert_eq!(policy.evaluate(0.10, later), TileMode::Tiles);
        assert_eq!(policy.evaluate(0.20, later), TileMode::Tiles);
    }

    #[test]
    fn waits_for_min_dwell_before_entering_video() {
        let now = t0();
        let mut policy = TilePolicy::with_config(now, cfg());
        assert_eq!(
            policy.evaluate(1.0, now + Duration::from_millis(100)),
            TileMode::Tiles
        );
        assert_eq!(
            policy.evaluate(1.0, now + Duration::from_millis(500)),
            TileMode::Video
        );
    }

    #[test]
    fn enters_video_when_rolling_average_crosses_enter_threshold() {
        let now = t0();
        let mut policy = TilePolicy::with_config(now, cfg());
        let later = now + Duration::from_secs(1);
        for sample in [0.10, 0.20, 0.30] {
            policy.evaluate(sample, later);
        }
        assert_eq!(policy.mode(), TileMode::Tiles);
        assert_eq!(policy.evaluate(0.40, later), TileMode::Video);
        assert!(policy.rolling_dirty_fraction() >= 0.25);
    }

    #[test]
    fn video_mode_hysteresis_holds_through_gray_band() {
        let now = t0();
        let mut policy = TilePolicy::with_config(now, cfg());
        assert_eq!(
            policy.evaluate(1.0, now + Duration::from_secs(1)),
            TileMode::Video
        );
        let later = now + Duration::from_secs(2);
        for _ in 0..4 {
            assert_eq!(policy.evaluate(0.18, later), TileMode::Video);
        }
        assert_eq!(policy.mode(), TileMode::Video);
        assert!(policy.rolling_dirty_fraction() > cfg().exit_video_threshold);
    }

    #[test]
    fn exits_video_after_low_average_and_dwell() {
        let now = t0();
        let mut policy = TilePolicy::with_config(now, cfg());
        assert_eq!(
            policy.evaluate(1.0, now + Duration::from_secs(1)),
            TileMode::Video
        );
        for _ in 0..4 {
            policy.evaluate(0.0, now + Duration::from_millis(1200));
        }
        assert_eq!(
            policy.evaluate(0.0, now + Duration::from_millis(1500)),
            TileMode::Tiles
        );
    }

    #[test]
    fn waits_for_min_dwell_before_exiting_video() {
        let now = t0();
        let mut policy = TilePolicy::with_config(now, cfg());
        assert_eq!(
            policy.evaluate(1.0, now + Duration::from_secs(1)),
            TileMode::Video
        );
        for _ in 0..4 {
            assert_eq!(
                policy.evaluate(0.0, now + Duration::from_millis(1100)),
                TileMode::Video
            );
        }
        assert_eq!(
            policy.evaluate(0.0, now + Duration::from_millis(1500)),
            TileMode::Tiles
        );
    }

    #[test]
    fn clamps_samples_and_keeps_bounded_history() {
        let now = t0();
        let mut policy = TilePolicy::with_config(now, cfg());
        for sample in [-5.0, 0.5, 2.0, 0.0, 0.0] {
            policy.evaluate(sample, now + Duration::from_secs(1));
        }
        assert_eq!(policy.history.len(), 4);
        assert!(policy.rolling_dirty_fraction() <= 1.0);
        assert!(policy.rolling_dirty_fraction() >= 0.0);
    }
}
