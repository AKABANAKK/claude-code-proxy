use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;

#[test]
fn help_exposes_account_commands() {
    Command::cargo_bin("claude-code-proxy")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains(
            "Manage Claude accounts for the Anthropic passthrough",
        ));
    Command::cargo_bin("claude-code-proxy")
        .unwrap()
        .args(["anthropic", "accounts", "--help"])
        .assert()
        .success()
        .stdout(contains("add"))
        .stdout(contains("list"))
        .stdout(contains("update"))
        .stdout(contains("remove"));
    Command::cargo_bin("claude-code-proxy")
        .unwrap()
        .args(["anthropic", "accounts", "list", "--help"])
        .assert()
        .success()
        .stdout(contains("[COLUMN:asc|desc]"))
        .stdout(contains("registration order"))
        .stdout(contains(
            "name, status, available_at, 5h, 7d, 7d_oi, added_at",
        ));
}

#[test]
fn corrupt_accounts_file_is_not_treated_as_empty_or_overwritten() {
    let config = TempDir::new().unwrap();
    let directory = config.path().join("anthropic");
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("accounts.json");
    for content in [r#"{"accounts":["#, r#"{"accounts":"invalid"}"#] {
        std::fs::write(&file, content).unwrap();

        for args in [
            ["list"].as_slice(),
            ["update"].as_slice(),
            ["add", "new"].as_slice(),
        ] {
            Command::cargo_bin("claude-code-proxy")
                .unwrap()
                .env("CCP_CONFIG_DIR", config.path())
                .args(["anthropic", "accounts"])
                .args(args)
                .write_stdin("sk-ant-oat-test-new\n")
                .assert()
                .failure()
                .stderr(contains("accounts.json"));
            assert_eq!(std::fs::read_to_string(&file).unwrap(), content);
        }
    }
}

fn anthropic_accounts_command(
    config_dir: &TempDir,
    args: &[&str],
) -> Result<Command, Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["anthropic", "accounts"]);
    cmd.args(args);
    cmd.env("CCP_CONFIG_DIR", config_dir.path());
    cmd.env("XDG_STATE_HOME", config_dir.path());
    cmd.env("LOCALAPPDATA", config_dir.path());
    Ok(cmd)
}

fn register_list_fixture(config: &TempDir, names: &[&str]) {
    let directory = config.path().join("anthropic");
    std::fs::create_dir_all(&directory).unwrap();
    let accounts: Vec<_> = names
        .iter()
        .map(|name| serde_json::json!({"name": name, "token": "private-test-token", "addedAt": 0}))
        .collect();
    std::fs::write(
        directory.join("accounts.json"),
        serde_json::to_vec(&serde_json::json!({"accounts": accounts})).unwrap(),
    )
    .unwrap();
}

fn list_usage_directory(config: &TempDir) -> std::path::PathBuf {
    config.path().join("claude-code-proxy/anthropic/accounts")
}

fn write_list_usage(config: &TempDir, name: &str, snapshot: serde_json::Value) {
    let directory = list_usage_directory(config);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(format!("{name}.json")),
        serde_json::to_vec(&snapshot).unwrap(),
    )
    .unwrap();
}

fn list_output(config: &TempDir) -> String {
    let output = anthropic_accounts_command(config, &["list"])
        .unwrap()
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("private-test-token"));
    stdout
}

fn listed_account(output: &str, name: &str) -> String {
    output
        .lines()
        .find(|line| {
            line.split_once(". ")
                .and_then(|(_, account)| account.strip_prefix(name))
                .is_some_and(|remaining| remaining.trim_start().starts_with('['))
        })
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn listed_account_names(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter_map(|line| line.split_once(". "))
        .map(|(_, row)| row.split_whitespace().next().unwrap())
        .collect()
}

fn listed_cell<'a>(output: &'a str, name: &str, column: &str) -> &'a str {
    let header = output
        .lines()
        .find(|line| line.starts_with("name "))
        .unwrap();
    let index = header
        .split_whitespace()
        .position(|name| name == column)
        .unwrap();
    let row = output
        .lines()
        .find(|line| {
            line.split_once(". ")
                .and_then(|(_, account)| account.strip_prefix(name))
                .is_some_and(|remaining| remaining.trim_start().starts_with('['))
        })
        .unwrap();
    row.split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .nth(index)
        .unwrap()
}

#[test]
fn list_rejects_an_unsupported_sort_order() {
    let config = TempDir::new().unwrap();
    for sort in [
        "name",
        "name:invalid",
        "missing:asc",
        "name:asc:desc",
        ":asc",
    ] {
        anthropic_accounts_command(&config, &["list", sort])
            .unwrap()
            .assert()
            .code(2)
            .stderr(contains("invalid value"))
            .stderr(contains("<column>:asc or <column>:desc"))
            .stderr(contains(
                "name, status, available_at, 5h, 7d, 7d_oi, added_at",
            ));
    }
    assert!(!config.path().join("anthropic/accounts.json").exists());
    assert!(!list_usage_directory(&config).exists());
}

