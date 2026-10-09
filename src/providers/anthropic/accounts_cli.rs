//! CLI entry point for proxy-owned Claude account registrations.

use anyhow::Result;
use clap::Subcommand;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::account_usage::UsageFiles;
use super::accounts;
use crate::paths;

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
    /// List registered accounts, selection status, and observed usage
    List,
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
            for (index, (account, snapshot)) in stored.accounts.iter().zip(&snapshots).enumerate() {
                let snapshot = snapshot.as_ref();
                let active = snapshot
                    .and_then(|snapshot| snapshot["active"].as_bool())
                    .unwrap_or_else(|| legacy_active.as_deref() == Some(account.name.as_str()));
                println!(
                    "{}. {}  [{}]  5h: {}  7d: {}  7d_oi: {}  added {}",
                    index + 1,
                    account.name,
                    account_status(snapshot, active, now),
                    format_window_usage(snapshot, "5h", now),
                    format_window_usage(snapshot, "7d", now),
                    format_window_usage(snapshot, "7d_oi", now),
                    format_account_added_date(account.added_at)
                );
            }
            println!(
                "使用率は最後に取得した値です。-- は未取得、* はリセット時刻経過後の前回値です。"
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
