//! CLI entry point for proxy-owned Claude account registrations.

use anyhow::Result;
use axum::http::StatusCode;
use clap::Subcommand;
use futures_util::{StreamExt, stream};
use jiff::{Timestamp, tz::TimeZone};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_width::UnicodeWidthStr;

use super::account_relay::{USAGE_PROBE_CONCURRENCY, probe_account};
use super::account_usage::UsageFiles;
use super::pool::{AccountPool, PoolAccount};
use super::{accounts, ratelimit};
use crate::{config, paths};

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage Claude accounts used to replace the forwarded login
    Accounts {
        #[command(subcommand)]
        command: AccountsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum AccountsCommand {
    /// Register a token from `claude setup-token` read from standard input
    Add { name: String },
    /// List registered accounts, selection status, observed usage, and reset times
    List,
    /// Fetch and save usage for all registered accounts, then show the updated list
    Update,
    /// Remove a registered account
    Remove { name: String },
}

impl Command {
    pub fn run(self) -> Result<()> {
        match self {
            Self::Accounts { command } => run_accounts(command),
        }
    }
}

fn run_accounts(command: AccountsCommand) -> Result<()> {
    let store = accounts::file_store();
    match command {
        AccountsCommand::Add { name } => {
            eprintln!("Paste the token from `claude setup-token`:");
            let mut token = String::new();
            std::io::stdin().read_line(&mut token)?;
            match store.add(&name, &token) {
                Ok(account_count) => {
                    println!(
                        "Registered account {name} ({account_count} accounts) in {}",
                        store.path()
                    );
                    Ok(())
                }
                Err(err) => {
                    eprintln!("{err}");
                    std::process::exit(2);
                }
            }
        }
        AccountsCommand::List => {
            let stored = store.load()?;
            if stored.accounts.is_empty() {
                println!("No Claude accounts registered");
                return Ok(());
            }
            let usage = UsageFiles::new(paths::state_dir().join("anthropic").join("accounts"));
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            let time_zone = TimeZone::system();
            let snapshots: Vec<_> = stored
                .accounts
                .iter()
                .map(|account| match usage.read(&account.name) {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        eprintln!("Cannot load usage for {}: {err:#}", account.name);
                        None
                    }
                })
                .collect();
            let legacy_active = snapshots
                .iter()
                .any(|snapshot| {
                    snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot["active"].as_bool())
                        .is_none()
                })
                .then(|| last_selected_account(&paths::log_file(), &stored.accounts))
                .flatten();
            let threshold = config::anthropic_switch_threshold();
            let mut entries: Vec<_> = stored
                .accounts
                .iter()
                .zip(&snapshots)
                .map(|(account, snapshot)| {
                    let snapshot = snapshot.as_ref();
                    let active = snapshot
                        .and_then(|snapshot| snapshot["active"].as_bool())
                        .unwrap_or_else(|| legacy_active.as_deref() == Some(account.name.as_str()));
                    let availability = account_availability(snapshot, active, now, threshold);
                    (account, snapshot, active, availability)
                })
                .collect();
            entries.sort_by_key(|(_, _, _, availability)| *availability);
            let rows: Vec<_> = entries
                .into_iter()
                .enumerate()
                .map(|(index, (account, snapshot, active, availability))| {
                    let status = match availability {
                        AccountAvailability::ResetsAt(_) => "リセット待ち",
                        _ => account_status(snapshot, active, now),
                    };
                    [
                        format!("{}. {}", index + 1, account.name),
                        format!("[{status}]"),
                        format_account_availability(availability, &time_zone),
                        format!(
                            "5h: {} (reset {})",
                            format_window_usage(snapshot, "5h", now),
                            format_window_reset_date(snapshot, "5h", &time_zone),
                        ),
                        format!(
                            "7d: {} (reset {})",
                            format_window_usage(snapshot, "7d", now),
                            format_window_reset_date(snapshot, "7d", &time_zone),
                        ),
                        format!(
                            "7d_oi: {} (reset {})",
                            format_window_usage(snapshot, "7d_oi", now),
                            format_window_reset_date(snapshot, "7d_oi", &time_zone),
                        ),
                        format!("added {}", format_account_added_date(account.added_at)),
                    ]
                })
                .collect();
            // Terminal cell widths keep Japanese names and statuses aligned too.
            let widths: [usize; 7] = std::array::from_fn(|column| {
                rows.iter()
                    .map(|row| row[column].width())
                    .max()
                    .unwrap_or_default()
            });
            for row in rows {
                let line = row
                    .iter()
                    .zip(widths)
                    .map(|(cell, width)| format!("{cell}{}", " ".repeat(width - cell.width())))
                    .collect::<Vec<_>>()
                    .join("  ");
                println!("{}", line.trim_end());
            }
            println!(
                "使用率は最後に取得した値です。-- は未取得、* はリセット時刻経過後の前回値です。"
            );
            println!("利用可は保存情報からの目安です。日時不明のアカウントは末尾に表示します。");
            Ok(())
        }
        AccountsCommand::Update => {
            let stored = store.load()?;
            if stored.accounts.is_empty() {
                println!("No Claude accounts registered");
                return Ok(());
            }
            let account_count = stored.accounts.len();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let updated = runtime.block_on(update_account_usage(stored.accounts));
            println!("Updated usage for {updated} of {account_count} Claude accounts");
            run_accounts(AccountsCommand::List)?;
            anyhow::ensure!(
                updated == account_count,
                "Failed to update usage for {} Claude accounts",
                account_count - updated
            );
            Ok(())
        }
        AccountsCommand::Remove { name } => {
            if let Err(err) = store.remove(&name) {
                eprintln!("{err}");
                std::process::exit(2);
            }
            println!("Removed account {name}");
            Ok(())
        }
    }
}

