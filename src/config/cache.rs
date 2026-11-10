use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use super::FileConfig;
use crate::paths;

#[derive(PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl FileStamp {
    fn read(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            len: metadata.len(),
            #[cfg(unix)]
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ),
        })
    }
}

struct CachedFile {
    path: PathBuf,
    stamp: Option<FileStamp>,
    config: Option<FileConfig>,
}

// Only the current config file is retained; CLI/test directory changes cannot
// grow a process-wide cache of old configuration files.
static FILE: RwLock<Option<CachedFile>> = RwLock::new(None);

fn read_cached(directory: &Path) -> Option<FileConfig> {
    let path = directory.join("config.json");
    let mut config = None;
    for _ in 0..2 {
        let stamp = FileStamp::read(&path);
        {
            let cache = FILE.read().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = cache.as_ref()
                && cached.path == path
                && cached.stamp == stamp
            {
                return cached.config.clone();
            }
        }
        // Disk I/O and parsing never hold the shared cache lock.
        config = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        if stamp == FileStamp::read(&path) {
            *FILE.write().unwrap_or_else(|e| e.into_inner()) = Some(CachedFile {
                path,
                stamp,
                config: config.clone(),
            });
            break;
        }
    }
    config
}

pub(super) struct Snapshot {
    pub env: Arc<HashMap<String, String>>,
    pub directory: PathBuf,
    pub config: Option<FileConfig>,
    log_file: PathBuf,
}

impl Snapshot {
    fn capture(directories: paths::DirResolverEnv) -> Self {
        let directory = paths::resolve_config_dir(&directories);
        let log_file = paths::resolve_state_dir(&directories).join("proxy.log");
        let config = read_cached(&directory);
        Self {
            env: Arc::new(directories.env),
            directory,
            config,
            log_file,
        }
    }
}

tokio::task_local! {
    static REQUEST: Arc<Snapshot>;
}

pub(super) fn current() -> Option<Arc<Snapshot>> {
    REQUEST.try_with(Arc::clone).ok()
}

pub(super) fn environment() -> Arc<HashMap<String, String>> {
    current()
        .map(|s| s.env.clone())
        .unwrap_or_else(|| Arc::new(std::env::vars().collect()))
}

pub(super) fn config_dir() -> PathBuf {
    current()
        .map(|s| s.directory.clone())
        .unwrap_or_else(paths::config_dir)
}

pub(super) fn log_file() -> PathBuf {
    current()
        .map(|s| s.log_file.clone())
        .unwrap_or_else(paths::log_file)
}

pub(super) fn read(directory: &Path) -> Option<FileConfig> {
    if let Some(snapshot) = current()
        && snapshot.directory == directory
    {
        return snapshot.config.clone();
    }
    read_cached(directory)
}

/// Capture settings once for request processing. A later request observes file
/// replacement, edits, deletion, and environment changes independently.
pub async fn with_request_snapshot<F: Future>(future: F) -> F::Output {
    if current().is_some() {
        return future.await;
    }
    let snapshot = Arc::new(Snapshot::capture(paths::DirResolverEnv::default()));
    REQUEST.scope(snapshot, future).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment_for(directory: &Path) -> paths::DirResolverEnv {
        paths::DirResolverEnv {
            platform: std::env::consts::OS.into(),
            env: HashMap::from([(
                "CCP_CONFIG_DIR".into(),
                directory.to_string_lossy().into_owned(),
            )]),
            home: "/".into(),
        }
    }

    #[test]
    fn cache_observes_creation_edits_invalid_json_and_deletion() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("config.json");
        assert!(read_cached(directory.path()).is_none());
        fs::write(&path, r#"{"port":1234}"#).unwrap();
        assert_eq!(read_cached(directory.path()).unwrap().port, Some(1234));
        fs::write(&path, r#"{"port":5678}"#).unwrap();
        assert_eq!(read_cached(directory.path()).unwrap().port, Some(5678));
        fs::write(&path, "invalid").unwrap();
        assert!(read_cached(directory.path()).is_none());
        fs::write(&path, r#"{"port":9012}"#).unwrap();
        assert_eq!(read_cached(directory.path()).unwrap().port, Some(9012));
        fs::remove_file(&path).unwrap();
        assert!(read_cached(directory.path()).is_none());
    }

    #[tokio::test]
    async fn request_snapshot_is_stable_and_next_request_reloads() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("config.json");
        fs::write(&path, r#"{"port":1234,"codex":{"effort":"low"}}"#).unwrap();
        let env = environment_for(directory.path());
        REQUEST
            .scope(Arc::new(Snapshot::capture(env.clone())), async {
                assert_eq!(super::super::port(), 1234);
                assert_eq!(super::super::codex_effort().as_deref(), Some("low"));
                fs::write(&path, r#"{"port":5678,"codex":{"effort":"high"}}"#).unwrap();
                tokio::task::yield_now().await;
                assert_eq!(super::super::port(), 1234);
                assert_eq!(super::super::codex_effort().as_deref(), Some("low"));
            })
            .await;
        REQUEST
            .scope(Arc::new(Snapshot::capture(env)), async {
                assert_eq!(super::super::port(), 5678);
                assert_eq!(super::super::codex_effort().as_deref(), Some("high"));
            })
            .await;
    }

    #[tokio::test]
    async fn concurrent_requests_do_not_share_directory_or_environment() {
        let first = tempfile::TempDir::new().unwrap();
        let second = tempfile::TempDir::new().unwrap();
        fs::write(first.path().join("config.json"), r#"{"port":1234}"#).unwrap();
        fs::write(second.path().join("config.json"), r#"{"port":5678}"#).unwrap();
        let check = |directory: PathBuf, port| async move {
            REQUEST
                .scope(
                    Arc::new(Snapshot::capture(environment_for(&directory))),
                    async {
                        for _ in 0..5 {
                            tokio::task::yield_now().await;
                            assert_eq!(super::super::port(), port);
                        }
                    },
                )
                .await;
        };
        tokio::join!(
            check(first.path().into(), 1234),
            check(second.path().into(), 5678)
        );
    }
}
