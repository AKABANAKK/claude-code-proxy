use super::AnthropicProvider;
use super::account_relay::AccountRelay;
use super::pool::{AccountPool, PoolAccount};
use crate::provider::{Passthrough, Provider, RequestContext};
use axum::http::{HeaderValue, StatusCode, header::AUTHORIZATION};
use axum::response::Response;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const ORIGINAL_AUTHORIZATION: &str = "Bearer original";
const REGISTERED_ACCOUNT_NAME: &str = "test-registered";
const REGISTERED_TOKEN: &str = "sk-ant-oat-test-registered";
const MESSAGES_PATH: &str = "/v1/messages";
const READ_CHUNK_BYTES: usize = 4096;
const HTTP_HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";
const MOCK_OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
const MOCK_TOO_MANY_REQUESTS_RESPONSE: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
const MOCK_IMMEDIATE_RETRY_RESPONSE: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 0\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
const MOCK_UNAUTHORIZED_RESPONSE: &[u8] = b"HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
/// A 200 reporting the five-hour window as fully used until a reset far in the future.
const MOCK_OK_FIVE_HOUR_EXHAUSTED_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nanthropic-ratelimit-unified-5h-utilization: 1.0\r\nanthropic-ratelimit-unified-5h-reset: 4102444800\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
const FIRST_ACCOUNT_NAME: &str = "test-first";
const FIRST_TOKEN: &str = "sk-ant-oat-test-first";
const SECOND_ACCOUNT_NAME: &str = "test-second";
const SECOND_TOKEN: &str = "sk-ant-oat-test-second";

fn relay_context() -> RequestContext {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static(ORIGINAL_AUTHORIZATION),
    );
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    RequestContext {
        req_id: "req-relay".to_string(),
        session_id: None,
        session_seq: None,
        provider: "anthropic".to_string(),
        traffic: None,
        monitor: None,
        passthrough: Some(Passthrough {
            raw_body: axum::body::Bytes::from_static(
                br#"{"model":"claude-opus-4","messages":[{"role":"user","content":"hi"}]}"#,
            ),
            headers,
            path_and_query: MESSAGES_PATH.to_string(),
        }),
    }
}

async fn relay_request(provider: &AnthropicProvider) -> Response {
    let ctx = relay_context();
    let body = serde_json::from_slice(&ctx.passthrough.as_ref().unwrap().raw_body).unwrap();
    provider.handle_messages(body, ctx).await
}

fn registered_pool() -> AccountPool {
    AccountPool::new(
        vec![PoolAccount {
            name: REGISTERED_ACCOUNT_NAME.to_string(),
            token: REGISTERED_TOKEN.to_string(),
        }],
        crate::config::DEFAULT_ANTHROPIC_SWITCH_THRESHOLD,
        None,
    )
}

fn two_account_pool() -> AccountPool {
    AccountPool::new(
        vec![
            PoolAccount {
                name: FIRST_ACCOUNT_NAME.to_string(),
                token: FIRST_TOKEN.to_string(),
            },
            PoolAccount {
                name: SECOND_ACCOUNT_NAME.to_string(),
                token: SECOND_TOKEN.to_string(),
            },
        ],
        crate::config::DEFAULT_ANTHROPIC_SWITCH_THRESHOLD,
        None,
    )
}

fn sent_with(request: &str, token: &str) -> bool {
    request.contains(&format!("authorization: Bearer {token}"))
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    loop {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request ended before its body was complete");
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = request
            .windows(HTTP_HEADER_TERMINATOR.len())
            .position(|part| part == HTTP_HEADER_TERMINATOR)
        else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    return value.trim().parse::<usize>().ok();
                }
                None
            })
            .unwrap_or(0);
        if request.len() >= header_end + HTTP_HEADER_TERMINATOR.len() + content_length {
            return String::from_utf8_lossy(&request).into_owned();
        }
    }
}

/// Answers one connection per entry of `responses`, in order, while `relay_count`
/// requests are relayed through one provider. Returns the raw requests the upstream
/// received together with the responses relayed back.
async fn relay_through_mock_upstream(
    pool: Option<AccountPool>,
    responses: Vec<&'static [u8]>,
    relay_count: usize,
) -> (Vec<String>, Vec<Response>) {
    relay_through_mock_with_usage_directory(pool, responses, relay_count, None).await
}

