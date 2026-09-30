//! Per-account snapshots of observed usage, separate from the event log.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::ratelimit::WindowUsage;

#[derive(Debug)]
pub(super) struct ObservedWindow {
    pub usage: WindowUsage,
    pub observed_at: u64,
}

impl ObservedWindow {
    pub fn snapshot(&self, now: u64, threshold: f64) -> Value {
        let reset = self.usage.reset_at_unix_secs;
        let state = if reset.is_some_and(|reset| reset <= now) {
            // Passing a reset time is not confirmation of a new allowance.
            "reset_elapsed"
        } else if self.usage.utilization >= threshold {
            "threshold_reached"
        } else {
            "below_threshold"
        };
        json!({
            "utilization": self.usage.utilization,
            "observedAt": timestamp(self.observed_at),
            "observedAtUnixSecs": self.observed_at,
            "resetAt": reset.and_then(timestamp),
            "resetAtUnixSecs": reset,
            "state": state,
        })
    }
}

pub(super) fn timestamp(seconds: u64) -> Option<String> {
    let seconds = i64::try_from(seconds).ok()?;
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

pub(super) struct UsageFiles {
    directory: PathBuf,
}

impl UsageFiles {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn write(&self, account: &str, snapshot: &Value) -> anyhow::Result<()> {
        let path = self.directory.join(filename(account));
        crate::auth::write_atomically(&path.to_string_lossy(), snapshot)
    }
}

fn filename(account: &str) -> String {
    // Encode uppercase too, so distinct names stay distinct on case-insensitive disks.
    let mut name = String::new();
    for byte in account.bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_') {
            name.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(name, "%{byte:02x}");
        }
    }
    let reserved = matches!(name.as_str(), "con" | "prn" | "aux" | "nul")
        || ((name.starts_with("com") || name.starts_with("lpt"))
            && name.len() == 4
            && matches!(name.as_bytes()[3], b'1'..=b'9'));
    if name.is_empty() || name.len() > 180 || reserved {
        name = format!("~{:x}", Sha256::digest(account.as_bytes()));
    }
    format!("{name}.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_cannot_escape_or_alias_other_accounts() {
        assert_eq!(filename("first"), "first.json");
        assert_ne!(filename("first"), filename("First").to_lowercase());
        let names = ["../first", "a/b", "a%2fb", "a\\b", "日本語", "con", "", "a"];
        let mut paths = std::collections::HashSet::new();
        for name in names.into_iter().chain(["x".repeat(300).as_str()]) {
            let name = filename(name);
            assert_eq!(Path::new(&name).components().count(), 1);
            assert!(name.len() < 255);
            assert!(paths.insert(name.to_lowercase()));
        }
    }

    #[test]
    fn usage_file_is_replaced_with_complete_json() {
        let directory = tempfile::TempDir::new().unwrap();
        let files = UsageFiles::new(directory.path().to_path_buf());
        files
            .write("first", &json!({"windows": {"5h": null}}))
            .unwrap();
        let updated = json!({"windows": {"5h": {"utilization": 0.3}}});
        files.write("first", &updated).unwrap();
        let path = directory.path().join("first.json");
        let actual: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(actual, updated);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn unrepresentable_timestamps_do_not_panic() {
        assert_eq!(timestamp(u64::MAX), None);
    }
}
