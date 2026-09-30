//! Settings for the fork's Claude account switching support.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use super::read_file_config;
use crate::paths;

#[derive(Deserialize, Clone)]
pub(super) struct AnthropicConfig {
    #[serde(rename = "switchThreshold")]
    pub switch_threshold: Option<f64>,
}

pub const DEFAULT_ANTHROPIC_SWITCH_THRESHOLD: f64 = 0.98;

fn normalize_switch_threshold(value: f64) -> Option<f64> {
    let ratio = if value > 1.0 { value / 100.0 } else { value };
    (ratio > 0.0 && ratio <= 1.0).then_some(ratio)
}

pub fn anthropic_switch_threshold() -> f64 {
    switch_threshold_for_env(&std::env::vars().collect(), &paths::config_dir())
}

fn switch_threshold_for_env(env: &HashMap<String, String>, config_dir: &Path) -> f64 {
    env.get("CCP_ANTHROPIC_SWITCH_THRESHOLD")
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .and_then(normalize_switch_threshold)
        .or_else(|| {
            read_file_config(config_dir)?
                .anthropic?
                .switch_threshold
                .and_then(normalize_switch_threshold)
        })
        .unwrap_or(DEFAULT_ANTHROPIC_SWITCH_THRESHOLD)
}

pub fn anthropic_active_account() -> Option<String> {
    std::env::var("CCP_ANTHROPIC_ACTIVE_ACCOUNT")
        .ok()
        .filter(|raw| !raw.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_switch_threshold_accepts_ratio_and_percent() {
        assert_eq!(normalize_switch_threshold(0.95), Some(0.95));
        assert_eq!(normalize_switch_threshold(95.0), Some(0.95));
        assert_eq!(normalize_switch_threshold(1.0), Some(1.0));
        assert_eq!(normalize_switch_threshold(100.0), Some(1.0));
    }

    #[test]
    fn normalize_switch_threshold_rejects_out_of_range_values() {
        for value in [0.0, -1.0, 101.0, f64::NAN, f64::INFINITY] {
            assert_eq!(normalize_switch_threshold(value), None);
        }
    }

    #[test]
    fn threshold_uses_env_file_and_default_in_order() {
        let config = tempfile::TempDir::new().unwrap();
        let mut env = HashMap::new();
        let resolve = |env: &HashMap<String, String>| switch_threshold_for_env(env, config.path());
        assert_eq!(resolve(&env), 0.98);

        std::fs::write(
            config.path().join("config.json"),
            r#"{"anthropic":{"switchThreshold":95}}"#,
        )
        .unwrap();
        assert_eq!(resolve(&env), 0.95);

        env.insert("CCP_ANTHROPIC_SWITCH_THRESHOLD".into(), " 0.9 ".into());
        assert_eq!(resolve(&env), 0.9);
        for value in ["invalid", "0", "101", "NaN", "inf"] {
            env.insert("CCP_ANTHROPIC_SWITCH_THRESHOLD".into(), value.into());
            assert_eq!(resolve(&env), 0.95, "{value}");
        }

        std::fs::write(
            config.path().join("config.json"),
            r#"{"anthropic":{"switchThreshold":101}}"#,
        )
        .unwrap();
        assert_eq!(resolve(&env), 0.98);
    }
}