async fn update_account_usage(accounts: Vec<accounts::StoredAnthropicAccount>) -> usize {
    let usage = UsageFiles::new(paths::state_dir().join("anthropic").join("accounts"));
    let mut pool = AccountPool::new(
        accounts
            .into_iter()
            .map(|account| PoolAccount {
                name: account.name,
                token: account.token,
            })
            .collect(),
        config::anthropic_switch_threshold(),
        None,
    );
    let base_url = config::anthropic_base_url();
    let mut probes = stream::iter(pool.observation_targets())
        .map(|selection| {
            let base_url = &base_url;
            async move {
                let response = probe_account(base_url, &selection.account.token).await;
                (selection, response)
            }
        })
        .buffer_unordered(USAGE_PROBE_CONCURRENCY);
    let mut updated = 0;
    while let Some((selection, response)) = probes.next().await {
        let name = &selection.account.name;
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                eprintln!("Cannot update usage for {name}: {}", err.without_url());
                continue;
            }
        };
        let status = response.status();
        let observation = ratelimit::observe(response.headers());
        let has_usage = !observation.windows.is_empty();
        let refreshed =
            has_usage && (status.is_success() || status == StatusCode::TOO_MANY_REQUESTS);
        if !refreshed
            && status != StatusCode::UNAUTHORIZED
            && status != StatusCode::TOO_MANY_REQUESTS
        {
            eprintln!("Cannot update usage for {name}: HTTP {status}, no usable usage response");
            continue;
        }
        let previous = match usage.read(name) {
            Ok(previous) => previous,
            Err(err) => {
                eprintln!("Cannot load previous usage for {name}: {err:#}");
                None
            }
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        pool.observe(&selection, status, &observation, now);
        if let Some(previous) = &previous {
            pool.preserve_unobserved_windows(&selection, previous, now);
        }
        let mut snapshot = pool.usage_snapshot(&selection, now);
        if let Some(active) = previous
            .as_ref()
            .and_then(|snapshot| snapshot["active"].as_bool())
        {
            snapshot["active"] = Value::Bool(active);
        } else {
            snapshot.as_object_mut().unwrap().remove("active");
        }
        if let Err(err) = usage.save(name, &snapshot) {
            eprintln!("Cannot save usage for {name}: {err:#}");
            continue;
        }
        if refreshed {
            updated += 1;
        } else {
            eprintln!("Cannot update usage for {name}: HTTP {status}");
        }
    }
    updated
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AccountAvailability {
    Now,
    ResetsAt(u64),
    Unknown,
    Invalid,
}

fn account_availability(
    snapshot: Option<&Value>,
    active: bool,
    now: u64,
    threshold: f64,
) -> AccountAvailability {
    let Some(snapshot) = snapshot else {
        return AccountAvailability::Unknown;
    };
    if snapshot["invalid"].as_bool() == Some(true) {
        return AccountAvailability::Invalid;
    }
    let threshold = snapshot["switchThreshold"]
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0 && *value <= 1.0)
        .unwrap_or(threshold);
    let blocked_until = snapshot["blockedUntilUnixSecs"].as_u64();
    let mut resets_at = None;
    let mut has_current_usage = false;
    for window in ratelimit::WATCHED_WINDOWS {
        let usage = &snapshot["windows"][window];
        let Some(utilization) = usage["utilization"]
            .as_f64()
            .filter(|value| value.is_finite() && *value >= 0.0)
        else {
            continue;
        };
        let reset = usage["resetAtUnixSecs"].as_u64();
        if reset.is_some_and(|reset| reset <= now) {
            continue;
        }
        has_current_usage = true;
        if utilization >= threshold {
            let Some(reset) = reset else {
                return AccountAvailability::Unknown;
            };
            resets_at = Some(resets_at.map_or(reset, |previous: u64| previous.max(reset)));
        }
    }
    if let Some(reset) = resets_at {
        return AccountAvailability::ResetsAt(reset.max(blocked_until.unwrap_or(0)));
    }
    // A retry deadline without exhausted-window data does not establish a quota reset.
    if blocked_until.is_some_and(|until| until > now)
        || (blocked_until.is_none() && snapshot["eligible"].as_bool() == Some(false))
    {
        return AccountAvailability::Unknown;
    }
    if active || has_current_usage {
        AccountAvailability::Now
    } else {
        AccountAvailability::Unknown
    }
}

