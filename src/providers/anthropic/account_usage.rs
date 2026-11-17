//! Per-account snapshots of observed usage, separate from the event log.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::ratelimit::{WATCHED_WINDOWS, WindowUsage};

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

pub(super) fn now_unix_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(super) fn window_revision(snapshot: &Value, window: &str) -> u64 {
    snapshot["windowUpdatedAtUnixMicros"][window]
        .as_u64()
        .or_else(|| {
            snapshot["windows"][window]["observedAtUnixSecs"]
                .as_u64()
                .map(|secs| secs.saturating_mul(1_000_000))
        })
        .or_else(|| {
            snapshot["lastResponseStatus"]
                .as_u64()
                .filter(|status| (200..300).contains(status))?;
            snapshot["lastResponseAtUnixMicros"].as_u64()
        })
        .unwrap_or(0)
}

fn merge_refresh(previous: &Value, next: &Value) -> Value {
    let previous_time = previous["lastResponseAtUnixMicros"].as_u64().unwrap_or(0);
    let next_time = next["lastResponseAtUnixMicros"].as_u64().unwrap_or(0);
    let mut merged = if next_time >= previous_time {
        next.clone()
    } else {
        previous.clone()
    };
    merged["windows"] = json!({});
    merged["windowUpdatedAtUnixMicros"] = json!({});
    for window in WATCHED_WINDOWS {
        let old = window_revision(previous, window);
        let new = window_revision(next, window);
        let (source, revision) = if new >= old {
            (next, new)
        } else {
            (previous, old)
        };
        merged["windows"][window] = source["windows"][window].clone();
        if revision > 0 {
            merged["windowUpdatedAtUnixMicros"][window] = json!(revision);
        }
    }
    merged
}

pub(super) struct UsageFiles {
    directory: PathBuf,
    writer: OnceLock<io::Result<UsageWriter>>,
}

