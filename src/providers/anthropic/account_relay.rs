//! Account selection, bounded retries, and usage logging around an upstream request.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderValue, StatusCode, header::AUTHORIZATION};
use axum::response::Response;
use futures_util::{StreamExt, stream};
use serde_json::Value;

use super::account_usage::UsageFiles;
use super::pool::{AccountPool, AccountUse, BlockReason, PoolAccount, PoolEvent, Selection};
use super::{accounts, ratelimit};
use crate::anthropic::error::json_error;
use crate::logging::{Logger, create_logger};
use crate::traffic::TrafficCapture;

const STARTUP_PROBE_MODEL: &str = "claude-fable-5-1";
const STARTUP_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_PROBE_CONCURRENCY: usize = 3;

pub(super) struct AccountRelay {
    pool: Option<Mutex<AccountPool>>,
    usage_files: Option<UsageFiles>,
}

impl AccountRelay {
    pub(super) fn new() -> Self {
        Self::with_usage_directory(
            registered_account_pool(),
            crate::paths::state_dir().join("anthropic").join("accounts"),
        )
    }

    pub(super) fn with_pool(pool: Option<AccountPool>) -> Self {
        Self {
            pool: pool.map(Mutex::new),
            usage_files: None,
        }
    }

    pub(super) fn with_usage_directory(pool: Option<AccountPool>, directory: PathBuf) -> Self {
        let mut relay = Self::with_pool(pool);
        relay.usage_files = Some(UsageFiles::new(directory));
        // Read-only CLI commands construct providers too. Only server startup
        // and relayed traffic may initialize or overwrite snapshots.
        relay
    }

    pub(super) async fn initialize(&self, client: &reqwest::Client, base_url: &str) {
        let Some(pool) = &self.pool else {
            return;
        };
        let targets = {
            let pool = lock_pool(pool);
            self.save_all_usage(&pool, now_unix_secs());
            pool.observation_targets()
        };
        let log = create_logger("anthropic");
        log.info(
            "anthropic_accounts_refresh_started",
            Some(serde_json::Map::from_iter([
                (
                    "loadedAccountCount".into(),
                    serde_json::json!(targets.len()),
                ),
                ("source".into(), serde_json::json!("startup")),
                ("model".into(), serde_json::json!(STARTUP_PROBE_MODEL)),
            ])),
        );
        let mut probes = stream::iter(targets)
            .map(|selection| async move {
                let started = Instant::now();
                let response = probe_account(client, base_url, &selection.account.token).await;
                (selection, response, started.elapsed())
            })
            .buffer_unordered(STARTUP_PROBE_CONCURRENCY);
        let mut failures = 0;
        while let Some((selection, response, elapsed)) = probes.next().await {
            let mut fields = serde_json::Map::from_iter([
                ("account".into(), serde_json::json!(selection.account.name)),
                ("source".into(), serde_json::json!("startup")),
                ("ms".into(), serde_json::json!(elapsed.as_millis())),
            ]);
            match response {
                Ok(response) => {
                    let status = response.status();
                    let observation = ratelimit::observe(response.headers());
                    let (events, counts) = {
                        let mut pool = lock_pool(pool);
                        let now = now_unix_secs();
                        let events = pool.observe(&selection, status, &observation, now);
                        self.save_usage(
                            &selection.account.name,
                            pool.usage_snapshot(&selection, now),
                        );
                        (events, pool.count_fields(now))
                    };
                    fields.insert("status".into(), serde_json::json!(status.as_u16()));
                    fields.insert(
                        "observedWindowCount".into(),
                        serde_json::json!(observation.windows.len()),
                    );
                    log_pool_events(&log, "startup", &events, &counts);
                    if status.is_success() && !observation.windows.is_empty() {
                        log.info("anthropic_account_usage_refreshed", Some(fields));
                    } else {
                        failures += 1;
                        log.warn("anthropic_account_usage_refresh_failed", Some(fields));
                    }
                }
                Err(err) => {
                    failures += 1;
                    fields.insert(
                        "error".into(),
                        serde_json::json!(err.without_url().to_string()),
                    );
                    log.warn("anthropic_account_usage_refresh_failed", Some(fields));
                }
            }
        }
        if let Some(files) = &self.usage_files {
            files.flush().await;
        }
        let mut fields = lock_pool(pool).count_fields(now_unix_secs());
        fields.insert("failedAccountCount".into(), serde_json::json!(failures));
        log.info("anthropic_accounts_refresh_completed", Some(fields));
    }

    fn save_all_usage(&self, pool: &AccountPool, now: u64) {
        for (account, snapshot) in pool.usage_snapshots(now) {
            self.save_usage(account, snapshot);
        }
    }