#[test]
fn list_sorts_all_seven_displayed_columns_in_both_directions() {
    let config = TempDir::new().unwrap();
    let names = ["bravo", "unknown", "alpha", "tie", "charlie", "invalid"];
    register_list_fixture(&config, &names);
    let registry = config.path().join("anthropic/accounts.json");
    let mut registrations: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
    for (account, added_at) in registrations["accounts"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip([5000, 2000, 6000, 6000, 1000, 4000])
    {
        account["addedAt"] = serde_json::json!(added_at);
    }
    std::fs::write(&registry, serde_json::to_vec(&registrations).unwrap()).unwrap();
    let registrations = std::fs::read(&registry).unwrap();
    let reset = 4102444800_u64;
    for (name, active, five_hour, weekly, weekly_oi, delay) in [
        ("bravo", false, 1.05, 0.2, 0.7, 300),
        ("alpha", true, 0.09, 0.8, 0.2, 0),
        ("tie", false, 0.09, 0.8, 0.2, 0),
        ("charlie", false, 1.0, 0.1, 0.9, 100),
    ] {
        write_list_usage(
            &config,
            name,
            serde_json::json!({
                "account": name, "active": active, "eligible": five_hour < 0.98,
                "windows": {
                    "5h": {"utilization": five_hour, "resetAtUnixSecs": reset + delay},
                    "7d": {"utilization": weekly, "resetAtUnixSecs": reset + delay},
                    "7d_oi": {"utilization": weekly_oi, "resetAtUnixSecs": reset + delay}
                }
            }),
        );
    }
    write_list_usage(
        &config,
        "invalid",
        serde_json::json!({
            "account": "invalid", "invalid": true,
            "windows": {"5h": {"utilization": -1}, "7d": {"utilization": "bad"}}
        }),
    );
    let default_output = list_output(&config);
    assert_eq!(listed_account_names(&default_output), names);
    assert_eq!(
        default_output
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>(),
        [
            "name",
            "status",
            "available_at",
            "5h",
            "7d",
            "7d_oi",
            "added_at"
        ]
    );
    for (sort, expected) in [
        (
            "name:asc",
            ["alpha", "bravo", "charlie", "invalid", "tie", "unknown"],
        ),
        (
            "name:desc",
            ["unknown", "tie", "invalid", "charlie", "bravo", "alpha"],
        ),
        (
            "status:asc",
            ["bravo", "charlie", "unknown", "tie", "alpha", "invalid"],
        ),
        (
            "status:desc",
            ["invalid", "alpha", "tie", "unknown", "bravo", "charlie"],
        ),
        (
            "available_at:asc",
            ["alpha", "tie", "charlie", "bravo", "unknown", "invalid"],
        ),
        (
            "available_at:desc",
            ["bravo", "charlie", "alpha", "tie", "unknown", "invalid"],
        ),
        (
            "5h:asc",
            ["alpha", "tie", "charlie", "bravo", "unknown", "invalid"],
        ),
        (
            "5h:desc",
            ["bravo", "charlie", "alpha", "tie", "unknown", "invalid"],
        ),
        (
            "7d:asc",
            ["charlie", "bravo", "alpha", "tie", "unknown", "invalid"],
        ),
        (
            "7d:desc",
            ["alpha", "tie", "bravo", "charlie", "unknown", "invalid"],
        ),
        (
            "7d_oi:asc",
            ["alpha", "tie", "bravo", "charlie", "unknown", "invalid"],
        ),
        (
            "7d_oi:desc",
            ["charlie", "bravo", "alpha", "tie", "unknown", "invalid"],
        ),
        (
            "added_at:asc",
            ["charlie", "unknown", "invalid", "bravo", "alpha", "tie"],
        ),
        (
            "added_at:desc",
            ["alpha", "tie", "bravo", "invalid", "unknown", "charlie"],
        ),
    ] {
        let output = anthropic_accounts_command(&config, &["list", sort])
            .unwrap()
            .output()
            .unwrap();
        assert!(output.status.success(), "{sort}: {output:?}");
        let output = String::from_utf8(output.stdout).unwrap();
        assert_eq!(output.lines().count(), names.len() + 1, "{sort}");
        assert_eq!(listed_account_names(&output), expected, "{sort}");
        assert!(!output.contains("private-test-token"));
    }
    assert_eq!(std::fs::read(registry).unwrap(), registrations);
    assert_eq!(listed_account_names(&list_output(&config)), names);
}

#[test]
fn update_without_registered_accounts_does_not_contact_upstream_or_create_usage() {
    let config = TempDir::new().unwrap();
    anthropic_accounts_command(&config, &["update"])
        .unwrap()
        .env("CCP_ANTHROPIC_BASE_URL", "not-a-valid-upstream-url")
        .assert()
        .success()
        .stdout(contains("No Claude accounts registered"));
    assert!(!list_usage_directory(&config).exists());
}

#[test]
fn list_shows_selection_reset_wait_and_remaining_usage_percentages() {
    let config = TempDir::new().unwrap();
    register_list_fixture(&config, &["current", "waiting", "spare", "unknown"]);
    write_list_usage(
        &config,
        "current",
        serde_json::json!({
            "account": "current", "active": true, "eligible": true,
            "windows": {
                "5h": {"utilization": 0.1234},
                "7d": {"utilization": 0.5678},
                "7d_oi": {"utilization": 0.07}
            }
        }),
    );
    write_list_usage(
        &config,
        "waiting",
        serde_json::json!({
            "account": "waiting", "active": true, "eligible": false,
            "blockedUntilUnixSecs": 4102444800_u64,
            "windows": {"5h": {"utilization": 1.01}, "7d": {"utilization": 0.3}}
        }),
    );
    write_list_usage(
        &config,
        "spare",
        serde_json::json!({
            "account": "spare", "active": false, "eligible": true,
            "windows": {"5h": {"utilization": 0.4}, "7d": {"utilization": 0.8}}
        }),
    );

    let stdout = list_output(&config);
    let current = listed_account(&stdout, "current");
    assert!(current.contains("[稼働中]"));
    assert_eq!(listed_cell(&stdout, "current", "5h"), "12.3% (reset --)");
    assert_eq!(listed_cell(&stdout, "current", "7d"), "56.8% (reset --)");
    assert_eq!(listed_cell(&stdout, "current", "7d_oi"), "7.0% (reset --)");
    let waiting = listed_account(&stdout, "waiting");
    assert!(waiting.contains("[リセット待ち]"));
    assert_eq!(listed_cell(&stdout, "waiting", "5h"), "101.0% (reset --)");
    let spare = listed_account(&stdout, "spare");
    assert!(spare.contains("[残あり]"));
    assert_eq!(listed_cell(&stdout, "spare", "5h"), "40.0% (reset --)");
    assert_eq!(listed_cell(&stdout, "spare", "7d"), "80.0% (reset --)");
    assert_eq!(listed_cell(&stdout, "spare", "7d_oi"), "-- (reset --)");
    let unknown = listed_account(&stdout, "unknown");
    assert!(unknown.contains("[未確認]"));
    for column in ["5h", "7d", "7d_oi"] {
        assert_eq!(listed_cell(&stdout, "unknown", column), "-- (reset --)");
    }
}

#[test]
fn list_checks_all_reported_windows_and_ignores_missing_five_hour_usage() {
    let config = TempDir::new().unwrap();
    register_list_fixture(
        &config,
        &[
            "session",
            "weekly",
            "weekly_ready",
            "session_without_fable",
            "fable_with_session",
            "fable",
            "unknown",
        ],
    );
    let reset = 4102444800_u64;
    for (name, windows) in [
        (
            "session",
            serde_json::json!({
                "5h": {"utilization": 0.2, "resetAtUnixSecs": reset},
                "7d": {"utilization": 1.0, "resetAtUnixSecs": reset},
                "7d_oi": {"utilization": 0.2, "resetAtUnixSecs": reset}
            }),
        ),
        (
            "fable_with_session",
            serde_json::json!({
                "5h": {"utilization": 0.2, "resetAtUnixSecs": reset},
                "7d": {"utilization": 0.2, "resetAtUnixSecs": reset},
                "7d_oi": {"utilization": 0.99, "resetAtUnixSecs": reset}
            }),
        ),
        (
            "weekly",
            serde_json::json!({
                "5h": null,
                "7d": {"utilization": 0.99, "resetAtUnixSecs": reset}
            }),
        ),
        (
            "weekly_ready",
            serde_json::json!({
                "7d": {"utilization": 0.2, "resetAtUnixSecs": reset}
            }),
        ),
        (
            "session_without_fable",
            serde_json::json!({
                "5h": {"utilization": 0.2, "resetAtUnixSecs": reset},
                "7d": {"utilization": 0.3, "resetAtUnixSecs": reset},
                "7d_oi": null
            }),
        ),
        (
            "fable",
            serde_json::json!({
                "7d": {"utilization": 0.2, "resetAtUnixSecs": reset},
                "7d_oi": {"utilization": 0.99, "resetAtUnixSecs": reset}
            }),
        ),
        (
            "unknown",
            serde_json::json!({"5h": null, "7d": null, "7d_oi": null}),
        ),
    ] {
        write_list_usage(
            &config,
            name,
            serde_json::json!({
                "account": name, "active": false, "eligible": true, "windows": windows
            }),
        );
    }
    let output = anthropic_accounts_command(&config, &["list"])
        .unwrap()
        .env("TZ", "UTC")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for name in ["weekly_ready", "session_without_fable"] {
        assert!(listed_account(&stdout, name).contains("[残あり]"));
        assert_eq!(listed_cell(&stdout, name, "available_at"), "今すぐ");
    }
    for name in ["session", "weekly", "fable_with_session", "fable"] {
        assert!(listed_account(&stdout, name).contains("[リセット待ち]"));
        assert_eq!(
            listed_cell(&stdout, name, "available_at"),
            "2100/01/01 00:00"
        );
    }
    assert_eq!(listed_cell(&stdout, "weekly", "5h"), "-- (reset --)");
    assert_eq!(listed_cell(&stdout, "weekly_ready", "5h"), "-- (reset --)");
    assert_eq!(
        listed_cell(&stdout, "weekly_ready", "7d_oi"),
        "-- (reset --)"
    );
    assert_eq!(
        listed_cell(&stdout, "session_without_fable", "7d_oi"),
        "-- (reset --)"
    );
    assert!(listed_account(&stdout, "unknown").contains("[未確認]"));
    assert_eq!(listed_cell(&stdout, "unknown", "available_at"), "不明");
}

#[test]
fn list_preserves_registration_order_unless_available_at_sort_is_requested() {
    let config = TempDir::new().unwrap();
    let registration_order = [
        "weekly",
        "unknown",
        "later",
        "multi",
        "ready",
        "early",
        "invalid",
        "missing_reset",
        "expired",
        "retry",
        "same",
        "custom",
    ];
    register_list_fixture(&config, &registration_order);
    let registry = config.path().join("anthropic/accounts.json");
    let registrations = std::fs::read(&registry).unwrap();
    let first_reset = 4102444800_u64;
    for (name, snapshot) in [
        (
            "weekly",
            serde_json::json!({
                "blockedUntilUnixSecs": first_reset, "eligible": false,
                "windows": {
                    "5h": {"utilization": 0.0, "resetAtUnixSecs": first_reset},
                    "7d": {"utilization": 1.02, "resetAtUnixSecs": first_reset + 86400}
                }
            }),
        ),
        (
            "later",
            serde_json::json!({
                "eligible": false,
                "windows": {"5h": {"utilization": 1.0, "resetAtUnixSecs": first_reset + 3600}}
            }),
        ),
        (
            "multi",
            serde_json::json!({
                "blockedUntilUnixSecs": first_reset, "eligible": false,
                "windows": {
                    "5h": {"utilization": 1.44, "resetAtUnixSecs": first_reset},
                    "7d": {"utilization": 0.5, "resetAtUnixSecs": first_reset + 86400},
                    "7d_oi": {"utilization": 0.99, "resetAtUnixSecs": first_reset + 7200}
                }
            }),
        ),
        (
            "ready",
            serde_json::json!({"active": true, "eligible": true, "windows": {"5h": {"utilization": 0.2}}}),
        ),
        (
            "early",
            serde_json::json!({
                "eligible": false,
                "windows": {
                    "5h": {"utilization": 0.98, "resetAtUnixSecs": first_reset},
                    "7d": {"utilization": 0.5, "resetAtUnixSecs": first_reset + 172800}
                }
            }),
        ),
        (
            "invalid",
            serde_json::json!({"invalid": true, "active": true}),
        ),
        (
            "missing_reset",
            serde_json::json!({"eligible": false, "windows": {
                "5h": {"utilization": 1.0},
                "7d": {"utilization": 1.0, "resetAtUnixSecs": first_reset}
            }}),
        ),
        (
            "expired",
            serde_json::json!({
                "eligible": false, "blockedUntilUnixSecs": 1,
                "windows": {
                    "5h": {"utilization": 1.0, "resetAtUnixSecs": 1},
                    "7d": {"utilization": 0.4, "resetAtUnixSecs": first_reset}
                }
            }),
        ),
        (
            "retry",
            serde_json::json!({"eligible": false, "lastResponseStatus": 429, "blockedUntilUnixSecs": first_reset}),
        ),
        (
            "same",
            serde_json::json!({
                "eligible": false,
                "windows": {"5h": {"utilization": 1.0, "resetAtUnixSecs": first_reset + 3600}}
            }),
        ),
        (
            "custom",
            serde_json::json!({
                "switchThreshold": 0.9, "eligible": true,
                "windows": {"5h": {"utilization": 0.95, "resetAtUnixSecs": first_reset + 5400}}
            }),
        ),
    ] {
        let mut snapshot = snapshot;
        snapshot["account"] = serde_json::json!(name);
        write_list_usage(&config, name, snapshot);
    }
    let default_output = list_output(&config);
    assert_eq!(listed_account_names(&default_output), registration_order);
    assert_eq!(default_output.lines().count(), registration_order.len() + 1);
    let output = anthropic_accounts_command(&config, &["list", "available_at:asc"])
        .unwrap()
        .env("TZ", "UTC")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        listed_account_names(&stdout),
        [
            "ready",
            "expired",
            "early",
            "later",
            "same",
            "custom",
            "multi",
            "weekly",
            "unknown",
            "missing_reset",
            "retry",
            "invalid",
        ]
    );
    for name in ["ready", "expired"] {
        assert_eq!(listed_cell(&stdout, name, "available_at"), "今すぐ");
    }
    for (name, reset) in [
        ("early", "2100/01/01 00:00"),
        ("later", "2100/01/01 01:00"),
        ("custom", "2100/01/01 01:30"),
        ("multi", "2100/01/01 02:00"),
        ("weekly", "2100/01/02 00:00"),
    ] {
        assert_eq!(listed_cell(&stdout, name, "available_at"), reset);
    }
    for name in ["unknown", "missing_reset", "retry"] {
        assert_eq!(listed_cell(&stdout, name, "available_at"), "不明");
    }
    assert_eq!(listed_cell(&stdout, "invalid", "available_at"), "要再認証");
    assert_eq!(stdout.lines().count(), registration_order.len() + 1);
    assert_eq!(std::fs::read(registry).unwrap(), registrations);
    assert_eq!(
        listed_account_names(&list_output(&config)),
        registration_order
    );
}

