//! Running Python `cswap` and `ccswap` side by side on one store (#1's
//! compatibility bar): each operates on what the other wrote, and the two
//! queue on the same store and Claude Code locks.
//!
//! Every case here needs `CCSWAP_TEST_UPSTREAM` and is skipped without it.
//! To cover a new command, add a [`Step`] to a scenario (or a new scenario):
//! [`play`] checks after every step that both implementations read the store
//! identically through every entry of [`READERS`], so a step only states its
//! own outcome. A new read-only JSON command goes into [`READERS`].

mod support;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, Impl, KEYCHAIN_ACCOUNT, StubResponse, TestEnv, is_upstream};

const BACKUP_SERVICE: &str = "claude-swap";
const EMAILS: [&str; 4] = ["", "a@example.com", "b@example.com", "c@example.com"];

fn creds(num: usize) -> String {
    json!({"claudeAiOauth": {
        "accessToken": format!("sk-ant-oat01-{num}"),
        "refreshToken": format!("sk-ant-ort01-{num}"),
        "expiresAt": 9_999_999_999_000_i64,
    }})
    .to_string()
}

/// Claude Code logged in as account `num` (live Keychain item, `.claude.json`,
/// and a profile endpoint that resolves its token).
fn log_in(env: &TestEnv, num: usize) {
    let email = EMAILS[num];
    env.seed_login(email, &format!("org-{num}"), &format!("Org {num}"));
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &creds(num));
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(
            200,
            &json!({"account": {"uuid": format!("uuid-{email}"), "email": email},
                    "organization": {"uuid": format!("org-{num}")}})
            .to_string(),
        ),
    );
}

enum Step {
    /// Run `args` with one implementation; it must exit 0.
    Run(Impl, &'static [&'static str]),
    /// Claude Code `/login` as this account.
    LogIn(usize),
}

/// Run `steps`, checking after each that cswap and ccswap read the same store.
fn play(env: &TestEnv, steps: &[Step]) {
    for step in steps {
        match step {
            Step::LogIn(num) => log_in(env, *num),
            Step::Run(which, args) => {
                let out = env.cmd(*args).using(*which).run();
                assert_eq!(
                    out.code, 0,
                    "{which:?} {args:?}\nstdout:\n{}\nstderr:\n{}",
                    out.stdout, out.stderr
                );
                assert_reads_agree(env, &format!("after {which:?} {args:?}"));
            }
        }
    }
}

/// Read-only commands whose JSON both implementations must agree on, with
/// the fields that legitimately differ between runs blanked.
type Reader = (&'static [&'static str], fn(&mut Value));

const READERS: &[Reader] = &[
    (&["export", "-", "--full"], |envelope| {
        envelope["exportedAt"] = Value::Null;
        envelope["swapVersion"] = Value::Null;
    }),
    (&["list", "--json"], |listing| {
        for row in listing["accounts"].as_array_mut().into_iter().flatten() {
            row["usage"] = Value::Null;
        }
    }),
];

fn assert_reads_agree(env: &TestEnv, context: &str) {
    for (args, blank_volatile) in READERS {
        let read = |which: Impl| {
            let out = env.cmd(*args).using(which).run();
            out.assert_code(0);
            let mut value = out.json();
            blank_volatile(&mut value);
            value
        };
        assert_eq!(
            read(Impl::Ccswap),
            read(Impl::Upstream),
            "{args:?} {context}"
        );
    }
}