    fn save_usage(&self, account: &str, snapshot: Value) {
        if let Some(files) = &self.usage_files {
            files.write(account, snapshot);
        }
    }

    pub(super) async fn send(
        &self,
        builder: reqwest::RequestBuilder,
        req_id: &str,
        traffic: Option<&TrafficCapture>,
    ) -> Result<reqwest::Response, Response> {
        let (client, request) = builder.build_split();
        let request = request.map_err(upstream_error)?;
        let Some(pool) = &self.pool else {
            return send_upstream(&client, request, traffic).await;
        };
        let log = create_logger("anthropic");
        let mut tried_accounts = Vec::new();
        let mut rejected = None;
        while let Some(selection) = select_account(pool, &tried_accounts) {
            let Some(mut attempt) = request.try_clone() else {
                return Err(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "anthropic account retries require a buffered request body",
                ));
            };
            let Some(authorization) = bearer_authorization(&selection.account.token) else {
                return Err(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    format!(
                        "Claude account {} has a token that is not a valid header value",
                        selection.account.name
                    ),
                ));
            };
            if let Some(rejected_account) = tried_accounts.last() {
                log_account_retry(&log, req_id, rejected_account, &selection.account.name);
            }
            if selection.account_use != AccountUse::Continued {
                let pool = lock_pool(pool);
                let now = now_unix_secs();
                self.save_all_usage(&pool, now);
                log_account_use(&log, req_id, &selection, &pool, now);
            }
            attempt.headers_mut().insert(AUTHORIZATION, authorization);
            let upstream = send_upstream(&client, attempt, traffic).await?;
            let status = upstream.status();
            let observation = ratelimit::observe(upstream.headers());
            let (events, counts) = {
                let mut pool = lock_pool(pool);
                let now = now_unix_secs();
                let events = pool.observe(&selection, status, &observation, now);
                // Enqueue under the pool lock to preserve observation order;
                // the background writer handles slow disk I/O independently.
                self.save_usage(
                    &selection.account.name,
                    pool.usage_snapshot(&selection, now),
                );
                (events, pool.count_fields(now))
            };
            log_pool_events(&log, req_id, &events, &counts);
            if status != StatusCode::TOO_MANY_REQUESTS {
                return Ok(upstream);
            }
            tried_accounts.push(selection.account.name);
            rejected = Some(upstream);
        }
        match rejected {
            Some(upstream) => Ok(upstream),
            None => send_upstream(&client, request, traffic).await,
        }
    }
}

async fn probe_account(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    // setup-token credentials support model requests. Fable responses include
    // the 7d_oi window, which an Opus-only request may omit.
    client
        .post(format!(
            "{}/v1/messages?beta=true",
            base_url.trim_end_matches('/')
        ))
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
        .header(
            "user-agent",
            concat!("claude-code-proxy/", env!("CARGO_PKG_VERSION")),
        )
        .header("x-app", "cli")
        .timeout(STARTUP_PROBE_TIMEOUT)
        .json(&serde_json::json!({
            "model": STARTUP_PROBE_MODEL,
            "max_tokens": 1,
            "stream": false,
            "system": "You are Claude Code, Anthropic's official CLI for Claude.",
            "messages": [{"role": "user", "content": "Reply only OK."}],
        }))
        .send()
        .await
}

async fn send_upstream(
    client: &reqwest::Client,
    request: reqwest::Request,
    traffic: Option<&TrafficCapture>,
) -> Result<reqwest::Response, Response> {
    let started = Instant::now();
    let upstream = client.execute(request).await.map_err(upstream_error)?;
    if let Some(traffic) = traffic {
        write_upstream_response_headers(traffic, &upstream, started.elapsed());
    }
    Ok(upstream)
}

fn registered_account_pool() -> Option<AccountPool> {
    let stored = match accounts::file_store().load() {
        Ok(stored) => stored,
        Err(err) => {
            create_logger("anthropic").warn(
                "anthropic_accounts_load_failed",
                Some(serde_json::Map::from_iter([(
                    "error".to_string(),
                    serde_json::json!(err.to_string()),
                )])),
            );
            return None;
        }
    };
    if stored.accounts.is_empty() {
        return None;
    }
    let pool_accounts = stored
        .accounts
        .into_iter()
        .map(|account| PoolAccount {
            name: account.name,
            token: account.token,
        })
        .collect();
    Some(AccountPool::new(
        pool_accounts,
        crate::config::anthropic_switch_threshold(),
        crate::config::anthropic_active_account().as_deref(),
    ))
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// `None` when the stored token cannot be sent as a header value, for example because it
/// contains a line break.
fn bearer_authorization(token: &str) -> Option<HeaderValue> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
    value.set_sensitive(true);
    Some(value)
}

