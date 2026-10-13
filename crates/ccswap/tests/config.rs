//! Ported from upstream tests/test_config_cli.py (all classes) and the
//! CLI-observable parts of tests/test_settings.py (TestLoadSettings,
//! TestUiSettings, TestSetUnsetSetting, TestEffectiveSettings,
//! TestAtomicWriteThroughSymlink).

mod support;

use serde_json::{Value, json};
use support::{TestEnv, prog};

fn settings_path(env: &TestEnv) -> std::path::PathBuf {
    env.store_dir().join("settings.json")
}

#[test]
fn path_prints_settings_location_without_creating_the_store() {
    let env = TestEnv::new();
    let out = env.run(["config", "path"]);
    out.assert_code(0);
    assert_eq!(out.stdout, format!("{}\n", settings_path(&env).display()));
    assert!(!env.store_dir().exists());
}

const KEYS: [&str; 9] = [
    "autoswitch.threshold",
    "autoswitch.intervalSeconds",
    "autoswitch.cooldownSeconds",
    "autoswitch.hysteresisPct",
    "autoswitch.strategy",
    "autoswitch.includeApiKeyAccounts",
    "autoswitch.unhealthyTicks",
    "autoswitch.model",
    "ui.theme",
];

fn seed_settings(env: &TestEnv, text: &str) -> std::path::PathBuf {
    env.write_file(settings_path(env), text)
}

fn file_text(env: &TestEnv) -> String {
    std::fs::read_to_string(settings_path(env)).unwrap()
}

fn file_json(env: &TestEnv) -> Value {
    env.read_json(settings_path(env))
}

fn get_json(env: &TestEnv, key: &str) -> Value {
    let out = env.run(["config", "get", key, "--json"]);
    out.assert_code(0);
    out.json()["value"].clone()
}

fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn is_symlink(path: &std::path::Path) -> bool {
    path.symlink_metadata().unwrap().file_type().is_symlink()
}

