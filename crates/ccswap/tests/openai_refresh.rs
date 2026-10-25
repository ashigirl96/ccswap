mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use ccswap::openai::{proxy_token, serve};
use ccswap_claude::openai::InferenceLogins;
use ccswap_codex::CodexPaths;
use ccswap_core::{endpoints::Endpoints, env::Environment, paths::Paths};
use serde_json::{Value, json};
use support::{CodexAuth, StubResponse, TestEnv};

fn setup(expires_at: i64) -> (TestEnv, SocketAddr, String) {
    let env = TestEnv::new();
    let auth = CodexAuth::new("a@example.com", "acct-a");
    env.seed_codex_account(1, &auth);
    env.seed_codex_login(&auth);
    let environment = Environment::from_pairs([("HOME", env.home().to_str().unwrap())]);
    let paths = Paths::new(&environment);
    InferenceLogins::new(&paths)
        .write(
            "acct-a",
            &json!({
                "access_token":"inference-old", "refresh_token":"refresh-old",
                "client_id":"oaiapp-ccswap", "expires_at":expires_at,
                "scope":"chatgpt.tokens.use.direct", "subject":"user-a"
            }),
        )
        .unwrap();
    let token = proxy_token(&paths).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let codex = CodexPaths::new(&environment);
    let mut endpoints = Endpoints::new(&environment);
    endpoints.openai_responses_url = format!("{}/v1/responses", env.http().base_url());
    endpoints.openai_token_url = format!("{}/oauth/token", env.http().base_url());
    thread::spawn(move || serve(listener, paths, codex, endpoints).unwrap());
    (env, addr, token)
}