fn write_upstream_response_headers(
    traffic: &TrafficCapture,
    response: &reqwest::Response,
    elapsed: Duration,
) {
    traffic.write_json(
        "030-upstream-response-headers",
        &serde_json::json!({
            "status": response.status().as_u16(),
            "elapsedMs": elapsed.as_millis(),
            "headers": headers_to_json(response.headers()),
        }),
    );
}

fn headers_to_json(headers: &axum::http::HeaderMap) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in headers {
        out.insert(
            name.to_string(),
            Value::String(value.to_str().unwrap_or("").to_string()),
        );
    }
    Value::Object(out)
}

/// Callers drop the guard within the statement so it is never held across an `.await`.
fn lock_pool(pool: &Mutex<AccountPool>) -> MutexGuard<'_, AccountPool> {
    pool.lock().expect("anthropic account pool lock")
}

fn select_account(pool: &Mutex<AccountPool>, tried_accounts: &[String]) -> Option<Selection> {
    lock_pool(pool).select(now_unix_secs(), tried_accounts)
}

fn upstream_error(err: reqwest::Error) -> Response {
    json_error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        format!("anthropic upstream request failed: {err}"),
    )
}

fn log_account_use(
    log: &Logger,
    req_id: &str,
    selection: &Selection,
    pool: &AccountPool,
    now: u64,
) {
    let registered = match accounts::file_store().load() {
        Ok(stored) => Some(stored),
        Err(err) => {
            log.warn(
                "anthropic_accounts_load_failed",
                Some(serde_json::Map::from_iter([
                    ("reqId".into(), serde_json::json!(req_id)),
                    ("error".into(), serde_json::json!(err.to_string())),
                ])),
            );
            None
        }
    };
    if let Some(fields) = selection_fields(req_id, selection, pool, registered.as_ref(), now) {
        log.info("anthropic_account_selected", Some(fields));
    }
}

fn selection_fields(
    req_id: &str,
    selection: &Selection,
    pool: &AccountPool,
    registered: Option<&accounts::StoredAnthropicAccounts>,
    now: u64,
) -> Option<serde_json::Map<String, Value>> {
    let previous_account = match &selection.account_use {
        AccountUse::Continued => return None,
        AccountUse::First => None,
        AccountUse::Switched { previous } => Some(previous.as_str()),
    };
    let mut fields = pool.count_fields(now);
    fields.extend([
        ("reqId".to_string(), serde_json::json!(req_id)),
        (
            "account".to_string(),
            serde_json::json!(selection.account.name),
        ),
        (
            "previousAccount".to_string(),
            serde_json::json!(previous_account),
        ),
        (
            "registeredAccountCount".into(),
            serde_json::json!(registered.map(|stored| stored.accounts.len())),
        ),
        (
            "registrationReloadRequired".into(),
            serde_json::json!(registered.map(|stored| {
                !pool
                    .accounts()
                    .map(|account| (&account.name, &account.token))
                    .eq(stored
                        .accounts
                        .iter()
                        .map(|account| (&account.name, &account.token)))
            })),
        ),
    ]);
    Some(fields)
}

fn log_account_retry(log: &Logger, req_id: &str, rejected_account: &str, next_account: &str) {
    log.info(
        "anthropic_account_retry",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), serde_json::json!(req_id)),
            ("account".to_string(), serde_json::json!(rejected_account)),
            ("nextAccount".to_string(), serde_json::json!(next_account)),
            (
                "status".to_string(),
                serde_json::json!(StatusCode::TOO_MANY_REQUESTS.as_u16()),
            ),
        ])),
    );
}

