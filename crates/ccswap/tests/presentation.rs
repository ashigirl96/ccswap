//! Colour switches and the root guard.
//!
//! Ported from upstream tests/test_printer.py::TestColorDetection
//! (no_color_env_disables, no_color_empty_value_disables, force_color_enables),
//! TestThemePalette and cli.py:1366-1370 (root refusal).

mod support;

use serde_json::json;
use support::{TestEnv, sudo_available};

fn managed_env() -> TestEnv {
    let env = TestEnv::new();
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "sequence": [1],
        "accounts": {
            "1": {
                "email": "user@example.com",
                "uuid": "user-uuid",
                "organizationUuid": "org-uuid-5678",
                "organizationName": "Acme Corp",
                "added": "2024-01-01T00:00:00Z"
            }
        }
    }));
    env.seed_login("user@example.com", "org-uuid-5678", "Acme Corp");
    env
}

const PLAIN: &str = "Status: Account-1 (user@example.com [Acme Corp])\n  Total managed accounts: 1\n  no credentials\n";

#[test]
fn piped_output_is_plain_by_default() {
    let env = managed_env();
    let out = env.run(["status"]);
    out.assert_code(0);
    assert_eq!(out.stdout, PLAIN);
}

#[test]
fn force_color_styles_status_with_the_dark_palette() {
    let env = managed_env();
    let out = env.cmd(["status"]).env("FORCE_COLOR", "1").run();
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "\x1b[1mStatus:\x1b[0m \x1b[38;5;173mAccount-1\x1b[0m (user@example.com \x1b[38;5;250m[Acme Corp]\x1b[0m)\n  \x1b[2mTotal managed accounts: 1\x1b[0m\n  \x1b[2mno credentials\x1b[0m\n"
    );
}

#[test]
fn force_color_zero_still_enables_colour() {
    let env = managed_env();
    let out = env.cmd(["status"]).env("FORCE_COLOR", "0").run();
    assert!(
        out.stdout.contains("\x1b[1mStatus:\x1b[0m"),
        "{:?}",
        out.stdout
    );
}

#[test]
fn light_theme_setting_switches_palette() {
    let env = managed_env();
    env.write_json(
        env.store_dir().join("settings.json"),
        &json!({"schemaVersion": 1, "ui": {"theme": "light"}}),
    );
    let out = env.cmd(["status"]).env("FORCE_COLOR", "1").run();
    assert!(
        out.stdout.contains("\x1b[38;2;149;76;42mAccount-1\x1b[0m"),
        "{:?}",
        out.stdout
    );
    assert!(out.stdout.contains("\x1b[38;2;99;93;85m[Acme Corp]\x1b[0m"));
}

#[test]
fn no_color_wins_over_force_color_even_when_empty() {
    let env = managed_env();
    for value in ["1", ""] {
        let out = env
            .cmd(["status"])
            .env("FORCE_COLOR", "1")
            .env("NO_COLOR", value)
            .run();
        assert_eq!(out.stdout, PLAIN, "NO_COLOR={value:?}");
    }
}

#[test]
fn errors_are_red_on_stderr_when_coloured() {
    let env = managed_env();
    std::fs::write(env.store_dir().join("sequence.json"), "[]").unwrap();
    let out = env.cmd(["status"]).env("FORCE_COLOR", "1").run();
    out.assert_code(1);
    assert!(
        out.stderr.starts_with("\x1b[31mError: "),
        "{:?}",
        out.stderr
    );
    assert!(out.stderr.ends_with("\x1b[0m\n"), "{:?}", out.stderr);
}

#[test]
fn running_as_root_is_refused() {
    if !sudo_available() {
        eprintln!("skipped: needs passwordless sudo (runs on CI)");
        return;
    }
    let env = managed_env();
    let out = env.cmd(["status"]).sudo().run();
    give_back_to_user(&env);
    out.assert_code(1);
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "Error: Do not run this script as root (unless running in a container)\n"
    );
}

/// Upstream writes its log as root before refusing; hand the files back so
/// the temp dir can be removed.
fn give_back_to_user(env: &TestEnv) {
    let uid = std::process::Command::new("id").arg("-u").output().unwrap();
    let uid = String::from_utf8(uid.stdout).unwrap();
    let _ = std::process::Command::new("sudo")
        .args(["-n", "chown", "-R", uid.trim()])
        .arg(env.root())
        .status();
}
