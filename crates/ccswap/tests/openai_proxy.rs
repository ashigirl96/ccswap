mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use ccswap::openai::{proxy_token, serve};
use ccswap_claude::openai::InferenceLogins;
use ccswap_codex::CodexPaths;
use ccswap_core::{endpoints::Endpoints, env::Environment, paths::Paths};
use serde_json::{Value, json};
use support::{CodexAuth, StubResponse, TestEnv};

fn setup() -> (TestEnv, Environment) {
    let env = TestEnv::new();
    let auth = CodexAuth::new("a@example.com", "acct-a");
    env.seed_codex_account(1, &auth);
    env.seed_codex_login(&auth);
    let environment = Environment::from_pairs([("HOME", env.home().to_str().unwrap())]);
    InferenceLogins::new(&Paths::new(&environment))
        .write("acct-a", &json!({"access_token":"inference-a"}))
        .unwrap();
    (env, environment)
}

fn start(environment: &Environment, upstream: &str) -> (std::net::SocketAddr, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let paths = Paths::new(environment);
    let token = proxy_token(&paths).unwrap();
    let codex = CodexPaths::new(environment);
    let mut endpoints = Endpoints::new(environment);
    endpoints.openai_responses_url = format!("{upstream}/v1/responses");
    thread::spawn(move || serve(listener, paths, codex, endpoints).unwrap());
    (addr, token)
}

fn request(addr: std::net::SocketAddr, token: &str, body: &Value) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let body = body.to_string();
    write!(stream, "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        body.into(),
    )
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect()
}

fn text_events() -> String {
    sse(&[
        json!({"type":"response.created","response":{"id":"resp-1"}}),
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}),
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"Hello"}),
        json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"Hello"}}),
        json!({"type":"response.completed","response":{"id":"resp-1","usage":{"input_tokens":7,"output_tokens":2}}}),
    ])
}

#[test]
fn text_request_uses_active_inference_login_and_returns_anthropic_events() {
    let (env, environment) = setup();
    env.http().respond(
        "POST",
        "/v1/responses",
        StubResponse::json(200, &text_events()).header("Content-Type", "text/event-stream"),
    );
    let (addr, token) = start(&environment, &env.http().base_url());
    let (status, body) = request(
        addr,
        &token,
        &json!({
            "model":"sonnet", "stream":true, "max_tokens":100, "temperature":0.7,
            "top_p":0.9,"metadata":{"user_id":"private"},"system":[{"type":"text","text":"Be brief."}],
            "messages":[{"role":"user","content":"Hi"}],
        }),
    );
    assert_eq!(status, 200, "{body}");
    let events: Vec<Value> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    assert_eq!(
        events[2]["delta"],
        json!({"type":"text_delta","text":"Hello"})
    );
    assert_eq!(events[4]["usage"]["output_tokens"], 2);
    let requests = env.http().requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("Authorization"),
        Some("Bearer inference-a")
    );
    let sent: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(
        sent,
        json!({"model":"gpt-6.1-sol","instructions":"Be brief.","input":[{"role":"user","content":"Hi"}],"store":false,"stream":true})
    );
}