async fn relay_through_mock_with_usage_directory(
    pool: Option<AccountPool>,
    responses: Vec<&'static [u8]>,
    relay_count: usize,
    usage_directory: Option<std::path::PathBuf>,
) -> (Vec<String>, Vec<Response>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(read_http_request(&mut stream).await);
            stream.write_all(response).await.unwrap();
        }
        requests
    });

    let provider = AnthropicProvider {
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        base_url: format!("http://{addr}"),
        accounts: match usage_directory {
            Some(directory) => AccountRelay::with_usage_directory(pool, directory),
            None => AccountRelay::with_pool(pool),
        },
    };
    let mut relayed = Vec::new();
    for _ in 0..relay_count {
        relayed.push(
            tokio::time::timeout(Duration::from_secs(5), relay_request(&provider))
                .await
                .expect("relay completes"),
        );
    }
    let requests = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("upstream receives every expected request")
        .unwrap();
    (requests, relayed)
}

fn response_statuses(responses: &[Response]) -> Vec<StatusCode> {
    responses.iter().map(Response::status).collect()
}

#[tokio::test]
async fn message_and_count_tokens_relays_preserve_original_body_bytes() {
    // Whitespace, escaped keys and unknown message fields must all survive the fast path.
    let raw = br#"{ "model" : "claude-original", "messages": [
        {"role":"assistant", "unknown":true, "content":[
            {"type":"th\u0069nking", "thinking":"signed", "signature":"sig"}
        ]}
    ] }"#;
    for count_tokens in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            stream.write_all(MOCK_OK_RESPONSE).await.unwrap();
            request
        });
        let provider = AnthropicProvider {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            base_url: format!("http://{addr}"),
            accounts: AccountRelay::with_pool(None),
        };
        let mut ctx = relay_context();
        let passthrough = ctx.passthrough.as_mut().unwrap();
        passthrough.raw_body = axum::body::Bytes::from_static(raw);
        let path = if count_tokens {
            "/v1/messages/count_tokens"
        } else {
            MESSAGES_PATH
        };
        passthrough.path_and_query = path.to_string();
        let mut body: crate::anthropic::schema::MessagesRequest =
            serde_json::from_slice(raw).unwrap();
        body.model = Some("normalized-model".to_string());
        let response = tokio::time::timeout(Duration::from_secs(5), async {
            if count_tokens {
                provider.handle_count_tokens(body, ctx).await
            } else {
                provider.handle_messages(body, ctx).await
            }
        })
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let request = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert!(request.starts_with(&format!("POST {path} HTTP/1.1\r\n")));
        assert_eq!(request.split_once("\r\n\r\n").unwrap().1.as_bytes(), raw);
    }
}

#[tokio::test]
async fn relay_without_pool_forwards_original_authorization() {
    let (requests, responses) = relay_through_mock_upstream(None, vec![MOCK_OK_RESPONSE], 1).await;

    assert_eq!(response_statuses(&responses), vec![StatusCode::OK]);
    assert!(
        requests[0].contains(&format!("authorization: {ORIGINAL_AUTHORIZATION}")),
        "{}",
        requests[0]
    );
}

#[tokio::test]
async fn relay_with_pool_replaces_authorization_with_active_account() {
    let (requests, responses) =
        relay_through_mock_upstream(Some(registered_pool()), vec![MOCK_OK_RESPONSE], 1).await;

    assert_eq!(response_statuses(&responses), vec![StatusCode::OK]);
    assert!(sent_with(&requests[0], REGISTERED_TOKEN), "{}", requests[0]);
    assert!(
        !requests[0].contains(ORIGINAL_AUTHORIZATION),
        "{}",
        requests[0]
    );
}

#[tokio::test]
async fn relay_retries_rate_limited_request_with_next_account() {
    let (requests, responses) = relay_through_mock_upstream(
        Some(two_account_pool()),
        vec![MOCK_TOO_MANY_REQUESTS_RESPONSE, MOCK_OK_RESPONSE],
        1,
    )
    .await;

    assert_eq!(response_statuses(&responses), vec![StatusCode::OK]);
    assert!(sent_with(&requests[0], FIRST_TOKEN), "{}", requests[0]);
    assert!(sent_with(&requests[1], SECOND_TOKEN), "{}", requests[1]);
    let expected = relay_context().passthrough.unwrap();
    for request in &requests {
        assert!(request.starts_with("POST /v1/messages HTTP/1.1\r\n"));
        assert_eq!(
            request.split_once("\r\n\r\n").unwrap().1.as_bytes(),
            expected.raw_body
        );
    }
}