#[test]
fn list_reads_legacy_selection_and_does_not_override_explicit_inactive_state() {
    let config = TempDir::new().unwrap();
    register_list_fixture(&config, &["legacy"]);
    let snapshot = serde_json::json!({
        "account": "legacy", "eligible": true, "windows": {"5h": {"utilization": 0.4}}
    });
    write_list_usage(&config, "legacy", snapshot.clone());
    let log = config.path().join("claude-code-proxy/proxy.log");
    let selected = serde_json::json!({
        "service": "anthropic", "msg": "anthropic_account_selected",
        "fields": {"account": "legacy"}
    });
    let unrelated = serde_json::json!({
        "service": "anthropic", "msg": "anthropic_account_selected",
        "fields": {"account": "unregistered"}
    });
    let other_pool = serde_json::json!({
        "service": "anthropic", "msg": "anthropic_accounts_refresh_started",
        "fields": {"loadedAccountCount": 2}
    });
    std::fs::write(
        &log,
        format!("invalid log line\n{selected}\n{unrelated}\n{other_pool}\n"),
    )
    .unwrap();
    assert!(listed_account(&list_output(&config), "legacy").contains("[稼働中]"));

    let mut inactive = snapshot.clone();
    inactive["active"] = serde_json::json!(false);
    write_list_usage(&config, "legacy", inactive);
    assert!(listed_account(&list_output(&config), "legacy").contains("[残あり]"));

    write_list_usage(&config, "legacy", snapshot);
    let startup = serde_json::json!({
        "service": "anthropic", "msg": "anthropic_accounts_refresh_started"
    });
    std::fs::write(&log, format!("{selected}\n{startup}\n")).unwrap();
    assert!(listed_account(&list_output(&config), "legacy").contains("[残あり]"));
}

