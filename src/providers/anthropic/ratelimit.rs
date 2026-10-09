//! Claude subscription usage that the Anthropic API reports in response headers.

use axum::http::HeaderMap;

const UNIFIED_HEADER_PREFIX: &str = "anthropic-ratelimit-unified-";
const UTILIZATION_HEADER_SUFFIX: &str = "-utilization";
const RESET_HEADER_SUFFIX: &str = "-reset";
const RETRY_AFTER_HEADER: &str = "retry-after";

pub const FIVE_HOUR_WINDOW: &str = "5h";
pub const WEEKLY_WINDOW: &str = "7d";
/// The weekly window of Fable.
pub const FABLE_WEEKLY_WINDOW: &str = "7d_oi";

/// Windows whose utilization is compared with the switch threshold.
pub(super) const WATCHED_WINDOWS: [&str; 3] =
    [FIVE_HOUR_WINDOW, WEEKLY_WINDOW, FABLE_WEEKLY_WINDOW];

#[derive(Debug, Clone, PartialEq)]
pub struct WindowUsage {
    pub window: &'static str,
    /// Fraction of the window's allowance used, where 1.0 is the whole allowance.
    pub utilization: f64,
    pub reset_at_unix_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RateLimitObservation {
    pub windows: Vec<WindowUsage>,
    pub retry_after_secs: Option<u64>,
}

pub fn observe(headers: &HeaderMap) -> RateLimitObservation {
    RateLimitObservation {
        windows: WATCHED_WINDOWS
            .into_iter()
            .filter_map(|window| window_usage(headers, window))
            .collect(),
        retry_after_secs: header_number(headers, RETRY_AFTER_HEADER),
    }
}

fn window_usage(headers: &HeaderMap, window: &'static str) -> Option<WindowUsage> {
    let utilization_header = format!("{UNIFIED_HEADER_PREFIX}{window}{UTILIZATION_HEADER_SUFFIX}");
    let utilization = header_text(headers, &utilization_header)?
        .parse::<f64>()
        .ok()?;
    if !utilization.is_finite() || utilization < 0.0 {
        return None;
    }
    let reset_header = format!("{UNIFIED_HEADER_PREFIX}{window}{RESET_HEADER_SUFFIX}");
    Some(WindowUsage {
        window,
        utilization,
        reset_at_unix_secs: header_number(headers, &reset_header),
    })
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

fn header_number(headers: &HeaderMap, name: &str) -> Option<u64> {
    header_text(headers, name)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const FIVE_HOUR_RESET_UNIX_SECS: u64 = 1_790_622_000;
    const WEEKLY_RESET_UNIX_SECS: u64 = 1_790_748_000;
    const RETRY_AFTER_SECS: u64 = 30;

    fn headers_of(pairs: &[(&'static str, String)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn reads_utilization_and_reset_of_watched_windows() {
        let headers = headers_of(&[
            (
                "anthropic-ratelimit-unified-5h-utilization",
                "0.95".to_string(),
            ),
            (
                "anthropic-ratelimit-unified-5h-reset",
                FIVE_HOUR_RESET_UNIX_SECS.to_string(),
            ),
            (
                "anthropic-ratelimit-unified-7d-utilization",
                "0.58".to_string(),
            ),
            (
                "anthropic-ratelimit-unified-7d-reset",
                WEEKLY_RESET_UNIX_SECS.to_string(),
            ),
            (
                "anthropic-ratelimit-unified-7d_oi-utilization",
                "0.68".to_string(),
            ),
            (
                "anthropic-ratelimit-unified-7d_oi-reset",
                WEEKLY_RESET_UNIX_SECS.to_string(),
            ),
            (
                "anthropic-ratelimit-unified-status",
                "allowed_warning".to_string(),
            ),
        ]);

        assert_eq!(
            observe(&headers),
            RateLimitObservation {
                windows: vec![
                    WindowUsage {
                        window: FIVE_HOUR_WINDOW,
                        utilization: 0.95,
                        reset_at_unix_secs: Some(FIVE_HOUR_RESET_UNIX_SECS),
                    },
                    WindowUsage {
                        window: WEEKLY_WINDOW,
                        utilization: 0.58,
                        reset_at_unix_secs: Some(WEEKLY_RESET_UNIX_SECS),
                    },
                    WindowUsage {
                        window: FABLE_WEEKLY_WINDOW,
                        utilization: 0.68,
                        reset_at_unix_secs: Some(WEEKLY_RESET_UNIX_SECS),
                    },
                ],
                retry_after_secs: None,
            }
        );
    }

    #[test]
    fn skips_windows_without_numeric_utilization() {
        let headers = headers_of(&[
            (
                "anthropic-ratelimit-unified-5h-reset",
                FIVE_HOUR_RESET_UNIX_SECS.to_string(),
            ),
            (
                "anthropic-ratelimit-unified-7d-utilization",
                "high".to_string(),
            ),
        ]);

        assert!(observe(&headers).windows.is_empty());
    }

    #[test]
    fn missing_reset_leaves_reset_time_unknown() {
        let headers = headers_of(&[(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.5".to_string(),
        )]);

        assert_eq!(
            observe(&headers).windows,
            vec![WindowUsage {
                window: FIVE_HOUR_WINDOW,
                utilization: 0.5,
                reset_at_unix_secs: None,
            }]
        );
    }

    #[test]
    fn reads_retry_after_seconds() {
        let headers = headers_of(&[("retry-after", RETRY_AFTER_SECS.to_string())]);

        let observation = observe(&headers);

        assert_eq!(observation.retry_after_secs, Some(RETRY_AFTER_SECS));
        assert!(observation.windows.is_empty());
    }

    #[test]
    fn over_limit_utilization_remains_a_fraction() {
        let headers = headers_of(&[(
            "anthropic-ratelimit-unified-5h-utilization",
            "1.25".to_string(),
        )]);

        assert_eq!(observe(&headers).windows[0].utilization, 1.25);
    }

    #[test]
    fn skips_negative_and_non_finite_utilization() {
        for value in ["-0.1", "NaN", "inf", "-inf"] {
            let headers = headers_of(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                value.to_string(),
            )]);

            assert!(observe(&headers).windows.is_empty(), "{value}");
        }
    }

    #[test]
    fn malformed_recovery_headers_leave_recovery_time_unknown() {
        let headers = headers_of(&[
            (
                "anthropic-ratelimit-unified-5h-utilization",
                "0.99".to_string(),
            ),
            (
                "anthropic-ratelimit-unified-5h-reset",
                "invalid".to_string(),
            ),
            ("retry-after", "-1".to_string()),
        ]);

        let observation = observe(&headers);

        assert_eq!(observation.windows[0].reset_at_unix_secs, None);
        assert_eq!(observation.retry_after_secs, None);
    }
}