#[tokio::test]
async fn relay_switches_account_after_usage_reaches_threshold() {
    let (requests, responses) = relay_through_mock_upstream(
        Some(two_account_pool()),
        vec![MOCK_OK_FIVE_HOUR_EXHAUSTED_RESPONSE, MOCK_OK_RESPONSE],
        2,
    )
    .await;

    assert_eq!(
        response_statuses(&responses),
        vec![StatusCode::OK, StatusCode::OK]
    );
    assert!(sent_with(&requests[0], FIRST_TOKEN), "{}", requests[0]);
    assert!(sent_with(&requests[1], SECOND_TOKEN), "{}", requests[1]);
}

#[tokio::test]
async fn relay_returns_rate_limit_when_every_account_rejects_the_request() {
    let (requests, responses) = relay_through_mock_upstream(
        Some(two_account_pool()),
        vec![
            MOCK_TOO_MANY_REQUESTS_RESPONSE,
            MOCK_TOO_MANY_REQUESTS_RESPONSE,
        ],
        1,
    )
    .await;

    assert_eq!(
        response_statuses(&responses),
        vec![StatusCode::TOO_MANY_REQUESTS]
    );
    assert!(sent_with(&requests[0], FIRST_TOKEN), "{}", requests[0]);
    assert!(sent_with(&requests[1], SECOND_TOKEN), "{}", requests[1]);
}

#[tokio::test]
async fn relay_stops_using_account_rejected_as_unauthorized() {
    let (requests, responses) = relay_through_mock_upstream(
        Some(two_account_pool()),
        vec![MOCK_UNAUTHORIZED_RESPONSE, MOCK_OK_RESPONSE],
        2,
    )
    .await;

    assert_eq!(
        response_statuses(&responses),
        vec![StatusCode::UNAUTHORIZED, StatusCode::OK]
    );
    assert!(sent_with(&requests[0], FIRST_TOKEN), "{}", requests[0]);
    assert!(sent_with(&requests[1], SECOND_TOKEN), "{}", requests[1]);
}

#[tokio::test]
async fn relay_tries_each_account_once_with_zero_retry_after() {
    for final_response in [MOCK_OK_RESPONSE, MOCK_IMMEDIATE_RETRY_RESPONSE] {
        let (requests, responses) = relay_through_mock_upstream(
            Some(two_account_pool()),
            vec![MOCK_IMMEDIATE_RETRY_RESPONSE, final_response],
            1,
        )
        .await;

        let expected_status = if final_response == MOCK_OK_RESPONSE {
            StatusCode::OK
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        assert_eq!(response_statuses(&responses), vec![expected_status]);
        assert_eq!(requests.len(), 2);
        assert!(sent_with(&requests[0], FIRST_TOKEN));
        assert!(sent_with(&requests[1], SECOND_TOKEN));
    }
}

#[tokio::test]
async fn relay_falls_back_to_original_authorization_when_all_accounts_are_invalid() {
    let (requests, responses) = relay_through_mock_upstream(
        Some(two_account_pool()),
        vec![
            MOCK_UNAUTHORIZED_RESPONSE,
            MOCK_UNAUTHORIZED_RESPONSE,
            MOCK_OK_RESPONSE,
        ],
        3,
    )
    .await;

    assert_eq!(
        response_statuses(&responses),
        vec![
            StatusCode::UNAUTHORIZED,
            StatusCode::UNAUTHORIZED,
            StatusCode::OK
        ]
    );
    assert!(sent_with(&requests[0], FIRST_TOKEN));
    assert!(sent_with(&requests[1], SECOND_TOKEN));
    assert!(requests[2].contains(&format!("authorization: {ORIGINAL_AUTHORIZATION}")));
}

#[tokio::test]
async fn relay_preserves_final_rate_limit_headers_and_body() {
    let (_, mut responses) = relay_through_mock_upstream(
        Some(two_account_pool()),
        vec![
            MOCK_TOO_MANY_REQUESTS_RESPONSE,
            b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\nrequest-id: final-rate-limit\r\ncontent-type: application/json\r\ncontent-length: 19\r\nconnection: close\r\n\r\n{\"error\":\"limited\"}",
        ],
        1,
    ).await;

    let response = responses.pop().unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "30");
    assert_eq!(response.headers()["request-id"], "final-rate-limit");
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), br#"{"error":"limited"}"#);
}