#[test]
fn list_expires_blocks_preserves_old_usage_and_identifies_invalid_credentials() {
    let config = TempDir::new().unwrap();
    register_list_fixture(&config, &["recovered", "invalid", "old"]);
    write_list_usage(
        &config,
        "recovered",
        serde_json::json!({
            "account": "recovered", "active": false, "eligible": false,
            "blockedUntilUnixSecs": 1,
            "windows": {
                "5h": {"utilization": 1.0, "resetAtUnixSecs": 1},
                "7d": {"utilization": 0.4, "resetAtUnixSecs": 4102444800_u64}
            }
        }),
    );
    write_list_usage(
        &config,
        "invalid",
        serde_json::json!({"account": "invalid", "active": true, "invalid": true}),
    );
    write_list_usage(
        &config,
        "old",
        serde_json::json!({
            "account": "old", "active": false,
            "windows": {"5h": {"utilization": 0.2, "resetAtUnixSecs": 1}}
        }),
    );

    let stdout = list_output(&config);
    let recovered = listed_account(&stdout, "recovered");
    assert!(recovered.contains("[残あり]"));
    assert!(listed_cell(&stdout, "recovered", "5h").starts_with("100.0%* (reset "));
    assert!(listed_cell(&stdout, "recovered", "7d").starts_with("40.0% (reset "));
    assert!(listed_account(&stdout, "invalid").contains("[認証無効]"));
    assert!(listed_account(&stdout, "old").contains("[未確認]"));
    assert_eq!(stdout.lines().count(), 4);
}