fn request(addr: SocketAddr, token: &str, streaming: bool) -> (u16, String, String) {
    let mut connection = TcpStream::connect(addr).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let body =
        json!({"model":"sonnet","stream":streaming,"messages":[{"role":"user","content":"hello"}]})
            .to_string();
    write!(connection,"POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
    let mut response = String::new();
    connection.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    (
        headers.split_whitespace().nth(1).unwrap().parse().unwrap(),
        headers.into(),
        body.into(),
    )
}

fn completed() -> StubResponse {
    StubResponse::json(200,"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n").header("Content-Type", "text/event-stream")
}

fn refreshed() -> StubResponse {
    StubResponse::json(200, &json!({"access_token":"inference-new","refresh_token":"refresh-new","token_type":"Bearer","expires_in":3600,"scope":"chatgpt.tokens.use.direct"}).to_string())
}

#[test]
fn expired_inference_login_is_refreshed_and_saved_before_inference() {
    let (env, addr, token) = setup(chrono::Utc::now().timestamp() - 10);
    env.http().respond("POST", "/oauth/token", refreshed());
    env.http().respond_with("POST", "/v1/responses", |request| {
        assert_eq!(
            request.header("Authorization"),
            Some("Bearer inference-new")
        );
        completed()
    });
    let (status, _, body) = request(addr, &token, false);
    assert_eq!(status, 200, "{body}");
    let calls = env.http().requests();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].path(), "/oauth/token");
    assert!(calls[0].body.contains("grant_type=refresh_token"));
    assert!(calls[0].body.contains("refresh_token=refresh-old"));
    assert!(calls[0].body.contains("client_id=oaiapp-ccswap"));
    assert!(
        calls[0]
            .body
            .contains("resource=https%3A%2F%2Fapi.openai.com%2Fv1")
    );
    let login: Value = serde_json::from_slice(
        &std::fs::read(env.store_dir().join("openai/logins/acct-a.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(login["refresh_token"], "refresh-new");
    assert_eq!(login["access_token"], "inference-new");
    assert_eq!(login["subject"], "user-a");
    assert!(login["expires_at"].as_i64().unwrap() > chrono::Utc::now().timestamp() + 3500);
}

#[test]
fn rejected_access_token_refreshes_once_and_retries_with_new_token() {
    let (env, addr, token) = setup(chrono::Utc::now().timestamp() + 3600);
    env.http().respond("POST", "/oauth/token", refreshed());
    env.http().respond_with("POST", "/v1/responses", |request| {
        match request.header("Authorization") {
            Some("Bearer inference-old") => {
                StubResponse::json(401, r#"{"error":{"code":"invalid_token"}}"#)
            }
            Some("Bearer inference-new") => completed(),
            other => panic!("Unexpected authorization: {other:?}"),
        }
    });
    let (status, _, body) = request(addr, &token, false);
    assert_eq!(status, 200, "{body}");
    let calls = env.http().requests();
    assert_eq!(
        calls.iter().map(|r| r.path()).collect::<Vec<_>>(),
        ["/v1/responses", "/oauth/token", "/v1/responses"]
    );
}

#[test]
fn concurrent_expired_requests_share_one_rotating_refresh() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    let (env, addr, token) = setup(chrono::Utc::now().timestamp() - 10);
    let refreshes = Arc::new(AtomicUsize::new(0));
    let count = refreshes.clone();
    env.http().respond_with("POST", "/oauth/token", move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(100));
        refreshed()
    });
    env.http().respond_with("POST", "/v1/responses", |request| {
        assert_eq!(
            request.header("Authorization"),
            Some("Bearer inference-new")
        );
        completed()
    });
    let ready = Arc::new(Barrier::new(3));
    let requests: Vec<_> = (0..2)
        .map(|_| {
            let ready = ready.clone();
            let token = token.clone();
            thread::spawn(move || {
                ready.wait();
                request(addr, &token, false)
            })
        })
        .collect();
    ready.wait();
    for result in requests {
        let (status, _, body) = result.join().unwrap();
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
}

#[test]
fn failed_refresh_requires_login_again_without_inference_or_token_loss() {
    let (env, addr, token) = setup(chrono::Utc::now().timestamp() - 10);
    let file = env.store_dir().join("openai/logins/acct-a.json");
    let before = std::fs::read(&file).unwrap();
    env.http().respond(
        "POST",
        "/oauth/token",
        StubResponse::json(400, r#"{"error":"invalid_grant"}"#),
    );
    let (status, _, body) = request(addr, &token, false);
    assert_eq!(status, 401, "{body}");
    let error: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(error["error"]["type"], "authentication_error");
    assert!(body.contains("ccswap claude openai login 1"));
    assert_eq!(std::fs::read(file).unwrap(), before);
    assert_eq!(
        env.http()
            .requests()
            .iter()
            .map(|r| r.path())
            .collect::<Vec<_>>(),
        ["/oauth/token"]
    );
}

#[test]
fn subscription_errors_use_anthropic_status_and_integer_retry_after() {
    for (status, code, kind) in [
        (
            429,
            "subscription_sharing_usage_limit_exceeded",
            "rate_limit_error",
        ),
        (
            503,
            "subscription_sharing_usage_unavailable",
            "overloaded_error",
        ),
    ] {
        let (env, addr, token) = setup(chrono::Utc::now().timestamp() + 3600);
        env.http().respond(
            "POST",
            "/v1/responses",
            StubResponse::json(
                status,
                &json!({"error":{"code":code,"message":"Try later"}}).to_string(),
            )
            .header("retry-after", "2.5"),
        );
        let (actual, headers, body) = request(addr, &token, false);
        assert_eq!(actual, status, "{body}");
        assert!(
            headers.lines().any(|line| line == "Retry-After: 3"),
            "{headers}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["type"],
            kind
        );
    }
}

#[test]
fn subscription_failure_after_stream_start_is_an_anthropic_error_event() {
    for (code, kind) in [
        (
            "subscription_sharing_usage_limit_exceeded",
            "rate_limit_error",
        ),
        ("subscription_sharing_usage_unavailable", "overloaded_error"),
    ] {
        let (env, addr, token) = setup(chrono::Utc::now().timestamp() + 3600);
        let events = format!(
            "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-1\"}}}}\n\nevent: response.failed\ndata: {}\n\n",
            json!({"type":"response.failed","response":{"error":{"code":code}}})
        );
        env.http().respond(
            "POST",
            "/v1/responses",
            StubResponse::json(200, &events).header("Content-Type", "text/event-stream"),
        );
        let (status, _, body) = request(addr, &token, true);
        assert_eq!(status, 200);
        let events: Vec<Value> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        assert_eq!(events[0]["type"], "message_start");
        assert_eq!(events.last().unwrap()["type"], "error");
        assert_eq!(events.last().unwrap()["error"]["type"], kind);
        assert!(!events.iter().any(|event| event["type"] == "message_stop"));
    }
}

#[test]
fn concurrent_unauthorized_requests_reuse_the_first_refresh() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    let (env, addr, token) = setup(chrono::Utc::now().timestamp() + 3600);
    let rejected = Arc::new(Barrier::new(2));
    env.http()
        .respond_with("POST", "/v1/responses", move |request| {
            if request.header("Authorization") == Some("Bearer inference-old") {
                rejected.wait();
                StubResponse::json(401, r#"{"error":{"code":"invalid_token"}}"#)
            } else {
                assert_eq!(
                    request.header("Authorization"),
                    Some("Bearer inference-new")
                );
                completed()
            }
        });
    let refreshes = Arc::new(AtomicUsize::new(0));
    let count = refreshes.clone();
    env.http().respond_with("POST", "/oauth/token", move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(100));
        refreshed()
    });
    let requests: Vec<_> = (0..2)
        .map(|_| {
            let token = token.clone();
            thread::spawn(move || request(addr, &token, false))
        })
        .collect();
    for result in requests {
        let (status, _, body) = result.join().unwrap();
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
}

#[test]
fn a_second_unauthorized_response_does_not_loop_refreshing() {
    let (env, addr, token) = setup(chrono::Utc::now().timestamp() + 3600);
    env.http().respond("POST", "/oauth/token", refreshed());
    env.http()
        .respond("POST", "/v1/responses", StubResponse::json(401, "{}"));
    let (status, _, body) = request(addr, &token, false);
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("ccswap claude openai login 1"));
    assert_eq!(
        env.http()
            .requests()
            .iter()
            .map(|r| r.path())
            .collect::<Vec<_>>(),
        ["/v1/responses", "/oauth/token", "/v1/responses"]
    );
}

#[test]
fn an_incomplete_refresh_response_does_not_overwrite_credentials() {
    let (env, addr, token) = setup(chrono::Utc::now().timestamp() - 10);
    let file = env.store_dir().join("openai/logins/acct-a.json");
    let before = std::fs::read(&file).unwrap();
    env.http().respond(
        "POST",
        "/oauth/token",
        StubResponse::json(
            200,
            r#"{"access_token":"incomplete","token_type":"Bearer","expires_in":3600}"#,
        ),
    );
    let (status, _, body) = request(addr, &token, false);
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("ccswap claude openai login 1"));
    assert_eq!(std::fs::read(file).unwrap(), before);
    assert_eq!(env.http().requests().len(), 1);
}
