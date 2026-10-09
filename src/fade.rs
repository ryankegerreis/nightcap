//! Pure, downward-only fade interpolation.

use std::time::Duration;

use crate::settings::FadeSettings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FadePlan {
    pub start_offset: Duration,
    pub length: Duration,
}

impl FadePlan {
    pub fn new(total: Duration, settings: &FadeSettings) -> Self {
        let before = Duration::from_secs(u64::from(settings.start_before_min) * 60);
        let over = Duration::from_secs(u64::from(settings.over_min) * 60).min(before);
        let length = if total < before {
            total.mul_f64(over.as_secs_f64() / before.as_secs_f64())
        } else {
            over
        };
        Self {
            start_offset: total.saturating_sub(before),
            length,
        }
    }

    pub fn level_at(&self, elapsed: Duration, from: f32, to: f32) -> f32 {
        let from = from.clamp(0.0, 1.0);
        let to = to.clamp(0.0, 1.0);
        if from <= to || elapsed < self.start_offset {
            return from;
        }
        let progress = if self.length.is_zero() {
            1.0
        } else {
            (elapsed.saturating_sub(self.start_offset).as_secs_f64() / self.length.as_secs_f64())
                .min(1.0) as f32
        };
        from + (to - from) * progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::FadeSettings;
    use std::time::Duration;

    fn minutes(value: u64) -> Duration {
        Duration::from_secs(value * 60)
    }
    fn settings(before: u32, over: u32) -> FadeSettings {
        FadeSettings {
            enabled: true,
            start_before_min: before,
            over_min: over,
            target_percent: 10,
        }
    }
    fn close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.00001,
            "{actual} != {expected}"
        );
    }

    #[test]
    fn normal_fade_starts_then_interpolates_and_holds() {
        let plan = FadePlan::new(minutes(45), &settings(20, 15));
        assert_eq!(plan.start_offset, minutes(25));
        assert_eq!(plan.length, minutes(15));
        close(plan.level_at(minutes(24), 0.8, 0.2), 0.8);
        close(plan.level_at(minutes(25), 0.8, 0.2), 0.8);
        close(plan.level_at(Duration::from_secs(1950), 0.8, 0.2), 0.5);
        close(plan.level_at(minutes(40), 0.8, 0.2), 0.2);
        close(plan.level_at(minutes(60), 0.8, 0.2), 0.2);
    }

    #[test]
    fn short_timer_scales_fade_and_preserves_hold_proportion() {
        let plan = FadePlan::new(minutes(10), &settings(20, 15));
        assert_eq!(plan.start_offset, Duration::ZERO);
        assert_eq!(plan.length, Duration::from_secs(450));
        close(plan.level_at(Duration::from_secs(225), 1.0, 0.0), 0.5);
        close(plan.level_at(minutes(9), 1.0, 0.0), 0.0);
    }

    #[test]
    fn over_is_clamped_to_lead_time_before_scaling() {
        let plan = FadePlan::new(minutes(45), &settings(20, 30));
        assert_eq!(plan.length, minutes(20));
        let short = FadePlan::new(minutes(10), &settings(20, 30));
        assert_eq!(short.length, minutes(10));
    }

    #[test]
    fn zero_length_jumps_at_start_and_zero_lead_jumps_at_end() {
        let plan = FadePlan::new(minutes(45), &settings(20, 0));
        close(plan.level_at(minutes(24), 1.0, 0.2), 1.0);
        close(plan.level_at(minutes(25), 1.0, 0.2), 0.2);
        let end = FadePlan::new(minutes(45), &settings(0, 15));
        assert_eq!(end.length, Duration::ZERO);
        close(end.level_at(minutes(44), 1.0, 0.2), 1.0);
        close(end.level_at(minutes(45), 1.0, 0.2), 0.2);
        let zero = FadePlan::new(Duration::ZERO, &settings(20, 15));
        assert_eq!(zero.length, Duration::ZERO);
        close(zero.level_at(Duration::ZERO, 1.0, 0.2), 0.2);
    }

    #[test]
    fn never_raises_and_clamps_levels() {
        let plan = FadePlan::new(minutes(10), &settings(20, 15));
        for elapsed in [0, 60, 300, 600] {
            close(plan.level_at(Duration::from_secs(elapsed), 0.1, 0.8), 0.1);
            close(plan.level_at(Duration::from_secs(elapsed), 0.4, 0.4), 0.4);
        }
        close(plan.level_at(minutes(10), 1.5, -0.5), 0.0);
    }
}