#[test]
fn list_does_not_use_corrupt_or_mismatched_account_usage() {
    let config = TempDir::new().unwrap();
    register_list_fixture(&config, &["corrupt", "mismatched", "spare"]);
    write_list_usage(
        &config,
        "mismatched",
        serde_json::json!({"account": "other"}),
    );
    std::fs::write(
        list_usage_directory(&config).join("corrupt.json"),
        "{broken",
    )
    .unwrap();
    write_list_usage(
        &config,
        "spare",
        serde_json::json!({
            "account": "spare", "active": false, "windows": {"5h": {"utilization": 0.0}}
        }),
    );
    let stdout = list_output(&config);
    for name in ["corrupt", "mismatched"] {
        let row = listed_account(&stdout, name);
        assert!(row.contains("[未確認]"));
        for column in ["5h", "7d", "7d_oi"] {
            assert_eq!(listed_cell(&stdout, name, column), "-- (reset --)");
        }
    }
    let spare = listed_account(&stdout, "spare");
    assert!(spare.contains("[残あり]"));
    assert_eq!(listed_cell(&stdout, "spare", "available_at"), "今すぐ");
    assert!(listed_cell(&stdout, "spare", "5h").starts_with("0.0% (reset "));
}

#[test]
fn anthropic_accounts_add_list_remove_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;

    anthropic_accounts_command(&temp, &["add", "second"])?
        .write_stdin("sk-ant-oat-test-second\n")
        .assert()
        .success()
        .stdout(contains("Registered account second (1 accounts)"));

    let output = anthropic_accounts_command(&temp, &["list"])?.output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("1. second"));
    assert!(!stdout.contains("sk-ant-oat-test-second"));

    anthropic_accounts_command(&temp, &["remove", "second"])?
        .assert()
        .success()
        .stdout(contains("Removed account second"));

    anthropic_accounts_command(&temp, &["list"])?
        .assert()
        .success()
        .stdout(contains("No Claude accounts registered"));
    Ok(())
}

#[test]
fn anthropic_accounts_reject_duplicate_and_unknown_names() -> Result<(), Box<dyn std::error::Error>>
{
    let temp = TempDir::new()?;

    anthropic_accounts_command(&temp, &["add", "second"])?
        .write_stdin("sk-ant-oat-test-second\n")
        .assert()
        .success();

    anthropic_accounts_command(&temp, &["add", "second"])?
        .write_stdin("sk-ant-oat-test-other\n")
        .assert()
        .failure()
        .code(2);

    anthropic_accounts_command(&temp, &["remove", "missing"])?
        .assert()
        .failure()
        .code(2);
    Ok(())
}