#[tokio::test]
async fn relay_preserves_streaming_response_after_switching_accounts() {
    let (_, mut responses) = relay_through_mock_upstream(
        Some(two_account_pool()),
        vec![
            MOCK_TOO_MANY_REQUESTS_RESPONSE,
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"type\":\"ping\"}\n\n",
        ],
        1,
    ).await;

    let response = responses.pop().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"data: {\"type\":\"ping\"}\n\n");
}

#[tokio::test]
async fn malformed_upstream_url_remains_a_bad_gateway_with_or_without_accounts() {
    for pool in [None, Some(two_account_pool())] {
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            base_url: "://invalid".to_string(),
            accounts: AccountRelay::with_pool(pool),
        };
        assert_eq!(
            relay_request(&provider).await.status(),
            StatusCode::BAD_GATEWAY
        );
    }
}

#[tokio::test]
async fn invalid_token_is_reported_without_exposing_it() {
    let token = "secret\ninvalid";
    let pool = AccountPool::new(
        vec![PoolAccount {
            name: "test-invalid".to_string(),
            token: token.to_string(),
        }],
        crate::config::DEFAULT_ANTHROPIC_SWITCH_THRESHOLD,
        None,
    );
    let provider = AnthropicProvider {
        client: reqwest::Client::new(),
        base_url: "http://127.0.0.1:1".to_string(),
        accounts: AccountRelay::with_pool(Some(pool)),
    };

    let response = relay_request(&provider).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let message = String::from_utf8(body.to_vec()).unwrap();
    assert!(message.contains("test-invalid"));
    assert!(!message.contains("secret"));
}

#[tokio::test]
async fn relay_saves_account_usage_and_unknown_windows_without_tokens() {
    let directory = tempfile::TempDir::new().unwrap();
    let (_, responses) = relay_through_mock_with_usage_directory(
        Some(two_account_pool()),
        vec![MOCK_OK_FIVE_HOUR_EXHAUSTED_RESPONSE, MOCK_OK_RESPONSE],
        2,
        Some(directory.path().to_path_buf()),
    )
    .await;
    assert_eq!(
        response_statuses(&responses),
        vec![StatusCode::OK, StatusCode::OK]
    );
    let first_text = std::fs::read_to_string(directory.path().join("test-first.json")).unwrap();
    let first: serde_json::Value = serde_json::from_str(&first_text).unwrap();
    assert_eq!(first["account"], FIRST_ACCOUNT_NAME);
    assert_eq!(first["windows"]["5h"]["utilization"], 1.0);
    assert_eq!(first["windows"]["5h"]["resetAt"], "2100-01-01T00:00:00Z");
    assert_eq!(first["windows"]["5h"]["resetAtUnixSecs"], 4102444800_u64);
    assert_eq!(first["eligible"], false);
    assert!(first["windows"]["7d"].is_null());
    assert!(first["windows"]["7d_oi"].is_null());
    assert!(!first_text.contains(FIRST_TOKEN));
    let second: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("test-second.json")).unwrap())
            .unwrap();
    assert_eq!(second["lastResponseStatus"], 200);
    assert!(second["windows"]["5h"].is_null());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[tokio::test]
async fn usage_write_failure_does_not_fail_the_upstream_request() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let (_, responses) = relay_through_mock_with_usage_directory(
        Some(registered_pool()),
        vec![MOCK_OK_RESPONSE],
        1,
        Some(file.path().to_path_buf()),
    )
    .await;
    assert_eq!(response_statuses(&responses), vec![StatusCode::OK]);
}

