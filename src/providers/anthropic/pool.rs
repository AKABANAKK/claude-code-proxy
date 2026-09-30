//! Selection of the Claude account the Anthropic passthrough sends requests with.

use std::collections::BTreeMap;

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::account_usage::{ObservedWindow, timestamp};
use super::ratelimit::{RateLimitObservation, WATCHED_WINDOWS};

const FIRST_ACCOUNT_INDEX: usize = 0;
/// How long an account stays out of rotation when the upstream gives no recovery time.
const BLOCK_WITHOUT_RESET_HEADER_SECS: u64 = 15 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolAccount {
    pub name: String,
    pub token: String,
}

#[derive(Debug)]
struct AccountState {
    account: PoolAccount,
    /// Unix time in seconds.
    blocked_until: Option<u64>,
    invalid: bool,
    windows: BTreeMap<&'static str, ObservedWindow>,
    last_response_at: Option<u64>,
    last_response_status: Option<u16>,
}

impl AccountState {
    fn new(account: PoolAccount) -> Self {
        Self {
            account,
            blocked_until: None,
            invalid: false,
            windows: BTreeMap::new(),
            last_response_at: None,
            last_response_status: None,
        }
    }

    fn is_available(&self, now_unix_secs: u64) -> bool {
        if self.invalid {
            return false;
        }
        self.blocked_until
            .is_none_or(|blocked_until| blocked_until <= now_unix_secs)
    }

    fn block_until(&mut self, until_unix_secs: u64) {
        let later = self.blocked_until.map_or(until_unix_secs, |blocked_until| {
            blocked_until.max(until_unix_secs)
        });
        self.blocked_until = Some(later);
    }

    fn usage_snapshot(&self, now: u64, threshold: f64) -> Value {
        let windows: serde_json::Map<String, Value> = WATCHED_WINDOWS
            .into_iter()
            .map(|window| {
                let value = self
                    .windows
                    .get(window)
                    .map(|observed| observed.snapshot(now, threshold))
                    .unwrap_or(Value::Null);
                (window.to_string(), value)
            })
            .collect();
        json!({
            "account": self.account.name,
            "asOf": timestamp(now),
            "asOfUnixSecs": now,
            "lastResponseAt": self.last_response_at.and_then(timestamp),
            "lastResponseStatus": self.last_response_status,
            "switchThreshold": threshold,
            "eligible": self.is_available(now),
            "invalid": self.invalid,
            "blockedUntil": self.blocked_until.and_then(timestamp),
            "blockedUntilUnixSecs": self.blocked_until,
            "windows": windows,
        })
    }
}