#[test]
fn read_only_commands_do_not_create_or_overwrite_usage_snapshots() {
    for existing in [true, false] {
        let config = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let registrations = config.path().join("anthropic");
        std::fs::create_dir_all(&registrations).unwrap();
        let accounts: Vec<_> = ["first", "second", "third", "forth", "five", "six"]
            .into_iter()
            .map(|name| {
                serde_json::json!({
                    "name": name, "token": format!("test-token-{name}"), "addedAt": 0
                })
            })
            .collect();
        std::fs::write(
            registrations.join("accounts.json"),
            serde_json::to_vec(&serde_json::json!({"accounts": accounts})).unwrap(),
        )
        .unwrap();
        let usage = state.path().join("claude-code-proxy/anthropic/accounts");
        let snapshot = usage.join("first.json");
        let original = r#"{"account":"first","windows":{"5h":{"utilization":0.64,"resetAtUnixSecs":4102444800}}}"#;
        if existing {
            std::fs::create_dir_all(&usage).unwrap();
            std::fs::write(&snapshot, original).unwrap();
        }

        for args in [
            vec!["models"],
            vec!["models", "--full"],
            vec!["kimi", "auth", "status"],
            vec!["anthropic", "accounts", "list"],
            vec!["anthropic", "accounts", "list", "name:asc"],
            vec!["anthropic", "accounts", "list", "available_at:desc"],
            vec!["anthropic", "accounts", "list", "5h:desc"],
        ] {
            let output = Command::cargo_bin("claude-code-proxy")
                .unwrap()
                .env("CCP_CONFIG_DIR", config.path())
                .env("XDG_STATE_HOME", state.path())
                .env("LOCALAPPDATA", state.path())
                .args(&args)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(if args[0] == "kimi" { 1 } else { 0 })
            );
            if existing {
                assert_eq!(
                    std::fs::read_to_string(&snapshot).unwrap(),
                    original,
                    "{args:?} overwrote usage"
                );
                assert_eq!(
                    std::fs::read_dir(&usage).unwrap().count(),
                    1,
                    "{args:?} created usage files"
                );
            } else {
                assert!(!usage.exists(), "{args:?} created a usage directory");
            }
        }
    }
}

struct ProxyProcess(std::process::Child);

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn register_probe_fixture(config: &TempDir, names: &[&str]) {
    let registrations = config.path().join("anthropic");
    std::fs::create_dir_all(&registrations).unwrap();
    let accounts: Vec<_> = names
        .iter()
        .map(|name| {
            serde_json::json!({"name": name, "token": format!("test-token-{name}"), "addedAt": 0})
        })
        .collect();
    std::fs::write(
        registrations.join("accounts.json"),
        serde_json::to_vec(&serde_json::json!({"accounts": accounts})).unwrap(),
    )
    .unwrap();
}

async fn read_probe_request(stream: &mut tokio::net::TcpStream) -> (String, serde_json::Value) {
    use tokio::io::AsyncReadExt;

    let mut request = Vec::new();
    let body_offset = loop {
        let mut chunk = [0; 4096];
        let len = stream.read(&mut chunk).await.unwrap();
        assert_ne!(len, 0);
        request.extend_from_slice(&chunk[..len]);
        let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..end]);
        let body_len = headers
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        if request.len() >= end + 4 + body_len {
            break end + 4;
        }
    };
    (
        String::from_utf8_lossy(&request[..body_offset]).into_owned(),
        serde_json::from_slice(&request[body_offset..]).unwrap(),
    )
}

const MOCK_UPDATED_USAGE: &str = concat!(
    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n",
    "anthropic-ratelimit-unified-5h-utilization: 0.1\r\n",
    "anthropic-ratelimit-unified-5h-reset: 4102444800\r\n",
    "anthropic-ratelimit-unified-7d-utilization: 0.2\r\n",
    "anthropic-ratelimit-unified-7d-reset: 4102531200\r\n",
    "anthropic-ratelimit-unified-7d_oi-utilization: 0.3\r\n",
    "anthropic-ratelimit-unified-7d_oi-reset: 4102531200\r\n\r\n{}"
);

async fn update_against_mock(
    config: &TempDir,
    responses: &[(&str, Option<&str>)],
) -> std::process::Output {
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_claude-code-proxy"));
    command
        .args(["anthropic", "accounts", "update"])
        .env("CCP_CONFIG_DIR", config.path())
        .env("XDG_STATE_HOME", config.path())
        .env("LOCALAPPDATA", config.path())
        .env("CCP_ANTHROPIC_SWITCH_THRESHOLD", "98")
        .env(
            "CCP_ANTHROPIC_BASE_URL",
            format!("http://{}", upstream.local_addr().unwrap()),
        )
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env("TZ", "UTC");
    let output = tokio::task::spawn_blocking(move || command.output().unwrap());
    let mock = async {
        let mut checked = std::collections::HashSet::new();
        for _ in responses {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let (headers, body) = read_probe_request(&mut stream).await;
            assert!(headers.starts_with("POST /v1/messages?beta=true HTTP/1.1"));
            assert_eq!(body["max_tokens"], 1);
            assert_eq!(body["stream"], false);
            let (name, response) = responses
                .iter()
                .find(|(name, _)| headers.contains(&format!("Bearer test-token-{name}\r\n")))
                .unwrap();
            assert!(checked.insert(*name), "account probed more than once");
            if let Some(response) = response {
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        }
    };
    let (output, ()) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(output, mock)
    })
    .await
    .expect("accounts update should finish all probes");
    let output = output.unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("test-token-"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("test-token-"));
    output
}

