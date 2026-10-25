mod support;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use support::{CodexAuth, StubResponse, TestEnv, base64url};

#[test]
fn status_lists_inference_logins_for_codex_accounts() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    let out = env.run(["claude", "openai", "status"]);
    out.assert_code(0);
    assert!(
        out.stdout.contains("1  a@example.com  not logged in"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("2  b@example.com  not logged in"),
        "{}",
        out.stdout
    );
}

fn form(raw: &str) -> BTreeMap<String, String> {
    let decode = |raw: &str| {
        let mut bytes = raw.bytes();
        let mut out = Vec::new();
        while let Some(b) = bytes.next() {
            out.push(match b {
                b'%' => u8::from_str_radix(
                    &String::from_utf8(bytes.by_ref().take(2).collect()).unwrap(),
                    16,
                )
                .unwrap(),
                b'+' => b' ',
                _ => b,
            });
        }
        String::from_utf8(out).unwrap()
    };
    raw.split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

fn signed_id_token(claims: &Value) -> String {
    let input = format!(
        "{}.{}",
        base64url(br#"{"alg":"RS256","kid":"test"}"#),
        base64url(claims.to_string().as_bytes())
    );
    let mut child = Command::new("/usr/bin/openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai-test-key.pem"
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    format!("{input}.{}", base64url(&out.stdout))
}

fn setup_login(env: &TestEnv, email: Option<&str>) -> Arc<Mutex<String>> {
    setup_login_with_claims(env, email, |_| {})
}

fn setup_login_with_claims(
    env: &TestEnv,
    email: Option<&str>,
    edit: impl Fn(&mut Value) + Send + Sync + 'static,
) -> Arc<Mutex<String>> {
    let nonce = Arc::new(Mutex::new(String::new()));
    let n = nonce.clone();
    let email = email.map(str::to_string);
    env.http().respond_with("POST", "/openai/token", move |req| {
        let fields = form(&req.body);
        assert_eq!(fields["grant_type"], "authorization_code");
        assert_eq!(fields["client_id"], "oaiapp_test");
        assert_eq!(fields["code"], "test-code");
        assert_eq!(fields["resource"], "https://api.openai.com/v1");
        assert!(fields["code_verifier"].len() >= 43);
        let mut claims = json!({"iss":"https://auth.openai.com", "aud":"oaiapp_test", "sub":"test-user", "exp": chrono::Utc::now().timestamp() + 3600, "nonce": n.lock().unwrap().clone()});
        if let Some(email) = &email {
            claims["email"] = email.clone().into();
        }
        edit(&mut claims);
        StubResponse::json(200, &json!({"access_token":"inference-access", "refresh_token":"inference-refresh", "id_token":signed_id_token(&claims), "token_type":"Bearer", "expires_in":3600, "scope":"openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"}).to_string())
    });
    let modulus = "E006DD0B60774D5C5143C9C4782DDC187CDF80DE3321552A081CD35AEE55AD6E357F8F272C49D389A5D3558B61B82B3C1E7357CA1A7560249EC26156A212072994C31E35BBC039567A91540C79B71E0B592ABB3DA776F194C9358A7D631142D3A64D728BF9B87823844B56EE5DFB0D5DDC229AA43BB35F10F24D36DF2970343C02F2883F5D18293AD9D085B3BED104B010C65F034265A38E4BEA9F0A598BAA458C2711411D6BE3DC7B6A3F740C5F88DCF7DF82C377C64FFB031DEF75EA405A54860F9E307115FDE93136FDD6A69573F8C6A19C39A2CFE7C3122247694A440FA7A4B795D9BC25E685D82569D353B56EB6CF7A4ECFC07043024D6E87CB10035973";
    let modulus: Vec<_> = modulus
        .as_bytes()
        .chunks(2)
        .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect();
    env.http().respond("GET", "/openai/jwks", StubResponse::json(200, &json!({"keys":[{"kty":"RSA", "alg":"RS256", "use":"sig", "kid":"test", "n":base64url(&modulus), "e":"AQAB"}]}).to_string()));
    nonce
}

fn login(env: &TestEnv, nonce: &Mutex<String>) -> support::Output {
    let mut running = env.cmd(["claude", "openai", "login", "1"]).spawn();
    let url = running
        .wait_for_line(Duration::from_secs(5), |l| {
            l.starts_with("http://127.0.0.1:")
        })
        .expect("authorization URL");
    let fields = form(url.split_once('?').unwrap().1);
    if fields["client_id"] == "dynamic_agent_client" {
        assert_eq!(fields["agent_name_hint"], "ccswap");
    } else {
        assert_eq!(fields["client_id"], "oaiapp_test");
        assert!(!fields.contains_key("agent_name_hint"));
    }
    assert_eq!(fields["code_challenge_method"], "S256");
    assert!(fields["ext_agent_host_id"].starts_with("urn:uuid:"));
    *nonce.lock().unwrap() = fields["nonce"].clone();
    let callback = fields["redirect_uri"].strip_prefix("http://").unwrap();
    let (addr, path) = callback.split_once('/').unwrap();
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(stream, "GET /{path}?code=test-code&state={}&client_id=oaiapp_test HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n", fields["state"]).unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    running.finish(Duration::from_secs(5))
}

#[test]
fn login_saves_a_separate_private_inference_login_and_logout_removes_it() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let codex_auth = env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let nonce = setup_login(&env, Some("A@Example.com"));
    login(&env, &nonce).assert_code(0);
    let path = env.store_dir().join("openai/logins/acct-a.json");
    let saved = env.read_json(&path);
    assert_eq!(saved["access_token"], "inference-access");
    assert_eq!(saved["refresh_token"], "inference-refresh");
    assert_eq!(saved["client_id"], "oaiapp_test");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        env.codex_managed_auth(1).as_deref(),
        Some(codex_auth.as_str())
    );
    let out = env.run(["claude", "openai", "status"]);
    out.assert_code(0);
    assert!(
        out.stdout.contains("1  a@example.com  logged in"),
        "{}",
        out.stdout
    );
    env.run(["claude", "openai", "logout", "1"]).assert_code(0);
    assert!(!path.exists());
}

#[test]
fn login_checks_email_but_accepts_an_id_token_without_email() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let nonce = setup_login(&env, Some("other@example.com"));
    let out = login(&env, &nonce);
    out.assert_code(1);
    assert!(out.stderr.contains("does not match"), "{}", out.stderr);
    assert!(!env.store_dir().join("openai/logins/acct-a.json").exists());
    let nonce = setup_login(&env, None);
    login(&env, &nonce).assert_code(0);
}

