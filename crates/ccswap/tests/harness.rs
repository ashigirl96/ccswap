//! The fake `security` follows upstream tests/test_macos_keychain.py and the
//! isolation guard ports the purpose of tests/test_real_store_guard.py.

mod support;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use support::{CodexAuth, StubResponse, TestEnv, check_isolation};

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
fn guard_refuses_to_launch_with_the_real_launchctl_on_path() {
    let env = TestEnv::new();
    let err = env
        .cmd(["status"])
        .env("PATH", "/usr/bin:/bin")
        .try_run()
        .unwrap_err();
    assert!(err.contains("launchctl on PATH is not the fake"), "{err}");
}

#[test]
fn guard_refuses_to_launch_with_a_claude_outside_the_root_on_path() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new();
    let outside = tempfile::tempdir().unwrap();
    let foreign = outside.path().join("claude");
    std::fs::write(&foreign, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}:{}",
        outside.path().display(),
        env.launchctl().bin_dir().display(),
        env.codex().bin_dir().display()
    );
    let err = env
        .cmd(["status"])
        .env("PATH", &path)
        .try_run()
        .unwrap_err();
    assert!(err.contains("claude on PATH is not the fake"), "{err}");
}

#[test]
fn guard_refuses_to_launch_without_the_fake_codex_on_path() {
    let env = TestEnv::new();
    let path = format!("{}:/usr/bin:/bin", env.launchctl().bin_dir().display());
    let err = env
        .cmd(["codex", "list"])
        .env("PATH", &path)
        .try_run()
        .unwrap_err();
    assert!(err.contains("codex on PATH is not the fake"), "{err}");
}

#[test]
fn guard_refuses_the_real_codex_api_and_codex_home() {
    let env = TestEnv::new();
    let err = env
        .cmd(["codex", "list"])
        .env("CCSWAP_CODEX_API_BASE_URL", "https://chatgpt.com")
        .try_run()
        .unwrap_err();
    assert!(
        err.contains("CCSWAP_CODEX_API_BASE_URL=https://chatgpt.com is not the local stub server"),
        "{err}"
    );

    let err = env
        .cmd(["codex", "list"])
        .env_remove("CCSWAP_CODEX_API_BASE_URL")
        .try_run()
        .unwrap_err();
    assert!(
        err.contains("CCSWAP_CODEX_API_BASE_URL is not set"),
        "{err}"
    );

    let real_home = std::env::var("HOME").unwrap();
    let err = env
        .cmd(["codex", "list"])
        .env("CODEX_HOME", &format!("{real_home}/.codex"))
        .try_run()
        .unwrap_err();
    assert!(err.contains("CODEX_HOME="), "{err}");
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
        ("CCSWAP_CODEX_API_BASE_URL", env.http().base_url()),
        (
            "PATH",
            format!(
                "{}:{}:/usr/bin:/bin",
                env.launchctl().bin_dir().display(),
                env.codex().bin_dir().display()
            ),
        ),
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

fn fake_codex(
    env: &TestEnv,
    args: &[&str],
    codex_home: &std::path::Path,
    stdin: &str,
) -> (i32, String) {
    let mut child = Command::new(env.codex().bin_dir().join("codex"))
        .args(args)
        .env("CODEX_HOME", codex_home)
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
fn fake_codex_login_writes_the_scripted_auth_into_codex_home() {
    let env = TestEnv::new();
    let home = env.root().join("managed");
    let auth = CodexAuth::new("b@example.com", "acct-b").render();
    env.codex().script_login(Some(&auth), 0);

    assert_eq!(fake_codex(&env, &["login"], &home, "").0, 0);
    assert_eq!(
        std::fs::read_to_string(home.join("auth.json")).unwrap(),
        auth
    );
    assert_eq!(
        fake_codex(&env, &["login"], &home, "").0,
        1,
        "each scripted login is used once"
    );

    let calls = env.codex().calls();
    assert_eq!(calls[0].argv, ["login"]);
    assert_eq!(calls[0].codex_home.as_deref(), home.to_str());
}

#[test]
fn fake_codex_app_server_answers_json_rpc_and_rewrites_auth() {
    let env = TestEnv::new();
    let home = env.root().join("managed");
    let refreshed = CodexAuth::new("a@example.com", "acct-a").tag("v2").render();
    env.codex().app_server(json!({
        "auth": refreshed,
        "results": { "account/rateLimits/read": { "rateLimits": { "primary": { "usedPercent": 12 } } } },
    }));
    let requests = [
        json!({ "id": 1, "method": "initialize", "params": {} }),
        json!({ "method": "initialized" }),
        json!({ "id": 2, "method": "account/rateLimits/read" }),
        json!({ "id": 3, "method": "account/unknown" }),
    ];
    let stdin: String = requests.iter().map(|r| format!("{r}\n")).collect();

    let (code, stdout) = fake_codex(
        &env,
        &["-s", "read-only", "-a", "never", "app-server"],
        &home,
        &stdin,
    );

    assert_eq!(code, 0);
    let replies: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(replies.len(), 3, "notifications get no reply: {stdout}");
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(
        replies[1]["result"]["rateLimits"]["primary"]["usedPercent"],
        12
    );
    assert_eq!(replies[2]["error"]["code"], -32601);
    assert_eq!(
        std::fs::read_to_string(home.join("auth.json")).unwrap(),
        refreshed
    );
    assert_eq!(env.codex().app_server_requests().len(), 4);
}

#[test]
fn fake_codex_answers_a_scripted_argv_first() {
    let env = TestEnv::new();
    env.codex()
        .respond("app-server daemon restart", "restarted\n", 0);
    let home = env.root().join("managed");
    assert_eq!(
        fake_codex(&env, &["app-server", "daemon", "restart"], &home, ""),
        (0, "restarted\n".into())
    );
}
