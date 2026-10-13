//! Ported from upstream tests/test_macos_keychain_contract.py (real-keychain layer).
//!
//! Runs on macOS GitHub Actions only, or locally with
//! `CCSWAP_TEST_REAL_KEYCHAIN=1`; a wrapper appends a throwaway keychain file to
//! every `security` command so the login keychain and search list are never touched.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, Cmd, Impl, KEYCHAIN_ACCOUNT, StubResponse, TestEnv};

const REAL_SECURITY: &str = "/usr/bin/security";
const BACKUP_SERVICE: &str = "claude-swap";
const EMAILS: [&str; 3] = ["", "a@example.com", "b@example.com"];

fn enabled() -> bool {
    let on = |name: &str, value: &str| std::env::var(name).is_ok_and(|v| v == value);
    let enabled = cfg!(target_os = "macos")
        && (on("GITHUB_ACTIONS", "true") || on("CCSWAP_TEST_REAL_KEYCHAIN", "1"));
    if !enabled {
        eprintln!(
            "skipped: real-keychain contract runs on macOS CI or with CCSWAP_TEST_REAL_KEYCHAIN=1"
        );
    }
    enabled
}

struct ThrowawayKeychain {
    path: PathBuf,
    wrapper: PathBuf,
}

impl ThrowawayKeychain {
    fn create(env: &TestEnv) -> Self {
        let dir = env.root().join("real-keychain");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.keychain-db");
        let kc = path.to_str().unwrap().to_string();
        let kc = kc.as_str();
        security(&["create-keychain", "-p", "", kc]);
        let keychain = ThrowawayKeychain {
            wrapper: dir.join("security"),
            path,
        };
        // No auto-lock: a relocked keychain would block on an unlock dialog.
        security(&["set-keychain-settings", kc]);
        security(&["unlock-keychain", "-p", "", kc]);
        std::fs::write(
            &keychain.wrapper,
            format!(
                "#!/bin/sh\nkc='{kc}'\nif [ \"$1\" = -i ]; then\n  sed \"s|\\$| \\\"$kc\\\"|\" | {REAL_SECURITY} -i\n  exit $?\nfi\nexec {REAL_SECURITY} \"$@\" \"$kc\"\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&keychain.wrapper, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        keychain
    }

    fn cmd<'a>(&self, env: &'a TestEnv, args: &[&str]) -> Cmd<'a> {
        env.cmd(args)
            .env(support::SECURITY_ENV, self.wrapper.to_str().unwrap())
    }

    fn set(&self, service: &str, account: &str, password: &str) {
        security(&[
            "add-generic-password",
            "-U",
            "-a",
            account,
            "-s",
            service,
            "-w",
            password,
            self.path.to_str().unwrap(),
        ]);
    }

    fn get(&self, service: &str, account: &str) -> Option<String> {
        let out = Command::new(REAL_SECURITY)
            .args(["find-generic-password", "-a", account, "-s", service, "-w"])
            .arg(&self.path)
            .output()
            .unwrap();
        match out.status.code() {
            Some(0) => Some(
                String::from_utf8(out.stdout)
                    .unwrap()
                    .trim_end_matches('\n')
                    .to_string(),
            ),
            Some(44) => None,
            code => panic!(
                "security find-generic-password: {code:?} {}",
                String::from_utf8_lossy(&out.stderr)
            ),
        }
    }
}

impl Drop for ThrowawayKeychain {
    fn drop(&mut self) {
        let _ = Command::new(REAL_SECURITY)
            .arg("delete-keychain")
            .arg(&self.path)
            .output();
    }
}