/// What cswap's `list --json` reports per slot: `(number, email, alias, disabled, active)`.
fn cswap_listing(env: &TestEnv) -> Vec<(u64, String, Value, bool, bool)> {
    let out = env.cmd(["list", "--json"]).using(Impl::Upstream).run();
    out.assert_code(0);
    out.json()["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["number"].as_u64().unwrap(),
                a["email"].as_str().unwrap().to_string(),
                a["alias"].clone(),
                a["disabled"].as_bool().unwrap_or(false),
                a["active"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

/// Which account's token a stored credential holds (writers may re-serialize it).
fn token_owner(stored: Option<String>) -> Option<usize> {
    let creds: Value = serde_json::from_str(&stored?).ok()?;
    creds["claudeAiOauth"]["accessToken"]
        .as_str()?
        .strip_prefix("sk-ant-oat01-")?
        .parse()
        .ok()
}

fn active_owner(env: &TestEnv) -> Option<usize> {
    token_owner(env.security().item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT))
}

fn backup_owner(env: &TestEnv, num: usize) -> Option<usize> {
    token_owner(
        env.security()
            .item(BACKUP_SERVICE, &format!("account-{num}-{}", EMAILS[num])),
    )
}

fn skip_without_upstream() -> bool {
    if !is_upstream() {
        eprintln!("skipped: set CCSWAP_TEST_UPSTREAM to run the cswap compatibility cases");
    }
    !is_upstream()
}

/// Accounts 1-3 registered (by cswap), account 3 live.
fn three_accounts() -> TestEnv {
    let env = TestEnv::new();
    for num in 1..=3 {
        log_in(&env, num);
        env.cmd(["add"]).using(Impl::Upstream).run().assert_code(0);
    }
    env
}

/// How many processes have `path` open (`lsof -t`).
fn openers(path: &Path) -> usize {
    let out = Command::new("lsof")
        .arg("-t")
        .arg(path)
        .output()
        .expect("run lsof");
    String::from_utf8_lossy(&out.stdout).lines().count()
}

fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The store is consistent: every slot holds its own account's token and the
/// live token belongs to the slot cswap lists as active.
#[track_caller]
fn assert_store_intact(env: &TestEnv) {
    let listing = cswap_listing(env);
    for (num, ..) in &listing {
        let num = *num as usize;
        assert_eq!(backup_owner(env, num), Some(num), "slot {num}");
    }
    let active: Vec<usize> = listing
        .iter()
        .filter(|row| row.4)
        .map(|row| row.0 as usize)
        .collect();
    assert_eq!(active.len(), 1, "{listing:?}");
    assert_eq!(active_owner(env), Some(active[0]), "{listing:?}");
    assert_reads_agree(env, "after the concurrent switches");
}

fn queues_behind_the_other_holding_the_locks(holder: Impl, waiter: Impl) {
    let env = three_accounts();
    let home = env.home();
    let store_lock = env.store_dir().join(".lock");
    env.security().pause_next("add-generic-password");

    std::thread::scope(|s| {
        let first = s.spawn(|| env.cmd(["switch", "1"]).using(holder).run());
        assert!(
            env.security().wait_until_paused(Duration::from_secs(10)),
            "{holder:?} never reached its backup write"
        );
        for lock in [
            ".claude/.oauth_refresh.lock",
            ".claude.lock",
            ".claude.json.lock",
        ] {
            assert!(
                home.join(lock).is_dir(),
                "{holder:?} holds Claude Code's {lock} where {waiter:?} looks for it"
            );
        }
        let second = s.spawn(|| env.cmd(["switch", "2"]).using(waiter).run());
        wait_for(
            &format!("{waiter:?} to wait on the store lock"),
            Duration::from_secs(4),
            || openers(&store_lock) >= 2,
        );
        assert!(
            !second.is_finished(),
            "{waiter:?} did not wait for the lock"
        );
        env.security().release();

        first.join().unwrap().assert_code(0);
        second.join().unwrap().assert_code(0);
    });

    assert_eq!(
        active_owner(&env),
        Some(2),
        "the waiter ran after the holder"
    );
    assert_eq!(backup_owner(&env, 1), Some(1));
    assert_store_intact(&env);
}

#[test]
fn ccswap_waits_for_cswap_holding_the_store_and_claude_code_locks() {
    if skip_without_upstream() {
        return;
    }
    queues_behind_the_other_holding_the_locks(Impl::Upstream, Impl::Ccswap);
}

#[test]
fn cswap_waits_for_ccswap_holding_the_store_and_claude_code_locks() {
    if skip_without_upstream() {
        return;
    }
    queues_behind_the_other_holding_the_locks(Impl::Ccswap, Impl::Upstream);
}

#[test]
fn concurrent_switches_from_both_leave_the_store_intact() {
    if skip_without_upstream() {
        return;
    }
    let env = three_accounts();
    let runs: Vec<(Impl, &str)> = (0..6)
        .map(|i| {
            let which = if i % 2 == 0 {
                Impl::Upstream
            } else {
                Impl::Ccswap
            };
            (which, ["1", "2", "3"][i % 3])
        })
        .collect();
    let outs: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = runs
            .iter()
            .map(|(which, target)| s.spawn(|| env.cmd(["switch", target]).using(*which).run()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for ((which, target), out) in runs.iter().zip(&outs) {
        assert_eq!(
            out.code, 0,
            "{which:?} switch {target}\nstdout:\n{}\nstderr:\n{}",
            out.stdout, out.stderr
        );
    }
    assert_store_intact(&env);
}

#[test]
fn slots_renumbered_by_one_are_renumbered_again_by_the_other() {
    if skip_without_upstream() {
        return;
    }
    let env = three_accounts();
    let (cswap, ccswap) = (Impl::Upstream, Impl::Ccswap);
    play(
        &env,
        &[
            Step::Run(ccswap, &["alias", "1", "dev"]),
            Step::Run(cswap, &["swap", "dev", "2"]),
            Step::Run(ccswap, &["move", "dev", "5"]),
            Step::Run(cswap, &["move", "dev", "1"]),
            Step::Run(ccswap, &["swap", "1", "3"]),
        ],
    );
    let listing: Vec<(u64, String)> = cswap_listing(&env)
        .into_iter()
        .map(|row| (row.0, row.1))
        .collect();
    assert_eq!(
        listing,
        vec![
            (1, EMAILS[3].into()),
            (3, EMAILS[1].into()),
            (5, EMAILS[2].into()),
        ]
    );
}

#[test]
fn ccswap_operates_on_a_cswap_store_and_cswap_reads_the_result() {
    if skip_without_upstream() {
        return;
    }
    let env = TestEnv::new();
    let (cswap, ccswap) = (Impl::Upstream, Impl::Ccswap);
    play(
        &env,
        &[
            Step::LogIn(1),
            Step::Run(cswap, &["add"]),
            Step::LogIn(2),
            Step::Run(cswap, &["add"]),
            Step::LogIn(3),
            Step::Run(ccswap, &["add", "--alias", "ci"]),
            Step::Run(ccswap, &["switch", "1"]),
            Step::Run(ccswap, &["alias", "2", "work"]),
            Step::Run(ccswap, &["disable", "3"]),
        ],
    );

    assert_eq!(
        cswap_listing(&env),
        vec![
            (1, EMAILS[1].into(), Value::Null, false, true),
            (2, EMAILS[2].into(), json!("work"), false, false),
            (3, EMAILS[3].into(), json!("ci"), true, false),
        ]
    );
    assert_eq!(active_owner(&env), Some(1));
    for num in 2..=3 {
        assert_eq!(backup_owner(&env, num), Some(num), "slot {num}");
    }
    let out = env.cmd(["switch"]).using(cswap).run();
    out.assert_code(0);
    assert_eq!(
        active_owner(&env),
        Some(2),
        "cswap skips the slot ccswap disabled"
    );
}

#[test]
fn cswap_operates_on_a_ccswap_store_and_ccswap_reads_the_result() {
    if skip_without_upstream() {
        return;
    }
    let env = TestEnv::new();
    let (cswap, ccswap) = (Impl::Upstream, Impl::Ccswap);
    play(
        &env,
        &[
            Step::LogIn(1),
            Step::Run(ccswap, &["add"]),
            Step::LogIn(2),
            Step::Run(ccswap, &["add"]),
            Step::LogIn(3),
            Step::Run(cswap, &["add", "--alias", "ci"]),
            Step::Run(cswap, &["switch", "1"]),
            Step::Run(cswap, &["alias", "2", "work"]),
            Step::Run(cswap, &["disable", "3"]),
        ],
    );

    let status = env.cmd(["status", "--json"]).using(ccswap).run();
    status.assert_code(0);
    assert_eq!(
        status.json()["active"]["email"],
        EMAILS[1],
        "{}",
        status.stdout
    );
    let aliases = env.cmd(["alias"]).using(ccswap).run();
    aliases.assert_code(0);
    assert!(aliases.stdout.contains("work"), "{}", aliases.stdout);
    assert!(aliases.stdout.contains("ci"), "{}", aliases.stdout);

    let out = env.cmd(["switch", "work"]).using(ccswap).run();
    out.assert_code(0);
    assert_eq!(active_owner(&env), Some(2), "ccswap resolves cswap's alias");
    assert_eq!(backup_owner(&env, 1), Some(1));
    let out = env.cmd(["switch"]).using(ccswap).run();
    out.assert_code(0);
    assert_eq!(
        active_owner(&env),
        Some(1),
        "ccswap skips the slot cswap disabled"
    );
    assert_reads_agree(&env, "after the ccswap switches");
}
