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
        .stdout(contains("remove"));
}

#[test]
fn corrupt_accounts_file_is_not_treated_as_empty_or_overwritten() {
    let config = TempDir::new().unwrap();
    let directory = config.path().join("anthropic");
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("accounts.json");
    for content in [r#"{"accounts":["#, r#"{"accounts":"invalid"}"#] {
        std::fs::write(&file, content).unwrap();

        for args in [["list"].as_slice(), ["add", "new"].as_slice()] {
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
    Ok(cmd)
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

#[tokio::test]
async fn serve_refreshes_all_six_accounts_before_any_client_request() {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let names = ["first", "second", "third", "forth", "five", "six"];
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
            let headers = String::from_utf8_lossy(&request[..body_offset]);
            assert!(headers.starts_with("POST /v1/messages?beta=true HTTP/1.1"));
            let account = names
                .iter()
                .find(|name| headers.contains(&format!("Bearer test-token-{name}\r\n")))
                .unwrap();
            assert!(checked.insert(*account), "account probed more than once");
            let body: serde_json::Value = serde_json::from_slice(&request[body_offset..]).unwrap();
            assert_eq!(body["max_tokens"], 1);
            assert_eq!(body["stream"], false);

            let response = concat!(
                "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n",
                "anthropic-ratelimit-unified-5h-utilization: 0.1\r\n",
                "anthropic-ratelimit-unified-5h-reset: 4102444800\r\n",
                "anthropic-ratelimit-unified-7d-utilization: 0.2\r\n",
                "anthropic-ratelimit-unified-7d-reset: 4102531200\r\n",
                "anthropic-ratelimit-unified-7d_oi-utilization: 0.3\r\n",
                "anthropic-ratelimit-unified-7d_oi-reset: 4102531200\r\n\r\n{}"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
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