#[track_caller]
fn security(args: &[&str]) {
    let out = Command::new(REAL_SECURITY).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "security {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Credentials tagged with `num`; `padding` grows them past `security -i`'s
/// line buffer so the write takes the argv path.
fn creds(num: usize, padding: usize) -> String {
    json!({"claudeAiOauth": {
        "accessToken": format!("sk-ant-oat01-{num}"),
        "refreshToken": format!("sk-ant-ort01-{num}"),
        "expiresAt": 9_999_999_999_000_i64,
        "padding": "x".repeat(padding),
    }})
    .to_string()
}

fn owner(stored: Option<String>) -> Option<usize> {
    let creds: Value = serde_json::from_str(&stored?).ok()?;
    creds["claudeAiOauth"]["accessToken"]
        .as_str()?
        .strip_prefix("sk-ant-oat01-")?
        .parse()
        .ok()
}

fn backup_account(num: usize) -> String {
    format!("account-{num}-{}", EMAILS[num])
}

fn log_in(env: &TestEnv, kc: &ThrowawayKeychain, num: usize, creds: &str) {
    let email = EMAILS[num];
    env.seed_login(email, "", "");
    kc.set(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, creds);
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(
            200,
            &json!({"account": {"uuid": format!("uuid-{email}"), "email": email}}).to_string(),
        ),
    );
}

#[test]
fn ccswap_round_trips_items_through_the_real_security() {
    if !enabled() {
        return;
    }
    let env = TestEnv::new();
    let kc = ThrowawayKeychain::create(&env);

    log_in(&env, &kc, 1, &creds(1, 0));
    kc.cmd(&env, &["add"])
        .using(Impl::Ccswap)
        .run()
        .assert_code(0);
    assert_eq!(
        kc.get(BACKUP_SERVICE, &backup_account(1)),
        Some(creds(1, 0)),
        "ccswap read the item security wrote and wrote its backup through `security -i`"
    );

    let big = creds(2, 3000);
    log_in(&env, &kc, 2, &big);
    kc.cmd(&env, &["add"])
        .using(Impl::Ccswap)
        .run()
        .assert_code(0);
    assert_eq!(
        kc.get(BACKUP_SERVICE, &backup_account(2)),
        Some(big.clone()),
        "a value too long for `security -i` goes through argv"
    );

    kc.cmd(&env, &["switch", "1"])
        .using(Impl::Ccswap)
        .run()
        .assert_code(0);
    assert_eq!(
        owner(kc.get(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT)),
        Some(1),
        "ccswap updated Claude Code's item in place (-U)"
    );
    assert_eq!(owner(kc.get(BACKUP_SERVICE, &backup_account(2))), Some(2));

    kc.cmd(&env, &["remove", "2"])
        .using(Impl::Ccswap)
        .stdin("y\n")
        .run()
        .assert_code(0);
    assert_eq!(
        kc.get(BACKUP_SERVICE, &backup_account(2)),
        None,
        "ccswap deleted the item"
    );
}

#[test]
fn cswap_and_ccswap_read_each_others_real_keychain_items() {
    if !enabled() {
        return;
    }
    if !support::is_upstream() {
        eprintln!("skipped: set CCSWAP_TEST_UPSTREAM to run the cswap half");
        return;
    }
    let env = TestEnv::new();
    let kc = ThrowawayKeychain::create(&env);

    log_in(&env, &kc, 1, &creds(1, 0));
    kc.cmd(&env, &["add"])
        .using(Impl::Upstream)
        .run()
        .assert_code(0);
    log_in(&env, &kc, 2, &creds(2, 0));
    kc.cmd(&env, &["add"])
        .using(Impl::Ccswap)
        .run()
        .assert_code(0);

    kc.cmd(&env, &["switch", "1"])
        .using(Impl::Ccswap)
        .run()
        .assert_code(0);
    assert_eq!(
        owner(kc.get(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT)),
        Some(1),
        "ccswap activated the backup cswap wrote"
    );
    kc.cmd(&env, &["switch", "2"])
        .using(Impl::Upstream)
        .run()
        .assert_code(0);
    assert_eq!(
        owner(kc.get(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT)),
        Some(2),
        "cswap activated the backup ccswap wrote"
    );
    assert_eq!(
        owner(kc.get(BACKUP_SERVICE, &backup_account(1))),
        Some(1),
        "cswap backed up the item ccswap activated"
    );
}