impl UsageFiles {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            writer: OnceLock::new(),
        }
    }

    pub fn read(&self, account: &str) -> anyhow::Result<Option<Value>> {
        let path = self.directory.join(filename(account));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| format!("Cannot read {}", path.display()));
            }
        };
        let snapshot: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid account usage snapshot: {}", path.display()))?;
        anyhow::ensure!(
            snapshot["account"].as_str() == Some(account),
            "Account name does not match usage snapshot: {}",
            path.display()
        );
        Ok(Some(snapshot))
    }

    /// Explicit CLI updates report storage failures to the caller.
    pub fn save(&self, account: &str, snapshot: &Value) -> anyhow::Result<()> {
        write_snapshot(&self.directory, account, snapshot)
    }

    pub fn refresh_path(&self, account: &str) -> PathBuf {
        self.directory
            .join(".refresh")
            .join("accounts")
            .join(filename(account))
    }

    /// Separate from runtime snapshots so queued writes cannot erase CLI updates.
    pub fn publish_refresh(&self, snapshots: &[Value]) -> anyhow::Result<()> {
        let directory = self.directory.join(".refresh");
        std::fs::create_dir_all(&directory)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let publication_lock = options.open(directory.join("publish.lock"))?;
        publication_lock.lock()?;
        let mut failure = None;
        for snapshot in snapshots {
            let result = (|| -> anyhow::Result<()> {
                let account = snapshot["account"]
                    .as_str()
                    .context("Missing account in usage refresh")?;
                let previous = match std::fs::read(self.refresh_path(account)) {
                    Ok(raw) => Some(serde_json::from_slice::<Value>(&raw)?),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => None,
                    Err(err) => return Err(err.into()),
                };
                let merged = previous
                    .as_ref()
                    .map(|previous| merge_refresh(previous, snapshot));
                write_snapshot(
                    &self.directory.join(".refresh").join("accounts"),
                    account,
                    merged.as_ref().unwrap_or(snapshot),
                )?;
                Ok(())
            })();
            if let Err(err) = result {
                failure.get_or_insert(err);
            }
        }
        match failure {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Call while holding the pool lock to preserve observation order. Only the
    /// newest pending snapshot per account is retained; no disk I/O runs here.
    pub fn write(&self, account: &str, snapshot: Value) {
        let writer = self
            .writer
            .get_or_init(|| UsageWriter::start(self.directory.clone()));
        match writer {
            Ok(writer) if !writer.thread.as_ref().is_some_and(JoinHandle::is_finished) => {
                let mut queue = writer.shared.lock();
                queue.revision += 1;
                queue.pending.insert(account.to_owned(), snapshot);
                writer.shared.ready.notify_one();
            }
            Ok(_) => log_write_failure(&self.directory, Some(account), "usage writer stopped"),
            Err(err) => log_write_failure(&self.directory, Some(account), &err.to_string()),
        }
    }

    /// Wait for snapshots queued before this call, without blocking the runtime.
    /// Startup uses this before accepting requests; normal responses do not wait.
    pub async fn flush(&self) {
        let Some(Ok(writer)) = self.writer.get() else {
            return;
        };
        let target = writer.shared.lock().revision;
        let mut completed = writer.completed.clone();
        while *completed.borrow_and_update() < target {
            if completed.changed().await.is_err() {
                log_write_failure(
                    &self.directory,
                    None,
                    "usage writer stopped before flushing",
                );
                break;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn with_writer(
        directory: PathBuf,
        write: impl Fn(&str, &Value) -> anyhow::Result<()> + Send + 'static,
    ) -> Self {
        Self {
            writer: OnceLock::from(UsageWriter::spawn(directory.clone(), write)),
            directory,
        }
    }
}

#[derive(Default)]
struct PendingWrites {
    pending: BTreeMap<String, Value>,
    revision: u64,
    closing: bool,
}

#[derive(Default)]
struct SharedWriter {
    queue: Mutex<PendingWrites>,
    ready: Condvar,
}

impl SharedWriter {
    fn lock(&self) -> MutexGuard<'_, PendingWrites> {
        self.queue.lock().unwrap_or_else(|err| err.into_inner())
    }
}

struct UsageWriter {
    shared: Arc<SharedWriter>,
    completed: watch::Receiver<u64>,
    thread: Option<JoinHandle<()>>,
}

impl UsageWriter {
    fn start(directory: PathBuf) -> io::Result<Self> {
        Self::spawn(directory.clone(), move |account, snapshot| {
            write_snapshot(&directory, account, snapshot)
        })
    }

    fn spawn(
        directory: PathBuf,
        write: impl Fn(&str, &Value) -> anyhow::Result<()> + Send + 'static,
    ) -> io::Result<Self> {
        let shared = Arc::new(SharedWriter::default());
        let worker_queue = shared.clone();
        let (progress, completed) = watch::channel(0);
        let thread = thread::Builder::new()
            .name("anthropic-usage".into())
            .spawn(move || {
                loop {
                    let (pending, revision) = {
                        let mut queue = worker_queue.lock();
                        while queue.pending.is_empty() && !queue.closing {
                            queue = worker_queue
                                .ready
                                .wait(queue)
                                .unwrap_or_else(|err| err.into_inner());
                        }
                        if queue.pending.is_empty() {
                            return;
                        }
                        (std::mem::take(&mut queue.pending), queue.revision)
                    };
                    // One writer commits every batch in order. An older in-flight
                    // snapshot finishes before any newer snapshot for that account.
                    // The batch and pending map each hold at most one per account.
                    for (account, snapshot) in pending {
                        if let Err(err) = write(&account, &snapshot) {
                            log_write_failure(&directory, Some(&account), &err.to_string());
                        }
                    }
                    // Coalesced snapshots are acknowledged by the newer replacement.
                    progress.send_replace(revision);
                }
            })?;
        Ok(Self {
            shared,
            completed,
            thread: Some(thread),
        })
    }
}

impl Drop for UsageWriter {
    fn drop(&mut self) {
        self.shared.lock().closing = true;
        self.shared.ready.notify_one();
        // The worker never takes the pool lock. Drain pending snapshots even when
        // the runtime is shutting down or initialization has been cancelled.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn write_snapshot(directory: &Path, account: &str, snapshot: &Value) -> anyhow::Result<()> {
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.join(filename(account));
    let temporary = path.with_extension(format!("json.tmp-{}", uuid::Uuid::new_v4()));
    let payload = serde_json::to_vec_pretty(snapshot)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&payload)?;
        // Startup re-queries usage. Atomic replacement keeps readers consistent;
        // let the OS handle writeback instead of forcing a disk barrier here.
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn log_write_failure(directory: &Path, account: Option<&str>, error: &str) {
    crate::logging::create_logger("anthropic").warn(
        "anthropic_account_usage_write_failed",
        Some(serde_json::Map::from_iter([
            ("account".into(), json!(account)),
            ("directory".into(), json!(directory)),
            ("error".into(), json!(error)),
        ])),
    );
}

#[cfg(test)]
pub(super) struct ReleaseWriterOnDrop(pub std::sync::mpsc::Sender<()>);

#[cfg(test)]
impl Drop for ReleaseWriterOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(());
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
    fn partial_publication_retains_a_newer_window_and_confirmed_absence() {
        for fable in [
            json!({"utilization":0.26,"observedAtUnixSecs":1}),
            Value::Null,
        ] {
            let directory = tempfile::TempDir::new().unwrap();
            let files = UsageFiles::new(directory.path().to_path_buf());
            files
                .publish_refresh(&[json!({"account":"first","lastResponseStatus":200,
                "lastResponseAtUnixMicros":20,"windowUpdatedAtUnixMicros":{"7d_oi":20},
                "windows":{"7d_oi":fable}})])
                .unwrap();
            files
                .publish_refresh(&[json!({"account":"first","lastResponseStatus":429,
                "lastResponseAtUnixMicros":30,"windowUpdatedAtUnixMicros":{"7d_oi":10,"7d":30},
                "windows":{"7d":{"utilization":0.23,"observedAtUnixSecs":1},
                    "7d_oi":{"utilization":1.0,"observedAtUnixSecs":1}}})])
                .unwrap();
            let saved: Value =
                serde_json::from_slice(&std::fs::read(files.refresh_path("first")).unwrap())
                    .unwrap();
            assert_eq!(saved["lastResponseStatus"], 429);
            assert_eq!(saved["windows"]["7d_oi"], fable);
            assert_eq!(window_revision(&saved, "7d_oi"), 20);
        }
    }

    #[test]
    fn a_failed_publication_does_not_discard_other_accounts() {
        let directory = tempfile::TempDir::new().unwrap();
        let files = UsageFiles::new(directory.path().to_path_buf());
        std::fs::create_dir_all(files.refresh_path("first")).unwrap();
        assert!(
            files
                .publish_refresh(&[json!({"account":"first"}), json!({"account":"second"})])
                .is_err()
        );
        let saved: Value =
            serde_json::from_slice(&std::fs::read(files.refresh_path("second")).unwrap()).unwrap();
        assert_eq!(saved["account"], "second");
    }

    #[test]
    fn read_uses_the_encoded_filename_without_starting_a_writer() {
        let directory = tempfile::TempDir::new().unwrap();
        let account = "../Work Account";
        let snapshot = json!({"account": account, "windows": {"5h": {"utilization": 0.4}}});
        write_snapshot(directory.path(), account, &snapshot).unwrap();
        let files = UsageFiles::new(directory.path().to_path_buf());
        assert_eq!(files.read(account).unwrap(), Some(snapshot));
        assert_eq!(files.read("missing").unwrap(), None);
        assert!(files.writer.get().is_none());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

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

    #[tokio::test]
    async fn usage_file_is_replaced_with_complete_json() {
        let directory = tempfile::TempDir::new().unwrap();
        let files = UsageFiles::new(directory.path().to_path_buf());
        files.write("first", json!({"windows": {"5h": null}}));
        files.flush().await;
        let updated = json!({"windows": {"5h": {"utilization": 0.3}}});
        files.write("first", updated.clone());
        files.flush().await;
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
    fn failed_atomic_replacement_preserves_destination_and_removes_temporary_file() {
        let directory = tempfile::TempDir::new().unwrap();
        let destination = directory.path().join("first.json");
        std::fs::create_dir(&destination).unwrap();
        assert!(write_snapshot(directory.path(), "first", &json!({"revision": 1})).is_err());
        assert!(destination.is_dir());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        write_snapshot(directory.path(), "second", &json!({"revision": 2})).unwrap();
        let second: Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("second.json")).unwrap())
                .unwrap();
        assert_eq!(second["revision"], 2);
    }

    #[tokio::test]
    async fn slow_writes_coalesce_per_account_without_reordering_or_growing_the_queue() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().to_path_buf();
        let written = Arc::new(Mutex::new(Vec::new()));
        let observed = written.clone();
        let (started, writing) = tokio::sync::oneshot::channel();
        let started = Mutex::new(Some(started));
        let (release, released) = std::sync::mpsc::channel();
        let files = UsageFiles::with_writer(path.clone(), move |account, snapshot| {
            if let Some(started) = started.lock().unwrap().take() {
                let _ = started.send(());
                let _ = released.recv();
            }
            observed
                .lock()
                .unwrap()
                .push((account.to_owned(), snapshot["revision"].as_u64().unwrap()));
            crate::auth::write_atomically(&path.join(filename(account)).to_string_lossy(), snapshot)
        });
        // Release the worker even if an assertion panics, before files is dropped.
        let release = ReleaseWriterOnDrop(release);
        files.write("first", json!({"revision": 0}));
        tokio::time::timeout(std::time::Duration::from_secs(2), writing)
            .await
            .unwrap()
            .unwrap();
        for revision in 1..=1000 {
            files.write("first", json!({"revision": revision}));
            files.write("second", json!({"revision": revision * 2}));
        }
        let writer = files.writer.get().unwrap().as_ref().unwrap();
        assert_eq!(writer.shared.lock().pending.len(), 2);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), files.flush())
                .await
                .is_err(),
            "flushing must wait for the in-flight write"
        );
        drop(release);
        tokio::time::timeout(std::time::Duration::from_secs(2), files.flush())
            .await
            .unwrap();
        assert_eq!(
            *written.lock().unwrap(),
            [
                ("first".into(), 0),
                ("first".into(), 1000),
                ("second".into(), 2000)
            ]
        );
        for (account, revision) in [("first", 1000), ("second", 2000)] {
            let actual: Value = serde_json::from_slice(
                &std::fs::read(directory.path().join(filename(account))).unwrap(),
            )
            .unwrap();
            assert_eq!(actual["revision"], revision);
        }
    }

    #[test]
    fn dropping_the_writer_saves_the_latest_snapshot_for_every_account() {
        let directory = tempfile::TempDir::new().unwrap();
        let files = UsageFiles::new(directory.path().to_path_buf());
        let names = ["first", "second", "third", "fourth", "fifth", "sixth"];
        for revision in 0..100 {
            for account in names {
                files.write(account, json!({"revision": revision}));
            }
        }
        drop(files);
        for account in names {
            let actual: Value = serde_json::from_slice(
                &std::fs::read(directory.path().join(filename(account))).unwrap(),
            )
            .unwrap();
            assert_eq!(actual["revision"], 99);
        }
    }

    #[tokio::test]
    async fn failed_write_does_not_stop_other_accounts_or_flushing() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().to_path_buf();
        let files = UsageFiles::with_writer(path.clone(), move |account, snapshot| {
            anyhow::ensure!(account != "first", "injected write failure");
            crate::auth::write_atomically(&path.join(filename(account)).to_string_lossy(), snapshot)
        });
        files.write("first", json!({"revision": 1}));
        files.write("second", json!({"revision": 2}));
        tokio::time::timeout(std::time::Duration::from_secs(2), files.flush())
            .await
            .unwrap();
        assert!(!directory.path().join("first.json").exists());
        let second: Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("second.json")).unwrap())
                .unwrap();
        assert_eq!(second["revision"], 2);
    }

    #[tokio::test]
    async fn construction_and_unused_flush_leave_the_directory_untouched() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("usage");
        let files = UsageFiles::new(path.clone());
        files.flush().await;
        assert!(files.writer.get().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn unrepresentable_timestamps_do_not_panic() {
        assert_eq!(timestamp(u64::MAX), None);
    }
}