#[test]
fn list_shows_every_key_as_default() {
    let env = TestEnv::new();
    let out = env.run(["config"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "autoswitch.threshold              90      (default)\n\
         autoswitch.intervalSeconds        60      (default)\n\
         autoswitch.cooldownSeconds        300     (default)\n\
         autoswitch.hysteresisPct          10      (default)\n\
         autoswitch.strategy               best    (default)\n\
         autoswitch.includeApiKeyAccounts  false   (default)\n\
         autoswitch.unhealthyTicks         3       (default)\n\
         autoswitch.model                  (none)  (default)\n\
         ui.theme                          auto    (default)\n"
    );
    assert_eq!(env.run(["config", "list"]).stdout, out.stdout);
    assert!(!env.store_dir().exists());
}

#[test]
fn list_json_reports_typed_values() {
    let env = TestEnv::new();
    let out = env.run(["config", "--json"]);
    out.assert_code(0);
    let defaults = [
        json!(90.0),
        json!(60.0),
        json!(300.0),
        json!(10.0),
        json!("best"),
        json!(false),
        json!(3),
        Value::Null,
        json!("auto"),
    ];
    let settings: Vec<Value> = KEYS
        .iter()
        .zip(defaults)
        .map(|(k, v)| json!({"key": k, "value": v, "isSet": false}))
        .collect();
    let expected = json!({
        "schemaVersion": 1,
        "path": settings_path(&env).display().to_string(),
        "settings": settings,
    });
    assert_eq!(out.json(), expected);
    assert!(out.stdout.contains("\"value\": 90.0,"));
    assert!(out.stdout.contains("\"value\": 3,"));
    assert_eq!(env.run(["config", "list", "--json"]).stdout, out.stdout);
}

#[test]
fn set_key_is_no_longer_marked_default_even_when_equal_to_it() {
    let env = TestEnv::new();
    env.run(["config", "set", "autoswitch.cooldownSeconds", "600"])
        .assert_code(0);
    env.run(["config", "set", "autoswitch.threshold", "90"])
        .assert_code(0);
    let out = env.run(["config"]);
    out.assert_code(0);
    let line = |key: &str| {
        out.stdout
            .lines()
            .find(|l| l.starts_with(key))
            .unwrap()
            .to_string()
    };
    assert_eq!(
        line("autoswitch.cooldownSeconds"),
        "autoswitch.cooldownSeconds        600   "
    );
    assert_eq!(
        line("autoswitch.threshold"),
        "autoswitch.threshold              90    "
    );
    assert_eq!(out.stdout.matches("(default)").count(), 7);
}

#[test]
fn set_then_get() {
    let env = TestEnv::new();
    let out = env.run(["config", "set", "autoswitch.threshold", "80"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "autoswitch.threshold = 80\n");
    let out = env.run(["config", "get", "autoswitch.threshold"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "80\n");
    for args in [
        ["config", "get", "autoswitch.threshold", "--json"],
        ["config", "--json", "get", "autoswitch.threshold"],
    ] {
        let out = env.run(args);
        out.assert_code(0);
        assert_eq!(
            out.stdout,
            "{\n  \"schemaVersion\": 1,\n  \"key\": \"autoswitch.threshold\",\n  \"value\": 80.0,\n  \"isSet\": true\n}\n"
        );
    }
}

#[test]
fn set_writes_only_that_key_with_upstream_format_and_modes() {
    let env = TestEnv::new();
    env.run(["config", "set", "autoswitch.threshold", "80"])
        .assert_code(0);
    assert_eq!(
        file_text(&env),
        "{\n  \"schemaVersion\": 1,\n  \"autoswitch\": {\n    \"threshold\": 80.0\n  }\n}"
    );
    assert_eq!(mode(&settings_path(&env)), 0o600);
    assert_eq!(mode(&env.store_dir()), 0o700);
    let entries: Vec<_> = std::fs::read_dir(env.store_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("settings.json")]);
}

#[test]
fn set_stores_each_kind_with_its_type() {
    let env = TestEnv::new();
    let cases = [
        ("autoswitch.unhealthyTicks", "5", "5", json!(5)),
        ("autoswitch.unhealthyTicks", " 1_0 ", "10", json!(10)),
        (
            "autoswitch.includeApiKeyAccounts",
            "no",
            "false",
            json!(false),
        ),
        (
            "autoswitch.includeApiKeyAccounts",
            "FALSE",
            "false",
            json!(false),
        ),
        (
            "autoswitch.includeApiKeyAccounts",
            " Yes ",
            "true",
            json!(true),
        ),
        (
            "autoswitch.model",
            "  Fable,Opus ",
            "Fable,Opus",
            json!("Fable,Opus"),
        ),
        (
            "autoswitch.strategy",
            "consume-first",
            "consume-first",
            json!("consume-first"),
        ),
        ("autoswitch.hysteresisPct", "12.5", "12.5", json!(12.5)),
        ("autoswitch.cooldownSeconds", "1e2", "100", json!(100.0)),
        ("ui.theme", "light", "light", json!("light")),
    ];
    for (key, raw, shown, stored) in cases {
        let out = env.run(["config", "set", key, raw]);
        out.assert_code(0);
        assert_eq!(out.stdout, format!("{key} = {shown}\n"));
        let (section, name) = key.split_once('.').unwrap();
        assert_eq!(file_json(&env)[section][name], stored, "{key} {raw}");
        assert_eq!(get_json(&env, key), stored, "{key} {raw}");
    }
}

#[test]
fn set_preserves_unknown_keys_and_key_order() {
    let env = TestEnv::new();
    seed_settings(
        &env,
        r#"{"futureSection": {"x": 1}, "autoswitch": {"futureKnob": true, "threshold": 80}, "ui": {"theme": "dark"}}"#,
    );
    env.run(["config", "set", "autoswitch.threshold", "70"])
        .assert_code(0);
    env.run(["config", "set", "autoswitch.model", "\u{e9}"])
        .assert_code(0);
    assert_eq!(
        file_text(&env),
        "{\n  \"futureSection\": {\n    \"x\": 1\n  },\n  \"autoswitch\": {\n    \"futureKnob\": true,\n    \"threshold\": 70.0,\n    \"model\": \"\\u00e9\"\n  },\n  \"ui\": {\n    \"theme\": \"dark\"\n  },\n  \"schemaVersion\": 1\n}"
    );
}

#[test]
fn invalid_values_exit_1_without_writing() {
    let env = TestEnv::new();
    let p = prog();
    let threshold_range = "autoswitch.threshold must be between 50 and 99.9".to_string();
    let cases = [
        ("autoswitch.threshold", "30", threshold_range.clone()),
        ("autoswitch.threshold", "200", threshold_range.clone()),
        ("autoswitch.threshold", "nan", threshold_range.clone()),
        ("autoswitch.threshold", "-5", threshold_range),
        (
            "autoswitch.intervalSeconds",
            "1",
            "autoswitch.intervalSeconds must be between 15 and 3600".into(),
        ),
        (
            "autoswitch.unhealthyTicks",
            "0",
            "autoswitch.unhealthyTicks must be between 1 and 100".into(),
        ),
        (
            "autoswitch.threshold",
            "high",
            "autoswitch.threshold expects a number, got 'high'".into(),
        ),
        (
            "autoswitch.threshold",
            "1__0",
            "autoswitch.threshold expects a number, got '1__0'".into(),
        ),
        (
            "autoswitch.unhealthyTicks",
            "3.5",
            "autoswitch.unhealthyTicks expects an integer, got '3.5'".into(),
        ),
        (
            "autoswitch.includeApiKeyAccounts",
            "falsy",
            "autoswitch.includeApiKeyAccounts expects true or false (or 1/0, yes/no), got 'falsy'"
                .into(),
        ),
        (
            "autoswitch.strategy",
            "chaos",
            "autoswitch.strategy must be one of: best, consume-first".into(),
        ),
        (
            "autoswitch.strategy",
            "Best",
            "autoswitch.strategy must be one of: best, consume-first".into(),
        ),
        (
            "ui.theme",
            "purple",
            "ui.theme must be one of: dark, light, auto".into(),
        ),
        (
            "autoswitch.model",
            "   ",
            format!(
                "autoswitch.model expects a non-empty value; use '{p} config unset autoswitch.model' to clear it"
            ),
        ),
        (
            "autoswitch.bogus",
            "1",
            format!(
                "unknown setting 'autoswitch.bogus'\nValid keys: {}",
                KEYS.join(", ")
            ),
        ),
    ];
    for (key, raw, message) in cases {
        let out = env.run(["config", "set", key, raw]);
        out.assert_code(1);
        assert_eq!(out.stdout, "");
        assert_eq!(out.stderr, format!("Error: {message}\n"), "{key} {raw}");
    }
    assert!(!env.store_dir().exists());
}

#[test]
fn unknown_key_with_json_prints_error_envelope() {
    let env = TestEnv::new();
    let out = env.run(["config", "--json", "get", "autoswitch.bogus"]);
    out.assert_code(1);
    assert_eq!(
        out.json(),
        json!({
            "schemaVersion": 1,
            "error": {
                "type": "ConfigError",
                "message": format!("unknown setting 'autoswitch.bogus'\nValid keys: {}", KEYS.join(", ")),
            }
        })
    );
    assert_eq!(out.stderr, "");
}

#[test]
fn set_and_unset_refuse_a_corrupt_file_and_leave_it_untouched() {
    let env = TestEnv::new();
    let path = settings_path(&env);
    let p = path.display();
    let cases: [(&[u8], String, String); 3] = [
        (
            b"{not json",
            format!("Could not read {p} ("),
            format!("Error: {p} is not valid JSON ("),
        ),
        (
            b"[1, 2]",
            format!("{p} is not a JSON object; using defaults"),
            format!("Error: {p} is not a JSON object; fix or delete it before changing settings"),
        ),
        (
            b"\xff{}",
            format!(
                "Could not read {p} ('utf-8' codec can't decode byte 0xff in position 0: invalid start byte); using defaults"
            ),
            format!(
                "Error: could not read {p}: 'utf-8' codec can't decode byte 0xff in position 0: invalid start byte"
            ),
        ),
    ];
    for (bytes, warning, error) in cases {
        std::fs::create_dir_all(env.store_dir()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        for args in [
            vec!["config", "set", "autoswitch.threshold", "80"],
            vec!["config", "unset", "autoswitch.threshold"],
        ] {
            let out = env.run(&args);
            out.assert_code(1);
            let lines: Vec<&str> = out.stderr.lines().collect();
            assert_eq!(lines.len(), 2, "{}", out.stderr);
            assert!(lines[0].starts_with(&warning), "{}", out.stderr);
            assert!(lines[1].starts_with(&error), "{}", out.stderr);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }
}

#[test]
fn every_command_warns_on_stderr_about_an_unusable_ui_theme() {
    let env = TestEnv::new();
    let cases = [
        (r#"{"ui": {"theme": "purple"}}"#, "'purple'"),
        (r#"{"ui": {"theme": [1, {"a": null}]}}"#, "[1, {'a': None}]"),
        (r#"{"ui": {"theme": "xé\"'"}}"#, r#"'xé"\''"#),
        (r#"{"ui": {"theme": 1.0}}"#, "1.0"),
        (r#"{"ui": {"theme": true}}"#, "True"),
    ];
    for (text, repr) in cases {
        seed_settings(&env, text);
        let out = env.run(["--version"]);
        out.assert_code(0);
        assert_eq!(
            out.stderr,
            format!("settings.json: unsupported ui.theme {repr}; using 'auto'\n")
        );
    }
    seed_settings(
        &env,
        r#"{"ui": {"theme": "light"}, "autoswitch": {"strategy": "x"}}"#,
    );
    assert_eq!(env.run(["--version"]).stderr, "");
}

#[test]
fn unset_restores_default_and_drops_the_empty_section() {
    let env = TestEnv::new();
    env.run(["config", "set", "autoswitch.threshold", "80"])
        .assert_code(0);
    env.run(["config", "set", "ui.theme", "light"])
        .assert_code(0);
    let out = env.run(["config", "unset", "autoswitch.threshold"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "autoswitch.threshold unset (default: 90)\n");
    assert_eq!(
        env.run(["config", "get", "autoswitch.threshold"]).stdout,
        "90\n"
    );
    assert_eq!(
        file_json(&env),
        json!({"schemaVersion": 1, "ui": {"theme": "light"}})
    );
    let out = env.run(["config", "unset", "autoswitch.model"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr, "autoswitch.model is not set; nothing to do\n");
    let out = env.run(["config", "unset", "ui.theme"]);
    assert_eq!(out.stdout, "ui.theme unset (default: auto)\n");
    assert_eq!(file_text(&env), "{\n  \"schemaVersion\": 1\n}");
}

#[test]
fn unset_stamps_schema_version_and_absent_key_does_not_write() {
    let env = TestEnv::new();
    env.run(["config", "unset", "autoswitch.threshold"])
        .assert_code(0);
    assert!(!env.store_dir().exists());
    seed_settings(&env, r#"{"autoswitch": {"threshold": 80, "x": 1}}"#);
    env.run(["config", "unset", "autoswitch.threshold"])
        .assert_code(0);
    assert_eq!(
        file_text(&env),
        "{\n  \"autoswitch\": {\n    \"x\": 1\n  },\n  \"schemaVersion\": 1\n}"
    );
}

#[test]
fn reading_a_missing_corrupt_or_non_object_file_gives_defaults() {
    let env = TestEnv::new();
    for text in [
        None,
        Some("{not json"),
        Some("[1, 2]"),
        Some(r#"{"autoswitch": 5, "ui": []}"#),
    ] {
        if let Some(text) = text {
            seed_settings(&env, text);
        }
        let out = env.run(["config", "--json"]);
        out.assert_code(0);
        let entries = out.json()["settings"].as_array().unwrap().clone();
        assert_eq!(
            entries[0],
            json!({"key": "autoswitch.threshold", "value": 90.0, "isSet": false})
        );
        assert!(entries.iter().all(|e| e["isSet"] == false), "{text:?}");
    }
    std::fs::write(settings_path(&env), [0xff, b'{', b'}']).unwrap();
    assert_eq!(get_json(&env, "autoswitch.threshold"), json!(90.0));
}

#[test]
fn reading_clamps_out_of_range_values_and_defaults_bad_types() {
    let env = TestEnv::new();
    seed_settings(
        &env,
        r#"{"autoswitch": {
            "threshold": 200, "intervalSeconds": 1, "cooldownSeconds": "high",
            "hysteresisPct": -5, "unhealthyTicks": 7.9, "strategy": "chaos",
            "includeApiKeyAccounts": 1, "model": 123
        }, "ui": {"theme": "purple"}}"#,
    );
    let out = env.run(["config", "--json"]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        "settings.json: unsupported ui.theme 'purple'; using 'auto'\n"
    );
    let expected = [
        json!(99.9),
        json!(15.0),
        json!(300.0),
        json!(0.0),
        json!("best"),
        json!(true),
        json!(7),
        Value::Null,
        json!("auto"),
    ];
    let entries = out.json()["settings"].as_array().unwrap().clone();
    for (entry, want) in entries.iter().zip(expected) {
        assert_eq!(entry["value"], want, "{}", entry["key"]);
        assert_eq!(entry["isSet"], true, "{}", entry["key"]);
    }
    assert!(out.stdout.contains("\"value\": 7,"));
    seed_settings(
        &env,
        r#"{"autoswitch": {"unhealthyTicks": 0, "includeApiKeyAccounts": "", "model": "", "strategy": "consume-first"}, "ui": {"theme": "light"}}"#,
    );
    assert_eq!(get_json(&env, "autoswitch.unhealthyTicks"), json!(1));
    assert_eq!(
        get_json(&env, "autoswitch.includeApiKeyAccounts"),
        json!(false)
    );
    assert_eq!(get_json(&env, "autoswitch.model"), Value::Null);
    assert_eq!(
        get_json(&env, "autoswitch.strategy"),
        json!("consume-first")
    );
    assert_eq!(get_json(&env, "ui.theme"), json!("light"));
    assert_eq!(
        env.run(["config", "get", "autoswitch.model"]).stdout,
        "(none)\n"
    );
}

#[test]
fn set_writes_through_a_symlink_and_hardens_only_the_store_dir() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let repo = env.root().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tracked = repo.join("settings.json");
    std::fs::write(&tracked, r#"{"tracked": true}"#).unwrap();
    std::fs::create_dir_all(env.store_dir()).unwrap();
    std::os::unix::fs::symlink(&tracked, settings_path(&env)).unwrap();

    env.run(["config", "set", "autoswitch.threshold", "80"])
        .assert_code(0);

    assert!(is_symlink(&settings_path(&env)));
    assert_eq!(
        env.read_json(&tracked),
        json!({"tracked": true, "schemaVersion": 1, "autoswitch": {"threshold": 80.0}})
    );
    assert_eq!(mode(&repo), 0o755);
    assert_eq!(mode(&env.store_dir()), 0o700);
    assert_eq!(mode(&tracked), 0o600);
    assert_eq!(std::fs::read_dir(&repo).unwrap().count(), 1);
}

#[test]
fn set_through_a_dangling_symlink_writes_where_it_points() {
    let env = TestEnv::new();
    let target = env.root().join("gone").join("settings.json");
    std::fs::create_dir_all(env.store_dir()).unwrap();
    std::os::unix::fs::symlink(&target, settings_path(&env)).unwrap();

    env.run(["config", "set", "ui.theme", "dark"])
        .assert_code(0);

    assert!(is_symlink(&settings_path(&env)));
    assert_eq!(
        env.read_json(&target),
        json!({"schemaVersion": 1, "ui": {"theme": "dark"}})
    );
}

#[test]
fn argparse_errors_exit_2() {
    let env = TestEnv::new();
    let p = prog();
    let top = format!(
        "usage: {p} config [-h] [--json] [--debug] {{list,get,set,unset,path}} ...\n{p} config: error: "
    );
    let set_usage = format!("usage: {p} config set [-h] KEY VALUE\n{p} config set: error: ");
    let cases: Vec<(Vec<&str>, String)> = vec![
        (
            vec!["set", "autoswitch.threshold"],
            format!("{set_usage}the following arguments are required: VALUE\n"),
        ),
        (
            vec!["set", "--bogus"],
            format!("{set_usage}the following arguments are required: KEY, VALUE\n"),
        ),
        (
            vec!["get"],
            format!(
                "usage: {p} config get [-h] [--json] KEY\n{p} config get: error: the following arguments are required: KEY\n"
            ),
        ),
        (
            vec!["frobnicate"],
            format!(
                "{top}argument {{list,get,set,unset,path}}: invalid choice: 'frobnicate' (choose from 'list', 'get', 'set', 'unset', 'path')\n"
            ),
        ),
        (
            vec!["--json", "set", "autoswitch.threshold", "80"],
            format!("{top}--json can only be used with list or get\n"),
        ),
        (
            vec!["--json", "path"],
            format!("{top}--json can only be used with list or get\n"),
        ),
        (
            vec!["set", "--json", "a", "b"],
            format!("{top}unrecognized arguments: --json\n"),
        ),
        (
            vec!["path", "--debug"],
            format!("{top}unrecognized arguments: --debug\n"),
        ),
        (
            vec!["--bogus", "list"],
            format!("{top}unrecognized arguments: --bogus\n"),
        ),
        (
            vec!["get", "a", "b"],
            format!("{top}unrecognized arguments: b\n"),
        ),
        (
            vec!["list", "--", "x"],
            format!("{top}unrecognized arguments: -- x\n"),
        ),
        (
            vec!["--json=1"],
            format!("{top}argument --json: ignored explicit argument '1'\n"),
        ),
    ];
    for (args, stderr) in cases {
        let out = env.run(std::iter::once("config").chain(args.iter().copied()));
        out.assert_code(2);
        assert_eq!(out.stdout, "", "{args:?}");
        assert_eq!(out.stderr, stderr, "{args:?}");
    }
}

#[test]
fn option_prefixes_and_debug_are_accepted() {
    let env = TestEnv::new();
    let out = env.run(["config", "--de", "path"]);
    out.assert_code(0);
    assert_eq!(out.stdout, format!("{}\n", settings_path(&env).display()));
    let out = env.run(["config", "get", "--j", "ui.theme"]);
    out.assert_code(0);
    assert_eq!(out.json()["value"], "auto");
}

#[test]
fn help_lists_keys_and_verbs() {
    let env = TestEnv::new();
    let p = prog();
    let out = env.run(["config", "--help"]);
    out.assert_code(0);
    assert!(out.stdout.starts_with(&format!(
        "usage: {p} config [-h] [--json] [--debug] {{list,get,set,unset,path}} ...\n\nRead and edit"
    )));
    assert!(out.stdout.contains(
        "\n  autoswitch.model                  Also switch on these models' weekly limits (e.g. Fable, Fable,Opus, or all) (default (none))\n"
    ));
    assert!(
        out.stdout
            .contains("    unset               Remove one setting (revert to the default)\n")
    );
    assert_eq!(env.run(["config", "-h", "frobnicate"]).stdout, out.stdout);
    let out = env.run(["config", "get", "-h"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!(
            "usage: {p} config get [-h] [--json] KEY\n\npositional arguments:\n  KEY         Dotted key, e.g. autoswitch.threshold\n\noptions:\n  -h, --help  show this help message and exit\n  --json      Emit machine-readable JSON to stdout\n"
        )
    );
    assert_eq!(
        env.run(["config", "set", "--help"]).stdout,
        format!(
            "usage: {p} config set [-h] KEY VALUE\n\npositional arguments:\n  KEY\n  VALUE\n\noptions:\n  -h, --help  show this help message and exit\n"
        )
    );
    assert_eq!(
        env.run(["config", "path", "-h"]).stdout,
        format!(
            "usage: {p} config path [-h]\n\noptions:\n  -h, --help  show this help message and exit\n"
        )
    );
}

#[test]
fn main_help_mentions_config() {
    let env = TestEnv::new();
    let out = env.run(["--help"]);
    out.assert_code(0);
    assert!(out.stdout.contains(" config"));
}