#[tokio::test]
async fn first_relay_creates_snapshots_for_all_six_loaded_accounts() {
    let directory = tempfile::TempDir::new().unwrap();
    let usage_directory = directory.path().join("usage");
    let names = ["first", "second", "third", "forth", "five", "six"];
    let pool = AccountPool::new(
        names
            .iter()
            .map(|name| PoolAccount {
                name: name.to_string(),
                token: format!("test-token-{name}"),
            })
            .collect(),
        crate::config::DEFAULT_ANTHROPIC_SWITCH_THRESHOLD,
        None,
    );
    let (_, responses) = relay_through_mock_with_usage_directory(
        Some(pool),
        vec![MOCK_OK_RESPONSE],
        1,
        Some(usage_directory.clone()),
    )
    .await;
    assert_eq!(response_statuses(&responses), vec![StatusCode::OK]);
    assert_eq!(std::fs::read_dir(&usage_directory).unwrap().count(), 6);
    for name in names {
        let snapshot: serde_json::Value = serde_json::from_slice(
            &std::fs::read(usage_directory.join(format!("{name}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot["account"], name);
        assert!(snapshot["windows"]["5h"].is_null());
        assert!(snapshot["windows"]["7d"].is_null());
        assert!(snapshot["windows"]["7d_oi"].is_null());
        if name == "first" {
            assert_eq!(snapshot["lastResponseStatus"], 200);
        } else {
            assert!(snapshot["lastResponseStatus"].is_null());
        }
    }
}

#[tokio::test]
async fn startup_refresh_handles_failures_and_preserves_the_preferred_account() {
    let directory = tempfile::TempDir::new().unwrap();
    let names = ["first", "second", "third", "fourth", "fifth", "sixth"];
    let pool = AccountPool::new(
        names
            .iter()
            .map(|name| PoolAccount {
                name: format!("test-{name}"),
                token: format!("test-token-{name}"),
            })
            .collect(),
        0.98,
        Some("test-second"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..7 {
            let (mut connection, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut connection).await;
            let response: &[u8] = if sent_with(&request, "test-token-first") {
                MOCK_TOO_MANY_REQUESTS_RESPONSE
            } else if sent_with(&request, "test-token-second") && requests.len() < 6 {
                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\nanthropic-ratelimit-unified-5h-utilization: 0.41\r\nanthropic-ratelimit-unified-5h-reset: 4102444800\r\n\r\n{}"
            } else if sent_with(&request, "test-token-third") {
                MOCK_UNAUTHORIZED_RESPONSE
            } else if sent_with(&request, "test-token-fourth") {
                MOCK_OK_FIVE_HOUR_EXHAUSTED_RESPONSE
            } else if sent_with(&request, "test-token-sixth") {
                b"HTTP/1.1 503 Unavailable\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"
            } else {
                MOCK_OK_RESPONSE
            };
            if !sent_with(&request, "test-token-fifth") {
                connection.write_all(response).await.unwrap();
            }
            requests.push(request);
        }
        requests
    });
    let provider = AnthropicProvider {
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        base_url,
        accounts: AccountRelay::with_usage_directory(Some(pool), directory.path().to_path_buf()),
    };
    tokio::time::timeout(Duration::from_secs(5), provider.initialize())
        .await
        .expect("failed probes must not prevent initialization");
    let snapshots: Vec<serde_json::Value> = names
        .iter()
        .map(|name| {
            let text = std::fs::read_to_string(directory.path().join(format!("test-{name}.json")))
                .unwrap();
            assert!(!text.contains("test-token-"));
            serde_json::from_str(&text).unwrap()
        })
        .collect();
    assert_eq!(snapshots[0]["lastResponseStatus"], 429);
    assert_eq!(snapshots[0]["eligible"], false);
    assert_eq!(snapshots[1]["eligible"], true);
    assert_eq!(snapshots[1]["windows"]["5h"]["utilization"], 0.41);
    assert_eq!(snapshots[2]["invalid"], true);
    assert_eq!(snapshots[3]["eligible"], false);
    assert_eq!(snapshots[3]["windows"]["5h"]["utilization"], 1.0);
    assert!(snapshots[4]["lastResponseStatus"].is_null());
    assert!(snapshots[4]["windows"]["5h"].is_null());
    assert_eq!(snapshots[5]["lastResponseStatus"], 503);

    let response = tokio::time::timeout(Duration::from_secs(5), relay_request(&provider))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let after_request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("test-second.json")).unwrap())
            .unwrap();
    assert_eq!(after_request["windows"], snapshots[1]["windows"]);
    let requests = tokio::time::timeout(Duration::from_secs(5), mock)
        .await
        .unwrap()
        .unwrap();
    assert!(sent_with(requests.last().unwrap(), "test-token-second"));
    for name in names {
        assert_eq!(
            requests[..6]
                .iter()
                .filter(|request| sent_with(request, &format!("test-token-{name}")))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn startup_without_registered_accounts_does_not_contact_upstream() {
    let directory = tempfile::TempDir::new().unwrap();
    let relay = AccountRelay::with_usage_directory(None, directory.path().join("usage"));
    relay
        .initialize(&reqwest::Client::new(), "not-a-valid-upstream-url")
        .await;
    assert!(!directory.path().join("usage").exists());
}