fn log_pool_events(
    log: &Logger,
    req_id: &str,
    events: &[PoolEvent],
    counts: &serde_json::Map<String, Value>,
) {
    for event in events {
        match event {
            PoolEvent::Blocked {
                account,
                reason,
                until_unix_secs,
            } => {
                let mut fields = serde_json::Map::from_iter([
                    ("reqId".to_string(), serde_json::json!(req_id)),
                    ("account".to_string(), serde_json::json!(account)),
                    (
                        "untilUnixSecs".to_string(),
                        serde_json::json!(until_unix_secs),
                    ),
                ]);
                match reason {
                    BlockReason::Utilization {
                        window,
                        utilization,
                    } => {
                        fields.insert("window".to_string(), serde_json::json!(window));
                        fields.insert("utilization".to_string(), serde_json::json!(utilization));
                    }
                    BlockReason::TooManyRequests => {
                        fields.insert(
                            "status".to_string(),
                            serde_json::json!(StatusCode::TOO_MANY_REQUESTS.as_u16()),
                        );
                    }
                }
                fields.extend(counts.clone());
                log.info("anthropic_account_blocked", Some(fields));
            }
            PoolEvent::Invalidated { account } => {
                let mut fields = counts.clone();
                fields.extend([
                    ("reqId".to_string(), serde_json::json!(req_id)),
                    ("account".to_string(), serde_json::json!(account)),
                    (
                        "status".to_string(),
                        serde_json::json!(StatusCode::UNAUTHORIZED.as_u16()),
                    ),
                ]);
                log.warn("anthropic_account_invalid", Some(fields));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn slow_usage_disk_does_not_delay_failover_or_streaming() {
        use super::super::account_usage::ReleaseWriterOnDrop;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().to_path_buf();
        let (started, writing) = tokio::sync::oneshot::channel();
        let started = Mutex::new(Some(started));
        let (release, released) = std::sync::mpsc::channel();
        let files = UsageFiles::with_writer(path.clone(), move |account, snapshot| {
            if let Some(started) = started.lock().unwrap().take() {
                let _ = started.send(());
                let _ = released.recv();
            }
            crate::auth::write_atomically(
                &path.join(format!("{account}.json")).to_string_lossy(),
                snapshot,
            )
        });
        let relay = AccountRelay {
            pool: Some(Mutex::new(AccountPool::new(
                ["test-first", "test-second"]
                    .into_iter()
                    .map(|name| PoolAccount {
                        name: name.into(),
                        token: format!("token-{name}"),
                    })
                    .collect(),
                0.98,
                None,
            ))),
            usage_files: Some(files),
        };
        let release = ReleaseWriterOnDrop(release);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let mock = tokio::spawn(async move {
            let mut writing = Some(writing);
            for account in ["test-first", "test-second"] {
                let (mut connection, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let length = connection.read(&mut chunk).await.unwrap();
                    assert!(length > 0);
                    request.extend_from_slice(&chunk[..length]);
                }
                assert!(
                    String::from_utf8(request)
                        .unwrap()
                        .contains(&format!("authorization: Bearer token-{account}"))
                );
                if let Some(writing) = writing.take() {
                    writing.await.unwrap();
                }
                let response: &[u8] = if account == "test-first" {
                    b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"
                } else {
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: ping\n\n"
                };
                connection.write_all(response).await.unwrap();
            }
        });
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            relay.send(reqwest::Client::new().get(url), "req-slow-writer", None),
        )
        .await
        .expect("a blocked disk writer must not block the runtime or account pool")
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(response.bytes().await.unwrap(), "data: ping\n\n");
        mock.await.unwrap();
        drop(release);
        relay.usage_files.as_ref().unwrap().flush().await;
        for (account, status, eligible) in [("test-first", 429, false), ("test-second", 200, true)]
        {
            let snapshot: Value = serde_json::from_slice(
                &std::fs::read(directory.path().join(format!("{account}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(snapshot["lastResponseStatus"], status);
            assert_eq!(snapshot["eligible"], eligible);
        }
    }

    #[test]
    fn selection_log_distinguishes_six_registered_from_two_loaded_accounts() {
        let stored = accounts::StoredAnthropicAccounts {
            accounts: ["first", "second", "third", "forth", "five", "six"]
                .into_iter()
                .map(|name| accounts::StoredAnthropicAccount {
                    name: name.into(),
                    token: format!("secret-{name}"),
                    added_at: 0,
                })
                .collect(),
        };
        let mut pool = AccountPool::new(
            stored.accounts[..2]
                .iter()
                .map(|account| PoolAccount {
                    name: account.name.clone(),
                    token: account.token.clone(),
                })
                .collect(),
            0.98,
            None,
        );
        let selection = pool.select(100, &[]).unwrap();
        let fields = selection_fields("request", &selection, &pool, Some(&stored), 100).unwrap();
        assert_eq!(fields["registeredAccountCount"], 6);
        assert_eq!(fields["loadedAccountCount"], 2);
        assert_eq!(fields["registrationReloadRequired"], true);
        assert!(!serde_json::to_string(&fields).unwrap().contains("secret-"));

        let mut matching = stored.clone();
        matching.accounts.truncate(2);
        let fields = selection_fields("request", &selection, &pool, Some(&matching), 100).unwrap();
        assert_eq!(fields["registrationReloadRequired"], false);
        matching.accounts[0].token = "replacement".into();
        let fields = selection_fields("request", &selection, &pool, Some(&matching), 100).unwrap();
        assert_eq!(fields["registrationReloadRequired"], true);

        let fields = selection_fields("request", &selection, &pool, None, 100).unwrap();
        assert!(fields["registeredAccountCount"].is_null());
        assert!(fields["registrationReloadRequired"].is_null());
    }
}