#[tokio::test]
async fn update_refreshes_all_accounts_and_preserves_selection() {
    let config = TempDir::new().unwrap();
    let names = ["first", "second", "third", "fourth", "fifth", "sixth"];
    register_probe_fixture(&config, &names);
    for name in names {
        write_list_usage(
            &config,
            name,
            serde_json::json!({
                "account": name, "active": name == "second", "invalid": true,
                "windows": {"5h": {"utilization": 0.8, "resetAtUnixSecs": 1}}
            }),
        );
    }
    let registry = config.path().join("anthropic/accounts.json");
    let registrations = std::fs::read(&registry).unwrap();
    let responses: Vec<_> = names
        .iter()
        .map(|name| (*name, Some(MOCK_UPDATED_USAGE)))
        .collect();
    let output = update_against_mock(&config, &responses).await;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), names.len() + 1);
    assert_eq!(
        stdout
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>(),
        [
            "name",
            "status",
            "available_at",
            "5h",
            "7d",
            "7d_oi",
            "added_at"
        ]
    );
    assert_eq!(listed_account_names(&stdout), names);
    assert!(listed_account(&stdout, "second").contains("[稼働中]"));
    assert_eq!(
        listed_cell(&stdout, "second", "5h"),
        "10.0% (reset 2100/01/01 00:00)"
    );
    assert_eq!(std::fs::read(registry).unwrap(), registrations);
    for name in names {
        let snapshot: serde_json::Value = serde_json::from_slice(
            &std::fs::read(list_usage_directory(&config).join(format!("{name}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot["lastResponseStatus"], 200);
        assert_eq!(snapshot["active"], name == "second");
        assert_eq!(snapshot["eligible"], true);
        assert_eq!(snapshot["invalid"], false);
        assert_eq!(snapshot["windows"]["5h"]["utilization"], 0.1);
        assert_eq!(snapshot["windows"]["7d"]["utilization"], 0.2);
        assert_eq!(snapshot["windows"]["7d_oi"]["utilization"], 0.3);
    }
}

#[tokio::test]
async fn update_keeps_previous_usage_on_failures_and_replaces_windows_on_success() {
    let config = TempDir::new().unwrap();
    let names = [
        "current",
        "limited",
        "invalid",
        "offline",
        "missing",
        "unavailable",
    ];
    register_probe_fixture(&config, &names);
    for name in names {
        write_list_usage(
            &config,
            name,
            serde_json::json!({
                "account": name, "active": name == "current",
                "windows": {
                    "5h": {"utilization": 0.8, "observedAtUnixSecs": 42, "resetAtUnixSecs": 1},
                    "7d": {"utilization": 1.0, "observedAtUnixSecs": 42, "resetAtUnixSecs": 4102531200_u64},
                    "7d_oi": {"utilization": 0.3, "observedAtUnixSecs": 42, "resetAtUnixSecs": 1}
                }
            }),
        );
    }
    let previous: Vec<_> = ["offline", "missing", "unavailable"]
        .iter()
        .map(|name| {
            let path = list_usage_directory(&config).join(format!("{name}.json"));
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    let output = update_against_mock(
        &config,
        &[
            (
                "current",
                Some(concat!(
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n",
                    "anthropic-ratelimit-unified-5h-utilization: 0.1\r\n",
                    "anthropic-ratelimit-unified-5h-reset: 4102444800\r\n\r\n{}"
                )),
            ),
            (
                "limited",
                Some(concat!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 2\r\nconnection: close\r\n",
                    "anthropic-ratelimit-unified-5h-utilization: 1.0\r\n",
                    "anthropic-ratelimit-unified-5h-reset: 4102444800\r\n\r\n{}"
                )),
            ),
            (
                "invalid",
                Some(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                ),
            ),
            ("offline", None),
            (
                "missing",
                Some("HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"),
            ),
            (
                "unavailable",
                Some(
                    "HTTP/1.1 503 Unavailable\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                ),
            ),
        ],
    )
    .await;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), names.len() + 1);
    assert_eq!(listed_account_names(&stdout), names);
    assert!(listed_account(&stdout, "current").contains("[稼働中]"));
    assert!(listed_account(&stdout, "invalid").contains("[認証無効]"));
    let stderr = String::from_utf8(output.stderr).unwrap();
    for name in ["invalid", "offline", "missing", "unavailable"] {
        assert!(stderr.contains(&format!("Cannot update usage for {name}:")));
    }
    for (path, bytes) in previous {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    let current: serde_json::Value = serde_json::from_slice(
        &std::fs::read(list_usage_directory(&config).join("current.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(current["active"], true);
    assert_eq!(current["eligible"], true);
    assert_eq!(current["windows"]["5h"]["utilization"], 0.1);
    assert!(current["windows"]["7d"].is_null());
    assert!(current["windows"]["7d_oi"].is_null());
    let limited: serde_json::Value = serde_json::from_slice(
        &std::fs::read(list_usage_directory(&config).join("limited.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(limited["lastResponseStatus"], 429);
    assert_eq!(limited["eligible"], false);
}

#[tokio::test]
async fn update_does_not_restore_absent_five_hour_or_fable_limits() {
    let config = TempDir::new().unwrap();
    let names = ["weekly_only", "without_fable", "without_five_hour"];
    register_probe_fixture(&config, &names);
    for name in names {
        write_list_usage(
            &config,
            name,
            serde_json::json!({
                "account": name, "active": name == "weekly_only", "eligible": false,
                "blockedUntilUnixSecs": 4102444800_u64,
                "windows": {
                    "5h": {"utilization": 1.0, "observedAtUnixSecs": 42, "resetAtUnixSecs": 4102444800_u64},
                    "7d": {"utilization": 1.0, "observedAtUnixSecs": 42, "resetAtUnixSecs": 4102444800_u64},
                    "7d_oi": {"utilization": 1.0, "observedAtUnixSecs": 42, "resetAtUnixSecs": 4102444800_u64}
                }
            }),
        );
    }
    let output = update_against_mock(
        &config,
        &[
            (
                "weekly_only",
                Some(concat!(
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n",
                    "anthropic-ratelimit-unified-7d-utilization: 0.2\r\n",
                    "anthropic-ratelimit-unified-7d-reset: 4102531200\r\n\r\n{}"
                )),
            ),
            (
                "without_fable",
                Some(concat!(
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n",
                    "anthropic-ratelimit-unified-5h-utilization: 0.1\r\n",
                    "anthropic-ratelimit-unified-5h-reset: 4102444800\r\n",
                    "anthropic-ratelimit-unified-7d-utilization: 0.2\r\n",
                    "anthropic-ratelimit-unified-7d-reset: 4102531200\r\n\r\n{}"
                )),
            ),
            (
                "without_five_hour",
                Some(concat!(
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n",
                    "anthropic-ratelimit-unified-7d-utilization: 0.2\r\n",
                    "anthropic-ratelimit-unified-7d-reset: 4102531200\r\n",
                    "anthropic-ratelimit-unified-7d_oi-utilization: 0.3\r\n",
                    "anthropic-ratelimit-unified-7d_oi-reset: 4102531200\r\n\r\n{}"
                )),
            ),
        ],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), names.len() + 1);
    assert_eq!(listed_account_names(&stdout), names);
    for (name, five_hour, fable) in [
        ("weekly_only", None, None),
        ("without_fable", Some(0.1), None),
        ("without_five_hour", None, Some(0.3)),
    ] {
        let snapshot: serde_json::Value = serde_json::from_slice(
            &std::fs::read(list_usage_directory(&config).join(format!("{name}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot["eligible"], true);
        assert!(snapshot["blockedUntilUnixSecs"].is_null());
        assert_eq!(snapshot["windows"]["5h"]["utilization"].as_f64(), five_hour);
        assert_eq!(snapshot["windows"]["7d"]["utilization"], 0.2);
        assert_eq!(snapshot["windows"]["7d_oi"]["utilization"].as_f64(), fable);
        assert_eq!(listed_cell(&stdout, name, "available_at"), "今すぐ");
        if five_hour.is_none() {
            assert!(snapshot["windows"]["5h"].is_null());
            assert_eq!(listed_cell(&stdout, name, "5h"), "-- (reset --)");
        }
        if fable.is_none() {
            assert!(snapshot["windows"]["7d_oi"].is_null());
            assert_eq!(listed_cell(&stdout, name, "7d_oi"), "-- (reset --)");
        }
    }
}

#[tokio::test]
async fn update_reports_usage_write_failures() {
    let config = TempDir::new().unwrap();
    register_probe_fixture(&config, &["first"]);
    let snapshot_path = list_usage_directory(&config).join("first.json");
    std::fs::create_dir_all(&snapshot_path).unwrap();
    let output = update_against_mock(&config, &[("first", Some(MOCK_UPDATED_USAGE))]).await;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2);
    assert_eq!(listed_account_names(&stdout), ["first"]);
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Cannot save usage for first:")
    );
    assert!(snapshot_path.is_dir());
}

#[tokio::test]
async fn serve_refreshes_all_six_accounts_before_any_client_request() {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let names = ["first", "second", "third", "forth", "five", "six"];
    register_probe_fixture(&config, &names);
    let usage = state.path().join("claude-code-proxy/anthropic/accounts");
    std::fs::create_dir_all(&usage).unwrap();
    std::fs::write(
        usage.join("first.json"),
        r#"{"account":"first","lastResponseStatus":429,"windows":{"5h":null}}"#,
    )
    .unwrap();

    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let _child = ProxyProcess(
        std::process::Command::new(env!("CARGO_BIN_EXE_claude-code-proxy"))
            .args(["serve", "--no-monitor", "--port", "0"])
            .env("CCP_CONFIG_DIR", config.path())
            .env("XDG_STATE_HOME", state.path())
            .env("LOCALAPPDATA", state.path())
            .env("CCP_BIND_ADDRESS", "127.0.0.1")
            .env(
                "CCP_ANTHROPIC_BASE_URL",
                format!("http://{}", upstream.local_addr().unwrap()),
            )
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    tokio::time::timeout(Duration::from_secs(10), async {
        let mut checked = std::collections::HashSet::new();
        for _ in names {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let (headers, body) = read_probe_request(&mut stream).await;
            assert!(headers.starts_with("POST /v1/messages?beta=true HTTP/1.1"));
            let account = names
                .iter()
                .find(|name| headers.contains(&format!("Bearer test-token-{name}\r\n")))
                .unwrap();
            assert!(checked.insert(*account), "account probed more than once");
            assert_eq!(body["max_tokens"], 1);
            assert_eq!(body["stream"], false);

            stream
                .write_all(MOCK_UPDATED_USAGE.as_bytes())
                .await
                .unwrap();
        }
        loop {
            let ready = names.iter().all(|name| {
                std::fs::read(usage.join(format!("{name}.json")))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .is_some_and(|snapshot| snapshot["lastResponseStatus"] == 200)
            });
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("serve should refresh all accounts without client traffic");

    for name in names {
        let text = std::fs::read_to_string(usage.join(format!("{name}.json"))).unwrap();
        let snapshot: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(snapshot["account"], name);
        assert_eq!(snapshot["windows"]["5h"]["utilization"], 0.1);
        assert_eq!(snapshot["windows"]["5h"]["resetAtUnixSecs"], 4102444800_u64);
        assert_eq!(snapshot["windows"]["7d"]["utilization"], 0.2);
        assert_eq!(snapshot["windows"]["7d_oi"]["utilization"], 0.3);
        assert_eq!(snapshot["eligible"], true);
        assert!(!text.contains("test-token-"));
    }
}
