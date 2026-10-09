use crate::{config, paths};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

static STDERR_SUPPRESSION_DEPTH: AtomicUsize = AtomicUsize::new(0);

pub const REDACT_KEYS: [&str; 15] = [
    "authorization",
    "proxy-authorization",
    "access",
    "access_token",
    "refresh",
    "refresh_token",
    "id_token",
    "code",
    "code_verifier",
    "chatgpt-account-id",
    "cookie",
    "set-cookie",
    "x-api-key",
    "apikey",
    "api_key",
];

pub fn log_file() -> std::path::PathBuf {
    paths::log_file()
}

#[must_use]
pub struct StderrSuppressionGuard;

impl Drop for StderrSuppressionGuard {
    fn drop(&mut self) {
        STDERR_SUPPRESSION_DEPTH.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn suppress_stderr() -> StderrSuppressionGuard {
    STDERR_SUPPRESSION_DEPTH.fetch_add(1, Ordering::Relaxed);
    StderrSuppressionGuard
}

fn stderr_suppressed() -> bool {
    STDERR_SUPPRESSION_DEPTH.load(Ordering::Relaxed) > 0
}

fn should_mirror_to_stderr(level: &str, log_stderr: bool) -> bool {
    !stderr_suppressed() && (matches!(level, "warn" | "error") || log_stderr)
}

#[derive(Clone)]
pub struct Logger {
    service: String,
    base: serde_json::Map<String, Value>,
}

impl Logger {
    pub fn child(&self, bindings: serde_json::Map<String, Value>) -> Logger {
        let mut merged = self.base.clone();
        merged.extend(bindings);
        Logger {
            service: self.service.clone(),
            base: merged,
        }
    }

    pub fn debug(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("debug", msg, fields)
    }

    pub fn info(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("info", msg, fields)
    }

    pub fn warn(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("warn", msg, fields)
    }

    pub fn error(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("error", msg, fields)
    }

    fn emit(&self, level: &str, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        // Share one settings snapshot across the whole record, including nested fields.
        let settings = config::load_config();
        let mut body = serde_json::Map::new();
        body.insert("t".into(), Value::String(now_iso8601()));
        body.insert("level".into(), Value::String(level.to_string()));
        body.insert("service".into(), Value::String(self.service.clone()));
        body.insert("msg".into(), Value::String(msg.to_string()));

        let mut merged = self.base.clone();
        if let Some(fields) = fields {
            merged.extend(fields);
        }
        if !merged.is_empty() {
            body.insert(
                "fields".into(),
                redact_with_depth(Value::Object(merged), 0, settings.log_verbose),
            );
        }

        let line = Value::Object(body).to_string();

        let mirror_to_stderr = should_mirror_to_stderr(level, settings.log_stderr);
        if mirror_to_stderr {
            let _ = writeln!(io::stderr(), "{line}");
        }

        if write_log_line(line).is_err() && mirror_to_stderr {
            // swallow logging errors intentionally
        }
    }
}

pub fn create_logger(service: &str) -> Logger {
    Logger {
        service: service.to_string(),
        base: serde_json::Map::new(),
    }
}

fn write_log_line(line: String) -> io::Result<()> {
    let file = log_file();
    if let Some(dir) = file.parent() {
        create_dir(dir, 0o700)?;
    }

    if fs::metadata(&file).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
        rotate_file(&file)?;
    }

    append_log_line(&file, line)
}

fn append_log_line(path: &Path, mut line: String) -> io::Result<()> {
    let mut out = OpenOptions::new().create(true).append(true).open(path)?;
    // Append the record and newline together so concurrent writers cannot join lines.
    line.push('\n');
    out.write_all(line.as_bytes())
}

fn rotate_file(path: &Path) -> io::Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let rotated = path.with_extension(format!("{ts}"));
    fs::rename(path, rotated)?;
    Ok(())
}

fn create_dir(path: &Path, mode: u32) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_mode(path, mode);
    Ok(())
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(mode);
            let _ = fs::set_permissions(path, perm);
        }
    }
}

fn now_iso8601() -> String {
    let now = time::OffsetDateTime::now_utc();
    let format = time::format_description::parse_borrowed::<3>(
        "[year]-[month]-[day]T[hour]:[minute]:[second]Z",
    )
    .unwrap();
    now.format(&format).unwrap_or_else(|_| String::new())
}

pub fn redact_value(value: Value) -> Value {
    redact_with_depth(value, 0, config::log_verbose())
}