/// How the account chosen for a request relates to the account of the previous request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountUse {
    /// No request has used an account since the pool was built.
    First,
    Switched {
        previous: String,
    },
    Continued,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    index: usize,
    pub account: PoolAccount,
    pub account_use: AccountUse,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockReason {
    Utilization {
        window: &'static str,
        utilization: f64,
    },
    TooManyRequests,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PoolEvent {
    Blocked {
        account: String,
        reason: BlockReason,
        until_unix_secs: u64,
    },
    /// The upstream rejected the account's token, so the account is never selected again.
    Invalidated { account: String },
}

#[derive(Debug)]
pub struct AccountPool {
    accounts: Vec<AccountState>,
    active: usize,
    last_used: Option<usize>,
    threshold: f64,
}

impl AccountPool {
    pub fn new(accounts: Vec<PoolAccount>, threshold: f64, initial_active: Option<&str>) -> Self {
        let active = initial_active
            .and_then(|name| accounts.iter().position(|account| account.name == name))
            .unwrap_or(FIRST_ACCOUNT_INDEX);
        Self {
            accounts: accounts.into_iter().map(AccountState::new).collect(),
            active,
            last_used: None,
            threshold,
        }
    }

    /// Stays on the active account while it is available, otherwise advances in
    /// registration order (wrapping to the first). When no account is available, the
    /// valid account that recovers first becomes active so its upstream response can be
    /// returned as is. An empty pool or one with only invalid accounts returns `None`.
    pub fn active(&mut self, now_unix_secs: u64) -> Option<PoolAccount> {
        if self.accounts.is_empty() {
            return None;
        }
        self.active = self
            .first_available_from_active(now_unix_secs, &[])
            .or_else(|| self.earliest_recovering())?;
        Some(self.accounts[self.active].account.clone())
    }

    /// Chooses the account for the next attempt without reusing an account that already
    /// rejected this request, even if its block has expired. `None` ends retrying.
    pub fn select(&mut self, now_unix_secs: u64, tried_accounts: &[String]) -> Option<Selection> {
        let mut account = self.active(now_unix_secs)?;
        if tried_accounts.contains(&account.name) {
            self.active = self.first_available_from_active(now_unix_secs, tried_accounts)?;
            account = self.accounts[self.active].account.clone();
        }
        let account_use = self.account_use_of(self.active);
        self.last_used = Some(self.active);
        Some(Selection {
            index: self.active,
            account,
            account_use,
        })
    }

    /// Updates the selected account from the upstream answer to the request sent with it.
    pub fn observe(
        &mut self,
        selection: &Selection,
        status: StatusCode,
        observation: &RateLimitObservation,
        now_unix_secs: u64,
    ) -> Vec<PoolEvent> {
        let threshold = self.threshold;
        let account = &selection.account.name;
        let state = &mut self.accounts[selection.index];
        state.last_response_at = Some(now_unix_secs);
        state.last_response_status = Some(status.as_u16());
        for usage in &observation.windows {
            state.windows.insert(
                usage.window,
                ObservedWindow {
                    usage: usage.clone(),
                    observed_at: now_unix_secs,
                },
            );
        }
        if status == StatusCode::UNAUTHORIZED {
            state.invalid = true;
            return vec![PoolEvent::Invalidated {
                account: account.clone(),
            }];
        }
        let mut events = Vec::new();
        for usage in &observation.windows {
            if usage.utilization >= threshold {
                let until_unix_secs = usage.reset_at_unix_secs.unwrap_or_else(|| {
                    now_unix_secs.saturating_add(BLOCK_WITHOUT_RESET_HEADER_SECS)
                });
                state.block_until(until_unix_secs);
                events.push(PoolEvent::Blocked {
                    account: account.clone(),
                    reason: BlockReason::Utilization {
                        window: usage.window,
                        utilization: usage.utilization,
                    },
                    until_unix_secs,
                });
            }
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            let until_unix_secs = now_unix_secs.saturating_add(
                observation
                    .retry_after_secs
                    .unwrap_or(BLOCK_WITHOUT_RESET_HEADER_SECS),
            );
            state.block_until(until_unix_secs);
            events.push(PoolEvent::Blocked {
                account: account.clone(),
                reason: BlockReason::TooManyRequests,
                until_unix_secs,
            });
        }
        events
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    pub(super) fn accounts(&self) -> impl Iterator<Item = &PoolAccount> {
        self.accounts.iter().map(|state| &state.account)
    }

    /// Observe every registered account without changing the active account or
    /// recording a user-request selection.
    pub(super) fn observation_targets(&self) -> Vec<Selection> {
        self.accounts
            .iter()
            .enumerate()
            .map(|(index, state)| Selection {
                index,
                account: state.account.clone(),
                account_use: AccountUse::Continued,
            })
            .collect()
    }

    pub(super) fn usage_snapshots(&self, now: u64) -> impl Iterator<Item = (&str, Value)> {
        self.accounts.iter().map(move |state| {
            (
                state.account.name.as_str(),
                state.usage_snapshot(now, self.threshold),
            )
        })
    }

    pub(super) fn usage_snapshot(&self, selection: &Selection, now: u64) -> Value {
        self.accounts[selection.index].usage_snapshot(now, self.threshold)
    }

    pub(super) fn count_fields(&self, now: u64) -> serde_json::Map<String, Value> {
        let eligible = self
            .accounts
            .iter()
            .filter(|state| state.is_available(now))
            .count();
        let invalid = self.accounts.iter().filter(|state| state.invalid).count();
        serde_json::Map::from_iter([
            ("loadedAccountCount".into(), json!(self.len())),
            ("eligibleAccountCount".into(), json!(eligible)),
            (
                "blockedAccountCount".into(),
                json!(self.len() - eligible - invalid),
            ),
            ("invalidAccountCount".into(), json!(invalid)),
            (
                "allAccountsUnavailable".into(),
                json!(!self.is_empty() && eligible == 0),
            ),
        ])
    }

    fn first_available_from_active(
        &self,
        now_unix_secs: u64,
        tried_accounts: &[String],
    ) -> Option<usize> {
        let account_count = self.accounts.len();
        (0..account_count)
            .map(|offset| (self.active + offset) % account_count)
            .find(|&index| {
                let state = &self.accounts[index];
                state.is_available(now_unix_secs) && !tried_accounts.contains(&state.account.name)
            })
    }

    fn earliest_recovering(&self) -> Option<usize> {
        self.accounts
            .iter()
            .enumerate()
            .filter(|(_, state)| !state.invalid)
            .min_by_key(|(_, state)| state.blocked_until)
            .map(|(index, _)| index)
    }

    fn account_use_of(&self, index: usize) -> AccountUse {
        let Some(last_used) = self.last_used else {
            return AccountUse::First;
        };
        if last_used == index {
            return AccountUse::Continued;
        }
        AccountUse::Switched {
            previous: self.accounts[last_used].account.name.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::anthropic::ratelimit::{
        FABLE_WEEKLY_WINDOW, FIVE_HOUR_WINDOW, WEEKLY_WINDOW, WindowUsage,
    };

    const THRESHOLD: f64 = 0.95;
    const BELOW_THRESHOLD: f64 = 0.94;
    const NOW_UNIX_SECS: u64 = 1_700_000_000;
    const EARLY_RECOVERY_UNIX_SECS: u64 = NOW_UNIX_SECS + 60;
    const MIDDLE_RECOVERY_UNIX_SECS: u64 = NOW_UNIX_SECS + 120;
    const LATE_RECOVERY_UNIX_SECS: u64 = NOW_UNIX_SECS + 180;
    const RETRY_AFTER_SECS: u64 = 30;

    fn account(name: &str) -> PoolAccount {
        PoolAccount {
            name: name.to_string(),
            token: format!("sk-ant-oat-{name}"),
        }
    }

    fn pool_of(names: &[&str], initial_active: Option<&str>) -> AccountPool {
        let accounts = names.iter().map(|name| account(name)).collect();
        AccountPool::new(accounts, THRESHOLD, initial_active)
    }

    fn active_name(pool: &mut AccountPool, now_unix_secs: u64) -> Option<String> {
        pool.active(now_unix_secs).map(|account| account.name)
    }

    fn usage(
        window: &'static str,
        utilization: f64,
        reset_at_unix_secs: Option<u64>,
    ) -> RateLimitObservation {
        RateLimitObservation {
            windows: vec![WindowUsage {
                window,
                utilization,
                reset_at_unix_secs,
            }],
            retry_after_secs: None,
        }
    }

    /// Sends one request with the account the pool selects and feeds the answer back.
    fn answer(
        pool: &mut AccountPool,
        status: StatusCode,
        observation: &RateLimitObservation,
    ) -> Vec<PoolEvent> {
        let selection = pool
            .select(NOW_UNIX_SECS, &[])
            .expect("pool selects an account");
        pool.observe(&selection, status, observation, NOW_UNIX_SECS)
    }

    #[test]
    fn returns_first_account_by_default() {
        let mut pool = pool_of(&["first", "second"], None);

        assert_eq!(pool.active(NOW_UNIX_SECS), Some(account("first")));
    }

    #[test]
    fn starts_at_initial_active_account() {
        let mut pool = pool_of(&["first", "second"], Some("second"));

        assert_eq!(
            active_name(&mut pool, NOW_UNIX_SECS).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn unknown_initial_active_starts_at_first_account() {
        let mut pool = pool_of(&["first", "second"], Some("missing"));

        assert_eq!(
            active_name(&mut pool, NOW_UNIX_SECS).as_deref(),
            Some("first")
        );
    }

    #[test]
    fn empty_pool_returns_none() {
        let mut pool = pool_of(&[], None);

        assert!(pool.is_empty());
        assert_eq!(pool.active(NOW_UNIX_SECS), None);
    }

    #[test]
    fn usage_at_threshold_moves_to_next_account_and_stays() {
        let mut pool = pool_of(&["first", "second", "third"], None);

        let events = answer(
            &mut pool,
            StatusCode::OK,
            &usage(FIVE_HOUR_WINDOW, THRESHOLD, Some(EARLY_RECOVERY_UNIX_SECS)),
        );

        assert_eq!(
            events,
            vec![PoolEvent::Blocked {
                account: "first".to_string(),
                reason: BlockReason::Utilization {
                    window: FIVE_HOUR_WINDOW,
                    utilization: THRESHOLD,
                },
                until_unix_secs: EARLY_RECOVERY_UNIX_SECS,
            }]
        );
        assert_eq!(
            active_name(&mut pool, NOW_UNIX_SECS).as_deref(),
            Some("second")
        );
        assert_eq!(
            active_name(&mut pool, EARLY_RECOVERY_UNIX_SECS).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn usage_below_threshold_keeps_account() {
        let mut pool = pool_of(&["first", "second"], None);

        let events = answer(
            &mut pool,
            StatusCode::OK,
            &usage(
                WEEKLY_WINDOW,
                BELOW_THRESHOLD,
                Some(LATE_RECOVERY_UNIX_SECS),
            ),
        );

        assert!(events.is_empty());
        assert_eq!(
            active_name(&mut pool, NOW_UNIX_SECS).as_deref(),
            Some("first")
        );
    }

    #[test]
    fn fable_weekly_usage_counts_toward_threshold() {
        let mut pool = pool_of(&["first", "second"], None);

        answer(
            &mut pool,
            StatusCode::OK,
            &usage(
                FABLE_WEEKLY_WINDOW,
                THRESHOLD,
                Some(LATE_RECOVERY_UNIX_SECS),
            ),
        );

        assert_eq!(
            active_name(&mut pool, NOW_UNIX_SECS).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn usage_without_reset_blocks_for_default_period() {
        let mut pool = pool_of(&["first", "second"], None);

        let events = answer(
            &mut pool,
            StatusCode::OK,
            &usage(WEEKLY_WINDOW, THRESHOLD, None),
        );

        assert_eq!(
            events,
            vec![PoolEvent::Blocked {
                account: "first".to_string(),
                reason: BlockReason::Utilization {
                    window: WEEKLY_WINDOW,
                    utilization: THRESHOLD,
                },
                until_unix_secs: NOW_UNIX_SECS + BLOCK_WITHOUT_RESET_HEADER_SECS,
            }]
        );
    }

    #[test]
    fn too_many_requests_blocks_for_retry_after() {
        let mut pool = pool_of(&["first", "second"], None);
        let observation = RateLimitObservation {
            retry_after_secs: Some(RETRY_AFTER_SECS),
            ..Default::default()
        };

        let events = answer(&mut pool, StatusCode::TOO_MANY_REQUESTS, &observation);

        assert_eq!(
            events,
            vec![PoolEvent::Blocked {
                account: "first".to_string(),
                reason: BlockReason::TooManyRequests,
                until_unix_secs: NOW_UNIX_SECS + RETRY_AFTER_SECS,
            }]
        );
        assert_eq!(
            active_name(&mut pool, NOW_UNIX_SECS).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn too_many_requests_without_retry_after_blocks_for_default_period() {
        let mut pool = pool_of(&["first", "second"], None);

        let events = answer(
            &mut pool,
            StatusCode::TOO_MANY_REQUESTS,
            &RateLimitObservation::default(),
        );

        assert_eq!(
            events,
            vec![PoolEvent::Blocked {
                account: "first".to_string(),
                reason: BlockReason::TooManyRequests,
                until_unix_secs: NOW_UNIX_SECS + BLOCK_WITHOUT_RESET_HEADER_SECS,
            }]
        );
    }

    #[test]
    fn unauthorized_account_is_never_selected_again() {
        let mut pool = pool_of(&["first", "second"], None);

        let events = answer(
            &mut pool,
            StatusCode::UNAUTHORIZED,
            &RateLimitObservation::default(),
        );

        assert_eq!(
            events,
            vec![PoolEvent::Invalidated {
                account: "first".to_string(),
            }]
        );
        assert_eq!(
            active_name(&mut pool, LATE_RECOVERY_UNIX_SECS).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn blocked_last_account_wraps_to_first() {
        let mut pool = pool_of(&["first", "second", "third"], Some("third"));

        answer(
            &mut pool,
            StatusCode::TOO_MANY_REQUESTS,
            &RateLimitObservation::default(),
        );

        assert_eq!(
            active_name(&mut pool, NOW_UNIX_SECS).as_deref(),
            Some("first")
        );
    }

    #[test]
    fn all_blocked_selects_earliest_recovery() {
        let mut pool = pool_of(&["first", "second", "third"], None);
        for recovery_unix_secs in [
            LATE_RECOVERY_UNIX_SECS,
            EARLY_RECOVERY_UNIX_SECS,
            MIDDLE_RECOVERY_UNIX_SECS,
        ] {
            answer(
                &mut pool,
                StatusCode::OK,
                &usage(FIVE_HOUR_WINDOW, THRESHOLD, Some(recovery_unix_secs)),
            );
        }

        assert_eq!(
            active_name(&mut pool, NOW_UNIX_SECS).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn all_invalid_returns_no_account() {
        let mut pool = pool_of(&["first", "second"], Some("second"));
        for _ in 0..2 {
            answer(
                &mut pool,
                StatusCode::UNAUTHORIZED,
                &RateLimitObservation::default(),
            );
        }

        assert_eq!(pool.active(NOW_UNIX_SECS), None);
        assert_eq!(pool.select(LATE_RECOVERY_UNIX_SECS, &[]), None);
    }

    #[test]
    fn select_reports_first_use_switch_and_continuation() {
        let mut pool = pool_of(&["first", "second"], None);

        let first_use = pool.select(NOW_UNIX_SECS, &[]).unwrap();
        let continued = pool.select(NOW_UNIX_SECS, &[]).unwrap();
        pool.observe(
            &continued,
            StatusCode::TOO_MANY_REQUESTS,
            &RateLimitObservation::default(),
            NOW_UNIX_SECS,
        );
        let switched = pool.select(NOW_UNIX_SECS, &[]).unwrap();

        assert_eq!(first_use.account_use, AccountUse::First);
        assert_eq!(continued.account_use, AccountUse::Continued);
        assert_eq!(
            switched.account_use,
            AccountUse::Switched {
                previous: "first".to_string(),
            }
        );
        assert_eq!(switched.account.name, "second");
    }

    #[test]
    fn select_skips_tried_accounts_even_after_their_block_expires() {
        let mut pool = pool_of(&["first", "second"], None);

        answer(
            &mut pool,
            StatusCode::TOO_MANY_REQUESTS,
            &RateLimitObservation {
                retry_after_secs: Some(0),
                ..Default::default()
            },
        );

        let next = pool.select(NOW_UNIX_SECS, &["first".to_string()]).unwrap();

        assert_eq!(next.account.name, "second");
        assert_eq!(
            pool.select(NOW_UNIX_SECS, &["first".to_string(), "second".to_string()]),
            None
        );
    }

    #[test]
    fn shorter_blocks_do_not_shorten_the_longest_recovery_time() {
        let mut pool = pool_of(&["first", "second"], None);
        let selection = pool.select(NOW_UNIX_SECS, &[]).unwrap();
        let observation = RateLimitObservation {
            windows: vec![
                WindowUsage {
                    window: WEEKLY_WINDOW,
                    utilization: THRESHOLD,
                    reset_at_unix_secs: Some(LATE_RECOVERY_UNIX_SECS),
                },
                WindowUsage {
                    window: FIVE_HOUR_WINDOW,
                    utilization: THRESHOLD,
                    reset_at_unix_secs: Some(EARLY_RECOVERY_UNIX_SECS),
                },
            ],
            retry_after_secs: Some(RETRY_AFTER_SECS),
        };
        pool.observe(
            &selection,
            StatusCode::TOO_MANY_REQUESTS,
            &observation,
            NOW_UNIX_SECS,
        );
        // An in-flight response for the same account must not shorten its block either.
        pool.observe(
            &selection,
            StatusCode::OK,
            &usage(FIVE_HOUR_WINDOW, THRESHOLD, Some(MIDDLE_RECOVERY_UNIX_SECS)),
            NOW_UNIX_SECS,
        );
        answer(
            &mut pool,
            StatusCode::OK,
            &usage(FIVE_HOUR_WINDOW, THRESHOLD, Some(MIDDLE_RECOVERY_UNIX_SECS)),
        );

        assert!(
            pool.select(MIDDLE_RECOVERY_UNIX_SECS, &["second".to_string()])
                .is_none()
        );
        assert_eq!(
            pool.select(LATE_RECOVERY_UNIX_SECS, &["second".to_string()])
                .unwrap()
                .account
                .name,
            "first"
        );
    }

    #[test]
    fn very_large_retry_after_does_not_overflow() {
        let mut pool = pool_of(&["first", "second"], None);
        let events = answer(
            &mut pool,
            StatusCode::TOO_MANY_REQUESTS,
            &RateLimitObservation {
                retry_after_secs: Some(u64::MAX),
                ..Default::default()
            },
        );

        assert_eq!(
            events,
            vec![PoolEvent::Blocked {
                account: "first".to_string(),
                reason: BlockReason::TooManyRequests,
                until_unix_secs: u64::MAX,
            }]
        );
        assert_eq!(
            active_name(&mut pool, NOW_UNIX_SECS).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn snapshots_keep_observed_windows_when_later_headers_are_missing() {
        let mut pool = pool_of(&["first", "second"], None);
        let selection = pool.select(NOW_UNIX_SECS, &[]).unwrap();
        let initial = pool.usage_snapshot(&selection, NOW_UNIX_SECS);
        assert!(initial["windows"]["5h"].is_null());
        assert!(initial["lastResponseAt"].is_null());
        pool.observe(
            &selection,
            StatusCode::OK,
            &RateLimitObservation {
                windows: vec![
                    usage(FIVE_HOUR_WINDOW, 0.2, Some(EARLY_RECOVERY_UNIX_SECS))
                        .windows
                        .remove(0),
                    usage(WEEKLY_WINDOW, 0.4, Some(LATE_RECOVERY_UNIX_SECS))
                        .windows
                        .remove(0),
                ],
                retry_after_secs: None,
            },
            NOW_UNIX_SECS,
        );
        pool.observe(
            &selection,
            StatusCode::OK,
            &usage(FIVE_HOUR_WINDOW, 0.3, Some(EARLY_RECOVERY_UNIX_SECS)),
            NOW_UNIX_SECS + 1,
        );
        pool.observe(
            &selection,
            StatusCode::OK,
            &RateLimitObservation::default(),
            NOW_UNIX_SECS + 2,
        );
        let snapshot = pool.usage_snapshot(&selection, NOW_UNIX_SECS + 2);
        assert_eq!(snapshot["windows"]["5h"]["utilization"], 0.3);
        assert_eq!(snapshot["windows"]["7d"]["utilization"], 0.4);
        assert_eq!(
            snapshot["windows"]["7d"]["observedAtUnixSecs"],
            NOW_UNIX_SECS
        );
        assert!(snapshot["windows"]["7d_oi"].is_null());
        assert!(!snapshot.to_string().contains("sk-ant-oat"));
    }

    #[test]
    fn snapshot_distinguishes_elapsed_reset_from_observed_next_window() {
        let mut pool = pool_of(&["first"], None);
        let selection = pool.select(NOW_UNIX_SECS, &[]).unwrap();
        pool.observe(
            &selection,
            StatusCode::OK,
            &usage(FIVE_HOUR_WINDOW, 1.0, Some(EARLY_RECOVERY_UNIX_SECS)),
            NOW_UNIX_SECS,
        );
        let elapsed = pool.usage_snapshot(&selection, EARLY_RECOVERY_UNIX_SECS);
        assert_eq!(elapsed["windows"]["5h"]["state"], "reset_elapsed");
        assert_eq!(elapsed["windows"]["5h"]["utilization"], 1.0);
        assert_eq!(elapsed["eligible"], true);
        pool.observe(
            &selection,
            StatusCode::OK,
            &usage(FIVE_HOUR_WINDOW, 0.01, Some(LATE_RECOVERY_UNIX_SECS)),
            EARLY_RECOVERY_UNIX_SECS,
        );
        let recovered = pool.usage_snapshot(&selection, EARLY_RECOVERY_UNIX_SECS);
        assert_eq!(recovered["windows"]["5h"]["state"], "below_threshold");
        assert_eq!(recovered["windows"]["5h"]["utilization"], 0.01);
        assert_eq!(
            recovered["windows"]["5h"]["resetAtUnixSecs"],
            LATE_RECOVERY_UNIX_SECS
        );
    }

    #[test]
    fn counts_distinguish_blocked_invalid_and_recovered_accounts() {
        let mut pool = pool_of(&["first", "second"], None);
        answer(
            &mut pool,
            StatusCode::TOO_MANY_REQUESTS,
            &RateLimitObservation {
                retry_after_secs: Some(RETRY_AFTER_SECS),
                ..Default::default()
            },
        );
        answer(
            &mut pool,
            StatusCode::UNAUTHORIZED,
            &RateLimitObservation::default(),
        );
        let blocked = pool.count_fields(NOW_UNIX_SECS);
        assert_eq!(blocked["loadedAccountCount"], 2);
        assert_eq!(blocked["eligibleAccountCount"], 0);
        assert_eq!(blocked["blockedAccountCount"], 1);
        assert_eq!(blocked["invalidAccountCount"], 1);
        assert_eq!(blocked["allAccountsUnavailable"], true);
        let recovered = pool.count_fields(NOW_UNIX_SECS + RETRY_AFTER_SECS);
        assert_eq!(recovered["eligibleAccountCount"], 1);
        assert_eq!(recovered["allAccountsUnavailable"], false);
        assert_eq!(
            pool_of(&[], None).count_fields(NOW_UNIX_SECS)["allAccountsUnavailable"],
            false
        );
    }

    #[test]
    fn six_loaded_accounts_advance_to_third_after_second_is_blocked() {
        let mut pool = pool_of(&["first", "second", "third", "forth", "five", "six"], None);
        for _ in 0..2 {
            answer(
                &mut pool,
                StatusCode::OK,
                &usage(FIVE_HOUR_WINDOW, THRESHOLD, Some(LATE_RECOVERY_UNIX_SECS)),
            );
        }
        assert_eq!(
            pool.select(NOW_UNIX_SECS, &[]).unwrap().account.name,
            "third"
        );
        assert_eq!(pool.count_fields(NOW_UNIX_SECS)["eligibleAccountCount"], 4);
    }
}