fn format_account_availability(availability: AccountAvailability, time_zone: &TimeZone) -> String {
    let value = match availability {
        AccountAvailability::Now => "今すぐ".into(),
        AccountAvailability::ResetsAt(reset) => format_local_date(Some(reset), time_zone),
        AccountAvailability::Unknown => "不明".into(),
        AccountAvailability::Invalid => "要再認証".into(),
    };
    format!("利用可: {value}")
}

fn account_status(snapshot: Option<&Value>, active: bool, now: u64) -> &'static str {
    if let Some(snapshot) = snapshot {
        if snapshot["invalid"].as_bool() == Some(true) {
            return "認証無効";
        }
        let blocked_until = snapshot["blockedUntilUnixSecs"].as_u64();
        if blocked_until.is_some_and(|until| until > now)
            || (blocked_until.is_none() && snapshot["eligible"].as_bool() == Some(false))
        {
            return "リセット待ち";
        }
    }
    if active {
        return "稼働中";
    }
    if snapshot.is_some_and(|snapshot| {
        super::ratelimit::WATCHED_WINDOWS.into_iter().any(|window| {
            let usage = &snapshot["windows"][window];
            usage["utilization"]
                .as_f64()
                .is_some_and(|value| value.is_finite() && value >= 0.0)
                && usage["resetAtUnixSecs"]
                    .as_u64()
                    .is_none_or(|reset| reset > now)
        })
    }) {
        "残あり"
    } else {
        "未確認"
    }
}

fn format_window_usage(snapshot: Option<&Value>, window: &str, now: u64) -> String {
    let Some(usage) = snapshot.map(|snapshot| &snapshot["windows"][window]) else {
        return "--".into();
    };
    let Some(percent) = usage["utilization"]
        .as_f64()
        .map(|value| value * 100.0)
        .filter(|value| value.is_finite() && *value >= 0.0)
    else {
        return "--".into();
    };
    let stale = usage["resetAtUnixSecs"]
        .as_u64()
        .is_some_and(|reset| reset <= now);
    format!("{percent:.1}%{}", if stale { "*" } else { "" })
}

fn format_window_reset_date(
    snapshot: Option<&Value>,
    window: &str,
    time_zone: &TimeZone,
) -> String {
    format_local_date(
        snapshot.and_then(|snapshot| snapshot["windows"][window]["resetAtUnixSecs"].as_u64()),
        time_zone,
    )
}

fn format_local_date(seconds: Option<u64>, time_zone: &TimeZone) -> String {
    let reset_at = seconds
        .and_then(|seconds| i64::try_from(seconds).ok())
        .and_then(|seconds| Timestamp::from_second(seconds).ok());
    match reset_at {
        Some(reset_at) => reset_at
            .to_zoned(time_zone.clone())
            .strftime("%Y/%m/%d %H:%M")
            .to_string(),
        None => "--".into(),
    }
}

/// Older proxies only record the selected account in the event log.
fn last_selected_account(
    path: &Path,
    accounts: &[accounts::StoredAnthropicAccount],
) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut active = None;
    for line in BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
    {
        if !line.contains("anthropic_account_selected")
            && !line.contains("anthropic_accounts_refresh_started")
        {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if record["service"].as_str() != Some("anthropic") {
            continue;
        }
        match record["msg"].as_str() {
            Some("anthropic_accounts_refresh_started") => {
                if record["fields"]["loadedAccountCount"]
                    .as_u64()
                    .is_none_or(|count| count as usize == accounts.len())
                {
                    active = None;
                }
            }
            Some("anthropic_account_selected") => {
                if let Some(name) = record["fields"]["account"].as_str()
                    && accounts.iter().any(|account| account.name == name)
                {
                    active = Some(name.to_owned());
                }
            }
            _ => {}
        }
    }
    active
}

fn format_account_added_date(added_at_unix_milliseconds: u64) -> String {
    let added_at_unix_seconds = (added_at_unix_milliseconds / 1000) as i64;
    let Ok(added_at) = time::OffsetDateTime::from_unix_timestamp(added_at_unix_seconds) else {
        return added_at_unix_milliseconds.to_string();
    };
    let Ok(date_format) = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]")
    else {
        return added_at_unix_milliseconds.to_string();
    };
    added_at
        .format(&date_format)
        .unwrap_or_else(|_| added_at_unix_milliseconds.to_string())
}