fn redact_with_depth(value: Value, depth: u8, verbose: bool) -> Value {
    if depth > 6 {
        return Value::String("[depth-limit]".into());
    }

    match value {
        Value::String(s) => {
            if verbose {
                Value::String(s)
            } else if s.len() > 4000 {
                let mut end = 4000;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                Value::String(format!("{}…[{} more]", &s[..end], s.len() - end))
            } else {
                Value::String(s)
            }
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|v| redact_with_depth(v, depth + 1, verbose))
                .collect(),
        ),
        Value::Object(fields) => {
            let mut out = serde_json::Map::new();
            for (key, value) in fields {
                if REDACT_KEYS.contains(&key.to_lowercase().as_str()) {
                    out.insert(key, redact_key_redaction(value));
                } else {
                    out.insert(key, redact_with_depth(value, depth + 1, verbose));
                }
            }
            Value::Object(out)
        }
        value => value,
    }
}

fn redact_key_redaction(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(format!("[redacted len={}]", s.len())),
        _ => Value::String("[redacted]".to_string()),
    }
}

pub fn redacted_keys() -> HashSet<&'static str> {
    REDACT_KEYS.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static STDERR_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn concurrent_appends_preserve_complete_json_lines() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("proxy.log");
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for sequence in 0..1000 {
                        append_log_line(
                            path,
                            serde_json::json!({"worker": worker, "sequence": sequence}).to_string(),
                        )
                        .unwrap();
                    }
                });
            }
        });
        let text = std::fs::read_to_string(path).unwrap();
        let mut records = HashSet::new();
        for line in text.lines() {
            let record: Value =
                serde_json::from_str(line).expect("one complete JSON record per line");
            assert!(records.insert((
                record["worker"].as_u64().unwrap(),
                record["sequence"].as_u64().unwrap(),
            )));
        }
        assert_eq!(records.len(), 8000);
    }

    #[test]
    fn stderr_suppression_disables_level_mirroring() {
        let _lock = STDERR_TEST_LOCK.lock().unwrap();
        assert!(!should_mirror_to_stderr("info", false));
        assert!(should_mirror_to_stderr("info", true));
        assert!(should_mirror_to_stderr("warn", false));

        {
            let _guard = suppress_stderr();
            assert!(!should_mirror_to_stderr("warn", false));
            assert!(!should_mirror_to_stderr("error", true));
        }

        assert!(should_mirror_to_stderr("warn", false));
    }

    #[test]
    fn stderr_suppression_supports_nested_guards() {
        let _lock = STDERR_TEST_LOCK.lock().unwrap();
        let outer = suppress_stderr();
        let inner = suppress_stderr();
        assert!(!should_mirror_to_stderr("warn", false));

        drop(inner);
        assert!(!should_mirror_to_stderr("warn", false));

        drop(outer);
        assert!(should_mirror_to_stderr("warn", false));
    }

    #[test]
    fn redacts_proxy_authorization_case_insensitively() {
        let redacted = redact_value(serde_json::json!({
            "Proxy-Authorization": "Basic dXNlcjpwYXNz",
            "safe": "kept"
        }));

        assert_eq!(redacted["safe"], "kept");
        assert_eq!(redacted["Proxy-Authorization"], "[redacted len=18]");
    }

    #[test]
    fn truncation_preserves_utf8_and_reports_omitted_bytes() {
        for (text, prefix, omitted) in [
            ("a".repeat(4200), "a".repeat(4000), 200),
            ("あ".repeat(1400), "あ".repeat(1333), 201),
            (
                format!("a{}", "🙂".repeat(1050)),
                format!("a{}", "🙂".repeat(999)),
                204,
            ),
        ] {
            assert_eq!(
                redact_with_depth(Value::String(text), 0, false),
                Value::String(format!("{prefix}…[{omitted} more]")),
            );
        }
    }

    #[test]
    fn verbosity_applies_to_nested_values_without_disabling_secret_redaction() {
        let text = "あ".repeat(1400);
        let value = serde_json::json!({"nested": [{"text": text, "Authorization": "secret"}]});
        let verbose = redact_with_depth(value.clone(), 0, true);
        let normal = redact_with_depth(value, 0, false);
        assert_eq!(verbose["nested"][0]["text"], text);
        assert_eq!(
            normal["nested"][0]["text"],
            format!("{}…[201 more]", "あ".repeat(1333))
        );
        assert_eq!(verbose["nested"][0]["Authorization"], "[redacted len=6]");
        assert_eq!(normal["nested"][0]["Authorization"], "[redacted len=6]");
    }
}