#[test]
fn signed_but_invalid_identity_claims_never_replace_a_login() {
    for (field, value) in [
        ("nonce", json!("other")),
        ("iss", json!("https://attacker.invalid")),
        ("aud", json!("another-client")),
        ("exp", json!(1)),
        ("sub", json!("")),
    ] {
        let env = TestEnv::new();
        env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
        let nonce = setup_login_with_claims(&env, Some("a@example.com"), move |claims| {
            claims[field] = value.clone()
        });
        login(&env, &nonce).assert_code(1);
        assert!(!env.store_dir().join("openai/logins/acct-a.json").exists());
    }
}

#[test]
fn unknown_codex_number_does_not_start_login_or_create_an_openai_store() {
    let env = TestEnv::new();
    for verb in ["login", "logout"] {
        let out = env.run(["claude", "openai", verb, "1"]);
        out.assert_code(1);
        assert!(out.stderr.contains("No Codex account 1"), "{}", out.stderr);
    }
    assert!(!env.store_dir().join("openai").exists());
    assert!(env.http().requests().is_empty());
}

#[test]
fn inference_login_follows_the_codex_account_id_when_numbers_change() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let nonce = setup_login(&env, Some("a@example.com"));
    login(&env, &nonce).assert_code(0);
    let mut ledger = env.codex_ledger();
    let account = ledger["accounts"]
        .as_object_mut()
        .unwrap()
        .remove("1")
        .unwrap();
    ledger["accounts"]["3"] = account;
    ledger["sequence"] = json!([3]);
    env.seed_codex_ledger(ledger);
    let out = env.run(["claude", "openai", "status"]);
    out.assert_code(0);
    assert!(
        out.stdout.contains("3  a@example.com  logged in"),
        "{}",
        out.stdout
    );
    env.run(["claude", "openai", "logout", "3"]).assert_code(0);
    assert!(!env.store_dir().join("openai/logins/acct-a.json").exists());
}

#[test]
fn login_reuses_its_client_and_host_after_logout() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let nonce = setup_login(&env, Some("a@example.com"));
    login(&env, &nonce).assert_code(0);
    let host = env.read_json(env.store_dir().join("openai/host.json"));
    env.run(["claude", "openai", "logout", "1"]).assert_code(0);
    let retained = env.read_json(env.store_dir().join("openai/registrations/acct-a.json"));
    for key in ["access_token", "refresh_token", "id_token"] {
        assert!(retained.get(key).is_none());
    }
    login(&env, &nonce).assert_code(0);
    assert_eq!(
        env.read_json(env.store_dir().join("openai/host.json")),
        host
    );
}

#[test]
fn openai_is_only_a_claude_subcommand() {
    let env = TestEnv::new();
    env.run(["openai", "status"]).assert_code(2);
}

#[test]
fn an_expired_authorization_code_keeps_the_issued_client_for_the_next_sign_in() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let nonce = setup_login(&env, Some("a@example.com"));
    env.http().respond(
        "POST",
        "/openai/token",
        StubResponse::json(400, r#"{"error":"invalid_grant"}"#),
    );
    login(&env, &nonce).assert_code(1);
    let retained = env.read_json(env.store_dir().join("openai/registrations/acct-a.json"));
    assert_eq!(retained["client_id"], "oaiapp_test");
    assert!(retained.get("access_token").is_none());
    assert!(!env.store_dir().join("openai/logins/acct-a.json").exists());
    let nonce = setup_login(&env, Some("a@example.com"));
    login(&env, &nonce).assert_code(0);
}