#[test]
fn token_is_private_persistent_and_required_for_every_request() {
    use std::os::unix::fs::PermissionsExt;
    let (env, environment) = setup();
    let paths = Paths::new(&environment);
    let first = proxy_token(&paths).unwrap();
    let (addr, token) = start(&environment, &env.http().base_url());
    assert_eq!(first, token);
    assert_eq!(proxy_token(&paths).unwrap(), first);
    assert_eq!(first.len(), 64);
    assert_eq!(
        std::fs::metadata(paths.backup_root().join("openai/proxy-token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(paths.backup_root().join("openai"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let (status, body) = request(addr, "incorrect", &json!({"model":"sonnet","messages":[]}));
    assert_eq!(status, 401);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["type"],
        "authentication_error"
    );
    assert!(env.http().requests().is_empty());
}

#[test]
fn unknown_models_and_missing_inference_login_do_not_use_another_account() {
    let (env, environment) = setup();
    let (addr, token) = start(&environment, &env.http().base_url());
    let (status, body) = request(addr, &token, &json!({"model":"not-a-model","messages":[]}));
    assert_eq!(status, 400);
    assert!(body.contains("openai-models.json"));
    let second = CodexAuth::new("b@example.com", "acct-b");
    env.seed_codex_account(2, &second);
    env.seed_codex_login(&second);
    let (status, body) = request(addr, &token, &json!({"model":"sonnet","messages":[]}));
    assert_eq!(status, 401);
    assert!(body.contains("ccswap claude openai login 2"));
    assert!(env.http().requests().is_empty());
}

#[test]
fn every_model_family_and_mapped_gpt_name_is_accepted() {
    let (env, environment) = setup();
    env.http().respond(
        "POST",
        "/v1/responses",
        StubResponse::json(200, &text_events()),
    );
    let (addr, token) = start(&environment, &env.http().base_url());
    for (name, expected) in [
        ("opus", "gpt-6.1-sol"),
        ("sonnet", "gpt-6.1-sol"),
        ("haiku", "gpt-6-luna"),
        ("fable", "gpt-6.1-sol"),
        ("claude-opus-5-5", "gpt-6.1-sol"),
        ("gpt-6.1-sol", "gpt-6.1-sol"),
        ("gpt-6-luna", "gpt-6-luna"),
    ] {
        let (status, body) = request(
            addr,
            &token,
            &json!({"model":name,"messages":[{"role":"user","content":"Hi"}]}),
        );
        assert_eq!(status, 200, "{name}: {body}");
        let message: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(message["model"], expected);
        assert_eq!(message["content"], json!([{"type":"text","text":"Hello"}]));
        let sent: Value =
            serde_json::from_str(&env.http().requests().last().unwrap().body).unwrap();
        assert_eq!(sent["model"], expected);
    }
    assert_eq!(ccswap::openai::model_map().as_object().unwrap().len(), 4);
}

#[test]
fn next_request_uses_the_new_live_account_even_if_ledger_active_is_stale() {
    let (env, environment) = setup();
    let second = CodexAuth::new("b@example.com", "acct-b");
    env.seed_codex_account(2, &second);
    InferenceLogins::new(&Paths::new(&environment))
        .write("acct-b", &json!({"access_token":"inference-b"}))
        .unwrap();
    env.http().respond(
        "POST",
        "/v1/responses",
        StubResponse::json(200, &text_events()),
    );
    let (addr, token) = start(&environment, &env.http().base_url());
    let body = json!({"model":"sonnet","messages":[]});
    assert_eq!(request(addr, &token, &body).0, 200);
    env.seed_codex_login(&second);
    assert_eq!(request(addr, &token, &body).0, 200);
    let calls = env.http().requests();
    assert_eq!(calls[0].header("Authorization"), Some("Bearer inference-a"));
    assert_eq!(calls[1].header("Authorization"), Some("Bearer inference-b"));
}

#[test]
fn streaming_forwards_a_delta_before_openai_finishes() {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    let (_env, environment) = setup();
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", upstream.local_addr().unwrap());
    let (release, wait) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut connection, _) = upstream.accept().unwrap();
        let mut reader = BufReader::new(&connection);
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        drop(reader);
        let events = text_events();
        let (first, rest) = events
            .split_once("event: response.content_part.done")
            .unwrap();
        write!(connection,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n",first.len(),first).unwrap();
        connection.flush().unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        let last = format!("event: response.content_part.done{rest}");
        write!(connection, "{:x}\r\n{last}\r\n0\r\n\r\n", last.len()).unwrap();
    });
    let (addr, token) = start(&environment, &base);
    let mut client = TcpStream::connect(addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let body = json!({"model":"haiku","stream":true,"messages":[]}).to_string();
    write!(client,"POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
    let mut reader = BufReader::new(client);
    let mut received = String::new();
    while !received.contains("Hello") {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0, "{received}");
        received.push_str(&line);
    }
    assert!(!received.contains("message_stop"));
    release.send(()).unwrap();
    reader.read_to_string(&mut received).unwrap();
    assert!(received.contains("message_stop"));
    worker.join().unwrap();
}

#[test]
fn interrupted_streams_fail_without_claiming_a_successful_message_stop() {
    let (env, environment) = setup();
    let events = sse(&[
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"partial"}),
    ]);
    env.http()
        .respond("POST", "/v1/responses", StubResponse::json(200, &events));
    let (addr, token) = start(&environment, &env.http().base_url());
    let (status, body) = request(
        addr,
        &token,
        &json!({"model":"sonnet","stream":true,"messages":[]}),
    );
    assert_eq!(status, 200);
    assert!(body.contains("event: error"));
    assert!(!body.contains("message_stop"));
    let (status, body) = request(addr, &token, &json!({"model":"sonnet","messages":[]}));
    assert_eq!(status, 502);
    assert!(body.contains("response.completed"));
}

#[test]
fn health_requires_authentication_and_count_tokens_is_not_implemented() {
    let (env, environment) = setup();
    let (addr, token) = start(&environment, &env.http().base_url());
    for (path, auth, status) in [
        ("/health", token.as_str(), 200),
        ("/health", "wrong", 401),
        ("/v1/messages/count_tokens", token.as_str(), 404),
    ] {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {auth}\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status} ")),
            "{response}"
        );
    }
    assert!(env.http().requests().is_empty());
}

#[test]
fn malformed_http_framing_is_rejected_without_upstream_requests() {
    let (env, environment) = setup();
    let (addr, token) = start(&environment, &env.http().base_url());
    for framing in [
        "Content-Length: 0\r\nContent-Length: 0",
        "Transfer-Encoding: chunked",
        "Content-Length: 9999999999",
    ] {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write!(stream,"POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n{framing}\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
    }
    assert!(env.http().requests().is_empty());
}

#[test]
fn thinking_is_discarded_in_both_directions() {
    let (env, environment) = setup();
    let response = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","summary":[]}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"hidden reasoning"}),
        json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"Answer"}),
        json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":1}}}),
    ]);
    env.http()
        .respond("POST", "/v1/responses", StubResponse::json(200, &response));
    let (addr, token) = start(&environment, &env.http().base_url());
    let (status, body) = request(
        addr,
        &token,
        &json!({"model":"fable","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"secret"},{"type":"redacted_thinking","data":"encrypted"},{"type":"text","text":"Previous"}]}]}),
    );
    assert_eq!(status, 200, "{body}");
    let message: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(message["content"], json!([{"type":"text","text":"Answer"}]));
    let sent: Value = serde_json::from_str(&env.http().requests()[0].body).unwrap();
    assert_eq!(
        sent["input"],
        json!([{"role":"assistant","content":"Previous"}])
    );
}
