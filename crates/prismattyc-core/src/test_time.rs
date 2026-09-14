//! Wall-clock scale for test duration budgets (PT-254).

use std::time::Duration;

/// `PRISMATTYC_TEST_TIME_SCALE` (default 1). Values below 1 count as 1.
#[must_use]
pub fn test_time_scale() -> u32 {
    parse_test_time_scale(std::env::var("PRISMATTYC_TEST_TIME_SCALE").ok().as_deref())
}

/// Multiply a hang-check duration by [`test_time_scale`].
///
/// llvm-cov and loaded runners stretch wall time. Scale the upper bound
/// so a real stall still fails and a slow-but-live run does not.
#[must_use]
pub fn test_time_budget(base: Duration) -> Duration {
    base.saturating_mul(test_time_scale())
}

#[must_use]
pub fn parse_test_time_scale(raw: Option<&str>) -> u32 {
    raw.and_then(|s| s.parse().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_test_time_scale_defaults_and_rejects_zero() {
        assert_eq!(parse_test_time_scale(None), 1);
        assert_eq!(parse_test_time_scale(Some("")), 1);
        assert_eq!(parse_test_time_scale(Some("0")), 1);
        assert_eq!(parse_test_time_scale(Some("-1")), 1);
        assert_eq!(parse_test_time_scale(Some("4")), 4);
    }

    #[test]
    fn test_time_budget_multiplies() {
        assert_eq!(
            Duration::from_secs(1).saturating_mul(parse_test_time_scale(Some("4"))),
            Duration::from_secs(4)
        );
    }
}
