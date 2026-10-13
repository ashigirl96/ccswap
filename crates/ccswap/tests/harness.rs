//! The harness itself: isolation guard and the contracts of its doubles.
//!
//! The fake `security` follows upstream tests/test_macos_keychain.py
//! (exit 44 = not found, secret only with `-w`, `-i` reads commands from stdin
//! with `-X <hex>`); the guard has the purpose of upstream
//! tests/test_real_store_guard.py.

mod support;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};

use support::{StubResponse, TestEnv, check_isolation};

#[test]
fn guard_refuses_to_launch_without_the_security_override() {
    let env = TestEnv::new();
    let err = env
        .cmd(["status"])
        .env_remove("CCSWAP_SECURITY_PATH")
        .try_run()
        .unwrap_err();
    assert!(err.contains("CCSWAP_SECURITY_PATH is not set"), "{err}");
}

#[test]
fn guard_refuses_real_home_and_remote_endpoints() {
    let env = TestEnv::new();
    let real_home = std::env::var("HOME").unwrap();
    let err = env
        .cmd(["status"])
        .env("HOME", &real_home)
        .try_run()
        .unwrap_err();
    assert!(err.contains("HOME="), "{err}");

    let err = env
        .cmd(["status"])
        .env("CCSWAP_API_BASE_URL", "https://api.anthropic.com")
        .try_run()
        .unwrap_err();
    assert!(err.contains("not the local stub server"), "{err}");

    let err = env
        .cmd(["status"])
        .env("CLAUDE_CONFIG_DIR", &format!("{real_home}/.claude"))
        .try_run()
        .unwrap_err();
    assert!(err.contains("CLAUDE_CONFIG_DIR"), "{err}");
}

#[test]
fn guard_accepts_the_default_environment() {
    let env = TestEnv::new();
    let vars: std::collections::BTreeMap<String, String> = [
        ("HOME", env.home().display().to_string()),
        (
            "CCSWAP_SECURITY_PATH",
            env.security().path().display().to_string(),
        ),
        ("CCSWAP_API_BASE_URL", env.http().base_url()),
        ("CCSWAP_OAUTH_TOKEN_URL", env.http().base_url()),
        ("CCSWAP_UPDATE_CHECK_URL", env.http().base_url()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(check_isolation(&vars, &env.root()), Ok(()));
}

fn security(env: &TestEnv, args: &[&str], stdin: &str) -> (i32, String) {
    let mut child = Command::new(env.security().path())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
    )
}

#[test]
fn fake_security_follows_the_wrapper_contract() {
    let env = TestEnv::new();
    let find = [
        "find-generic-password",
        "-a",
        "me",
        "-w",
        "-s",
        "Claude Code",
    ];

    assert_eq!(security(&env, &find, "").0, 44);

    let hex: String = "s3cr\u{e9}t".bytes().map(|b| format!("{b:02x}")).collect();
    let line = format!("add-generic-password -U -a \"me\" -s \"Claude Code\" -X {hex}\n");
    assert_eq!(security(&env, &["-i"], &line).0, 0);
    assert_eq!(
        env.security().item("Claude Code", "me").as_deref(),
        Some("s3cr\u{e9}t")
    );

    assert_eq!(security(&env, &find, ""), (0, "s3cr\u{e9}t\n".into()));
    let (code, out) = security(
        &env,
        &["find-generic-password", "-a", "me", "-s", "Claude Code"],
        "",
    );
    assert_eq!(code, 0);
    assert!(!out.contains("s3cr"), "{out}");

    let delete = ["delete-generic-password", "-a", "me", "-s", "Claude Code"];
    assert_eq!(security(&env, &delete, "").0, 0);
    assert_eq!(security(&env, &delete, "").0, 44);

    let calls = env.security().calls();
    assert_eq!(calls[1].argv, ["-i"]);
    assert_eq!(calls[1].stdin.as_deref(), Some(line.as_str()));

    env.security().fail_with(51);
    assert_eq!(security(&env, &find, "").0, 51);
}

#[test]
fn stub_server_answers_routes_and_records_requests() {
    let env = TestEnv::new();
    env.http().respond(
        "GET",
        "/api/oauth/usage",
        StubResponse::json(200, r#"{"ok":true}"#).header("Retry-After", "7"),
    );
    let addr = env
        .http()
        .base_url()
        .trim_start_matches("http://")
        .to_string();
    let mut stream = TcpStream::connect(&addr).unwrap();
    stream
        .write_all(
            b"GET /api/oauth/usage?x=1 HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer abc\r\n\r\n",
        )
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(response.contains("Retry-After: 7\r\n"));
    assert!(response.ends_with(r#"{"ok":true}"#));

    let requests = env.http().requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path(), "/api/oauth/usage");
    assert_eq!(requests[0].target, "/api/oauth/usage?x=1");
    assert_eq!(requests[0].header("authorization"), Some("Bearer abc"));
}

#[test]
fn fake_claude_is_first_on_path_and_records_calls() {
    let env = TestEnv::new();
    env.claude()
        .respond("auth status --json", r#"{"loggedIn": true}"#, 0);
    let path = format!(
        "{}:{}",
        env.claude_bin_dir().display(),
        std::env::var("PATH").unwrap()
    );
    let out = Command::new("claude")
        .args(["auth", "status", "--json"])
        .env("PATH", &path)
        .env("CLAUDE_CONFIG_DIR", "/tmp/profile-x")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        r#"{"loggedIn": true}"#
    );
    let calls = env.claude().calls();
    assert_eq!(calls[0].argv, ["auth", "status", "--json"]);
    assert_eq!(
        calls[0].env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        Some("/tmp/profile-x")
    );
}
