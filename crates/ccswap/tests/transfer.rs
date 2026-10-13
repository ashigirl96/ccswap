//! Ported from upstream tests/test_transfer.py (TestRoundTrip,
//! TestAliasTransfer, TestSelectiveExport, TestConflictPolicy, TestValidation,
//! TestPipeMode, TestEmptyHome, TestFilePermissions,
//! TestValidateAllBeforeWrite, TestCleanHomeActivation (seeding half),
//! TestSlimVsFullConfig, TestSlimVsFullCredentials, TestExportSkipsBrokenSlots,
//! TestImportClearsDeadTokenQuarantine, TestForceOverwriteNarratesTheStrikeClear,
//! TestCrossPlatform (import of a "linux" export)), plus cross-implementation
//! round trips with upstream `cswap`.

mod support;

use support::hint;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use ccswap_core::pyjson;
use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, Impl, KEYCHAIN_ACCOUNT, TestEnv, is_upstream};

const BACKUP_SERVICE: &str = "claude-swap";
const RT_BOB_FP: &str = "sha256:b9de5f24e89f54d584cf14a42783ad8a00b06e0071fa78ee8db84d8d416085ac";
const RT_LIVE_FP: &str = "sha256:80f301d7c7438817bae102e3e139ad6dc7327abff94078f0d84b90bb4b343e8f";
const LEGACY_NO_REFRESH_FP: &str =
    "sha256-full:f4a039686a91d92d8932ebd7a362a131eee6c8fa3daa13c8ee018476145fd9a3";

fn local_part(email: &str) -> &str {
    email.split('@').next().unwrap()
}

/// The account's own login, carrying a marker so overwrites are visible.
fn creds_for(email: &str) -> Value {
    json!({"claudeAiOauth": {
        "accessToken": format!("at-{}", local_part(email)),
        "refreshToken": format!("rt-{}", local_part(email)),
        "expiresAt": 9_999_999_999_000_i64,
        "marker": email,
    }})
}

fn config_for(num: u32, email: &str, org: &str, org_name: &str) -> Value {
    json!({"oauthAccount": {
        "emailAddress": email,
        "accountUuid": format!("acct-{num}"),
        "organizationUuid": org,
        "organizationName": org_name,
    }})
}

struct Seed<'a> {
    num: u32,
    email: &'a str,
    org: &'a str,
    org_name: &'a str,
    creds: Option<Value>,
    config: Option<Value>,
    alias: Option<&'a str>,
}

impl<'a> Seed<'a> {
    fn new(num: u32, email: &'a str) -> Self {
        Seed {
            num,
            email,
            org: "",
            org_name: "",
            creds: None,
            config: None,
            alias: None,
        }
    }

    fn org(mut self, org: &'a str, name: &'a str) -> Self {
        self.org = org;
        self.org_name = name;
        self
    }

    fn creds(mut self, creds: Value) -> Self {
        self.creds = Some(creds);
        self
    }

    fn config(mut self, config: Value) -> Self {
        self.config = Some(config);
        self
    }

    fn alias(mut self, alias: &'a str) -> Self {
        self.alias = Some(alias);
        self
    }
}

/// Mirrors upstream `_seed_account`.
fn seed(env: &TestEnv, s: Seed) {
    let creds = s.creds.clone().unwrap_or_else(|| creds_for(s.email));
    let config = s
        .config
        .clone()
        .unwrap_or_else(|| config_for(s.num, s.email, s.org, s.org_name));
    seed_backup_creds(env, s.num, s.email, &creds);
    seed_backup_config(env, s.num, s.email, &config);
    seed_record(env, &s);
}

fn seed_backup_creds(env: &TestEnv, num: u32, email: &str, creds: &Value) {
    env.security().set_item(
        BACKUP_SERVICE,
        &format!("account-{num}-{email}"),
        &pyjson::to_string_compact(creds),
    );
}

fn seed_backup_config(env: &TestEnv, num: u32, email: &str, config: &Value) {
    env.write_file(
        env.store_dir()
            .join(format!("configs/.claude-config-{num}-{email}.json")),
        &pyjson::to_string_compact(config),
    );
}

fn seed_record(env: &TestEnv, s: &Seed) {
    let path = env.store_dir().join("sequence.json");
    let mut data = if path.exists() {
        env.read_json(&path)
    } else {
        json!({"activeAccountNumber": null, "lastUpdated": "", "sequence": [], "accounts": {}})
    };
    let mut record = json!({
        "email": s.email,
        "uuid": format!("acct-{}", s.num),
        "organizationUuid": s.org,
        "organizationName": s.org_name,
        "added": "2024-01-01T00:00:00Z",
    });
    if let Some(alias) = s.alias {
        record["alias"] = alias.into();
    }
    data["accounts"][s.num.to_string()] = record;
    let seq = data["sequence"].as_array_mut().unwrap();
    if !seq.contains(&json!(s.num)) {
        seq.push(json!(s.num));
        seq.sort_by_key(|v| v.as_u64());
    }
    if data["activeAccountNumber"].is_null() {
        data["activeAccountNumber"] = s.num.into();
    }
    env.seed_sequence(data);
}

fn set_active(env: &TestEnv, active: Value) {
    let mut data = env.sequence();
    data["activeAccountNumber"] = active;
    env.seed_sequence(data);
}

fn stored_creds_text(env: &TestEnv, num: &str, email: &str) -> Option<String> {
    env.security()
        .item(BACKUP_SERVICE, &format!("account-{num}-{email}"))
}

fn stored_creds(env: &TestEnv, num: &str, email: &str) -> Value {
    serde_json::from_str(&stored_creds_text(env, num, email).expect("backup credential")).unwrap()
}

fn stored_config_text(env: &TestEnv, num: &str, email: &str) -> String {
    std::fs::read_to_string(
        env.store_dir()
            .join(format!("configs/.claude-config-{num}-{email}.json")),
    )
    .unwrap()
}

fn export_to(env: &TestEnv, file: &Path, extra: &[&str]) -> Value {
    let mut args = vec!["export", file.to_str().unwrap()];
    args.extend_from_slice(extra);
    env.run(&args).assert_code(0);
    serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap()
}

fn backup_file(env: &TestEnv, name: &str) -> PathBuf {
    env.root().join(name)
}

fn write_envelope(path: &Path, envelope: &Value) {
    std::fs::write(path, envelope.to_string()).unwrap();
}

fn import(env: &TestEnv, file: &Path, extra: &[&str]) -> support::Output {
    let mut args = vec!["import", file.to_str().unwrap()];
    args.extend_from_slice(extra);
    env.run(&args)
}

fn emails(envelope: &Value) -> Vec<&str> {
    envelope["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["email"].as_str().unwrap())
        .collect()
}

fn slot_of<'a>(seq: &'a Value, email: &str) -> &'a str {
    seq["accounts"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, a)| a["email"] == email)
        .map(|(n, _)| n.as_str())
        .unwrap_or_else(|| panic!("{email} not in roster: {seq}"))
}

fn strike(env: &TestEnv, rows: Value) {
    env.write_json(
        env.store_dir().join("cache/usage.json"),
        &json!({"schemaVersion": 2, "accounts": rows}),
    );
}

fn usage_row(env: &TestEnv, num: &str) -> Value {
    env.read_json(env.store_dir().join("cache/usage.json"))["accounts"][num].clone()
}

fn dead_row(email: &str, org: &str, fp: Option<&str>) -> Value {
    json!({"email": email, "organizationUuid": org, "authDeadStrikes": 1,
           "struckFingerprint": fp, "lastError": "invalid_grant", "consecutiveFailures": 1})
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn sample_envelope(email: Value, number: Value) -> Value {
    json!({
        "version": 1,
        "exportedAt": "2026-01-01T00:00:00Z",
        "exportedFrom": "linux",
        "swapVersion": "0.0.0",
        "encrypted": false,
        "activeAccountNumber": if number.is_i64() { number.clone() } else { Value::Null },
        "accounts": [{
            "number": number,
            "email": email,
            "uuid": "u",
            "organizationUuid": "",
            "organizationName": "",
            "added": "2024-01-01T00:00:00Z",
            "credentials": {"accessToken": "tok-1", "refreshToken": "rtok-1", "expiresAt": 9999},
            "config": {"oauthAccount": {"emailAddress": "user@example.com", "accountUuid": "acct-uuid",
                       "organizationUuid": "org-uuid", "organizationName": "Acme"}},
        }],
    })
}

#[test]
fn export_import_round_trip() {
    let src = TestEnv::new();
    seed(
        &src,
        Seed::new(1, "alice@example.com").org("org-a", "Org A"),
    );
    seed(&src, Seed::new(2, "bob@example.com"));
    let file = backup_file(&src, "backup.cswap");

    let out = src.run(["export", file.to_str().unwrap()]);
    out.assert_code(0);
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        format!("Exported 2 account(s) to {}\n", file.display())
    );
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.ends_with("}\n"));
    let envelope: Value = serde_json::from_str(&text).unwrap();
    let keys: Vec<&str> = envelope
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "version",
            "exportedAt",
            "exportedFrom",
            "swapVersion",
            "encrypted",
            "activeAccountNumber",
            "accounts"
        ]
    );
    assert_eq!(envelope["version"], 1);
    assert_eq!(envelope["exportedFrom"], "macos");
    assert_eq!(envelope["encrypted"], false);
    assert_eq!(envelope["activeAccountNumber"], 1);
    if !is_upstream() {
        assert_eq!(envelope["swapVersion"], ccswap_core::VERSION);
    }
    assert_eq!(
        envelope["accounts"][0],
        json!({
            "number": 1,
            "email": "alice@example.com",
            "uuid": "acct-1",
            "organizationUuid": "org-a",
            "organizationName": "Org A",
            "added": "2024-01-01T00:00:00Z",
            "credentials": creds_for("alice@example.com"),
            "config": config_for(1, "alice@example.com", "org-a", "Org A"),
        })
    );
    let entry_keys: Vec<&str> = envelope["accounts"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        entry_keys,
        [
            "number",
            "email",
            "uuid",
            "organizationUuid",
            "organizationName",
            "added",
            "credentials",
            "config"
        ]
    );

    let dst = TestEnv::new();
    let out = import(&dst, &file, &[]);
    out.assert_code(0);
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "Imported alice@example.com → slot 1\nImported bob@example.com → slot 2\nDone: 2 imported, 0 overwritten, 0 skipped\n"
    );
    let seq = dst.sequence();
    assert_eq!(seq["activeAccountNumber"], 1);
    assert_eq!(seq["sequence"], json!([1, 2]));
    assert_eq!(
        seq["accounts"]["1"],
        json!({"email": "alice@example.com", "uuid": "acct-1", "organizationUuid": "org-a",
               "organizationName": "Org A", "added": "2024-01-01T00:00:00Z"})
    );
    assert_eq!(
        stored_creds_text(&dst, "1", "alice@example.com").unwrap(),
        pyjson::to_string_compact(&creds_for("alice@example.com")),
        "stored as json.dumps of the imported object"
    );
    assert_eq!(
        stored_config_text(&dst, "1", "alice@example.com"),
        pyjson::to_string_pretty(&config_for(1, "alice@example.com", "org-a", "Org A"))
    );
    assert_eq!(mode(&dst.store_dir().join("sequence.json")), 0o600);
}

#[test]
fn active_state_carried_but_not_applied() {
    let src = TestEnv::new();
    seed(&src, Seed::new(1, "a@example.com"));
    seed(&src, Seed::new(2, "b@example.com"));
    set_active(&src, json!(2));
    let file = backup_file(&src, "backup.cswap");
    assert_eq!(export_to(&src, &file, &[])["activeAccountNumber"], 2);

    let dst = TestEnv::new();
    seed(&dst, Seed::new(9, "local@example.com"));
    import(&dst, &file, &[]).assert_code(0);
    assert_eq!(dst.sequence()["activeAccountNumber"], 9);
}

#[test]
fn import_never_touches_the_live_login() {
    let src = TestEnv::new();
    seed(&src, Seed::new(1, "a@example.com"));
    let file = backup_file(&src, "backup.cswap");
    export_to(&src, &file, &[]);

    let dst = TestEnv::new();
    import(&dst, &file, &[]).assert_code(0);
    assert!(!dst.global_config_path().exists());
    assert_eq!(
        dst.security().item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT),
        None
    );
}

#[test]
fn alias_round_trips() {
    let src = TestEnv::new();
    seed(&src, Seed::new(1, "alice@example.com").alias("dev"));
    seed(&src, Seed::new(2, "bob@example.com"));
    let file = backup_file(&src, "backup.cswap");
    let envelope = export_to(&src, &file, &[]);
    assert_eq!(envelope["accounts"][0]["alias"], "dev");
    assert!(envelope["accounts"][1].get("alias").is_none());

    let dst = TestEnv::new();
    import(&dst, &file, &[]).assert_code(0);
    let seq = dst.sequence();
    assert_eq!(seq["accounts"]["1"]["alias"], "dev");
    assert!(seq["accounts"]["2"].get("alias").is_none());
}

#[test]
fn import_alias_collision_with_local_drops_and_warns() {
    let src = TestEnv::new();
    seed(&src, Seed::new(1, "alice@example.com").alias("dev"));
    let file = backup_file(&src, "backup.cswap");
    export_to(&src, &file, &[]);

    let dst = TestEnv::new();
    seed(&dst, Seed::new(9, "existing@example.com").alias("dev"));
    let out = import(&dst, &file, &[]);
    out.assert_code(0);
    assert!(out.stderr.starts_with(
        "Warning: alias 'dev' for alice@example.com already used by an existing account, dropping the imported alias\n"
    ), "{}", out.stderr);
    let seq = dst.sequence();
    assert_eq!(seq["accounts"]["9"]["alias"], "dev");
    assert!(seq["accounts"]["1"].get("alias").is_none());
}

#[test]
fn import_reexport_of_same_account_keeps_own_alias() {
    let src = TestEnv::new();
    seed(&src, Seed::new(1, "alice@example.com").alias("dev"));
    let file = backup_file(&src, "backup.cswap");
    export_to(&src, &file, &[]);

    let dst = TestEnv::new();
    seed(&dst, Seed::new(1, "alice@example.com").alias("dev"));
    let out = import(&dst, &file, &["--force"]);
    out.assert_code(0);
    assert!(!out.stderr.contains("Warning"), "{}", out.stderr);
    assert_eq!(dst.sequence()["accounts"]["1"]["alias"], "dev");
}

#[test]
fn import_duplicate_alias_within_export_rejected() {
    let src = TestEnv::new();
    seed(&src, Seed::new(1, "alice@example.com").alias("dev"));
    seed(&src, Seed::new(2, "bob@example.com").alias("ops"));
    let file = backup_file(&src, "backup.cswap");
    let mut envelope = export_to(&src, &file, &[]);
    envelope["accounts"][1]["alias"] = "DEV".into();
    write_envelope(&file, &envelope);

    let dst = TestEnv::new();
    let out = import(&dst, &file, &[]);
    out.assert_code(1);
    assert_eq!(out.stderr, "Error: duplicate alias in export: dev\n");
}

#[test]
fn import_invalid_alias_format_rejected() {
    let src = TestEnv::new();
    seed(&src, Seed::new(1, "alice@example.com"));
    let file = backup_file(&src, "backup.cswap");
    let mut envelope = export_to(&src, &file, &[]);
    envelope["accounts"][0]["alias"] = "123".into();
    write_envelope(&file, &envelope);

    let dst = TestEnv::new();
    let out = import(&dst, &file, &[]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: invalid alias for alice@example.com: alias '123' cannot be purely numeric (reserved for slot numbers)\n"
    );
}

#[test]
fn export_single_by_number_or_email() {
    let env = TestEnv::new();
    seed(&env, Seed::new(1, "a@example.com"));
    seed(&env, Seed::new(2, "b@example.com"));
    let file = backup_file(&env, "one.cswap");

    let envelope = export_to(&env, &file, &["--account", "2"]);
    assert_eq!(emails(&envelope), ["b@example.com"]);
    assert_eq!(envelope["activeAccountNumber"], Value::Null);

    let envelope = export_to(&env, &file, &["--account", "a@example.com"]);
    assert_eq!(emails(&envelope), ["a@example.com"]);
    assert_eq!(envelope["activeAccountNumber"], 1);
}

#[test]
fn export_by_alias() {
    let env = TestEnv::new();
    seed(&env, Seed::new(1, "a@example.com"));
    seed(&env, Seed::new(2, "b@example.com").alias("work"));
    let file = backup_file(&env, "one.cswap");
    assert_eq!(
        emails(&export_to(&env, &file, &["--account", "WORK"])),
        ["b@example.com"]
    );
}

#[test]
fn export_unknown_account_or_empty_roster_fails() {
    let env = TestEnv::new();
    let file = backup_file(&env, "x.cswap");
    let out = env.run(["export", file.to_str().unwrap()]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        hint("Error: no accounts to export — run cswap --add-account first\n")
    );

    seed(&env, Seed::new(1, "a@example.com"));
    let out = env.run(["export", file.to_str().unwrap(), "--account", "999"]);
    out.assert_code(1);
    assert_eq!(out.stderr, "Error: account not found: 999\n");
    assert!(!file.exists());
}

#[test]
fn skip_when_account_exists_without_force() {
    let env = TestEnv::new();
    seed(&env, Seed::new(1, "alice@example.com").org("org-a", ""));
    let file = backup_file(&env, "b.cswap");
    export_to(&env, &file, &[]);

    let out = import(&env, &file, &[]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        "Skipped alice@example.com (already exists, use --force)\nDone: 0 imported, 0 overwritten, 1 skipped\n"
    );
    let seq = env.sequence();
    assert_eq!(
        seq["accounts"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["1"]
    );
}

#[test]
fn force_overwrites_existing_slot_in_place() {
    let env = TestEnv::new();
    seed(
        &env,
        Seed::new(3, "alice@example.com").org("org-a", "Org A"),
    );
    let file = backup_file(&env, "alice.cswap");
    let mut envelope = export_to(&env, &file, &["--account", "3"]);
    envelope["accounts"][0]["number"] = 1.into();
    envelope["accounts"][0]["credentials"]["claudeAiOauth"]["marker"] = "ALICE_NEW".into();
    write_envelope(&file, &envelope);
    seed(&env, Seed::new(1, "bob@example.com"));
    let bob_before = stored_creds_text(&env, "1", "bob@example.com");

    let out = import(&env, &file, &["--force"]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        "Overwrote alice@example.com (slot 3)\nDone: 0 imported, 1 overwritten, 0 skipped\n"
    );
    assert_eq!(
        stored_creds(&env, "3", "alice@example.com")["claudeAiOauth"]["marker"],
        "ALICE_NEW"
    );
    assert_eq!(stored_creds_text(&env, "1", "bob@example.com"), bob_before);
    let seq = env.sequence();
    assert_eq!(seq["accounts"]["1"]["email"], "bob@example.com");
    assert_eq!(seq["accounts"]["3"]["email"], "alice@example.com");
}

#[test]
fn import_over_live_login_hints_force_activation() {
    let env = TestEnv::new();
    seed(
        &env,
        Seed::new(1, "alice@example.com").org("org-a", "Org A"),
    );
    let file = backup_file(&env, "alice.cswap");
    export_to(&env, &file, &[]);
    env.seed_login("alice@example.com", "org-a", "Org A");
    env.security().set_item(
        ACTIVE_OAUTH_SERVICE,
        KEYCHAIN_ACCOUNT,
        r#"{"claudeAiOauth": {"accessToken": "sk-stale-live"}}"#,
    );

    let out = import(&env, &file, &["--force"]);
    out.assert_code(0);
    assert!(
        out.stderr.ends_with(
            &hint("Note: alice@example.com is your current live login — activate the imported credentials with: cswap --switch-to 1 --force\n")
        ),
        "{}",
        out.stderr
    );
    assert_eq!(
        env.security()
            .item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT)
            .unwrap(),
        r#"{"claudeAiOauth": {"accessToken": "sk-stale-live"}}"#,
        "import never activates"
    );
}

#[test]
fn import_without_matching_live_login_prints_no_hint() {
    let env = TestEnv::new();
    seed(
        &env,
        Seed::new(1, "alice@example.com").org("org-a", "Org A"),
    );
    env.seed_login("bob@example.com", "", "");
    let file = backup_file(&env, "alice.cswap");
    export_to(&env, &file, &[]);

    let out = import(&env, &file, &["--force"]);
    out.assert_code(0);
    assert!(!out.stderr.contains("current live login"), "{}", out.stderr);
}

#[test]
fn slot_allocation_when_exported_slot_taken() {
    let src = TestEnv::new();
    seed(&src, Seed::new(1, "alice@example.com"));
    let file = backup_file(&src, "a.cswap");
    export_to(&src, &file, &[]);

    let dst = TestEnv::new();
    seed(&dst, Seed::new(1, "bob@example.com"));
    seed(&dst, Seed::new(4, "carol@example.com"));
    let out = import(&dst, &file, &[]);
    out.assert_code(0);
    assert!(
        out.stderr
            .starts_with("Imported alice@example.com → slot 5\n")
    );
    let seq = dst.sequence();
    assert_eq!(seq["accounts"]["1"]["email"], "bob@example.com");
    assert_eq!(seq["accounts"]["5"]["email"], "alice@example.com");
    assert_eq!(seq["sequence"], json!([1, 4, 5]));
}

fn assert_import_rejected(envelope: &Value, expected: &str) {
    let env = TestEnv::new();
    let file = backup_file(&env, "bad.cswap");
    write_envelope(&file, envelope);
    let out = import(&env, &file, &[]);
    out.assert_code(1);
    assert_eq!(out.stderr, format!("Error: {expected}\n"));
    assert!(
        !env.store_dir().join("sequence.json").exists(),
        "nothing may be written for a rejected file"
    );
    assert!(env.security().items().is_empty());
}

#[test]
fn path_traversal_email_rejected() {
    assert_import_rejected(
        &sample_envelope("../../evil".into(), 1.into()),
        "invalid or missing email in imported account: '../../evil'",
    );
    assert_import_rejected(
        &sample_envelope(Value::Null, 1.into()),
        "invalid or missing email in imported account: None",
    );
}

#[test]
fn invalid_slot_numbers_rejected() {
    for (number, repr) in [
        (json!(-1), "-1"),
        (json!(0), "0"),
        (json!("../"), "'../'"),
        (json!(true), "True"),
        (json!(2.0), "2.0"),
        (Value::Null, "None"),
    ] {
        assert_import_rejected(
            &sample_envelope("user@example.com".into(), number),
            &format!("invalid slot number in imported account (user@example.com): {repr}"),
        );
    }
}

#[test]
fn envelope_checks_rejected() {
    let mut env = sample_envelope("user@example.com".into(), 1.into());
    env.as_object_mut().unwrap().shift_remove("version");
    assert_import_rejected(&env, "unsupported export version: None (expected 1)");

    let mut env = sample_envelope("user@example.com".into(), 1.into());
    env["version"] = 2.into();
    assert_import_rejected(&env, "unsupported export version: 2 (expected 1)");
    env["version"] = "1".into();
    assert_import_rejected(&env, "unsupported export version: '1' (expected 1)");

    let mut env = sample_envelope("user@example.com".into(), 1.into());
    env["encrypted"] = true.into();
    assert_import_rejected(
        &env,
        &hint(
            "encrypted exports are not supported in this version — decrypt before piping (e.g. gpg -d backup.gpg | cswap --import -)",
        ),
    );

    let mut env = sample_envelope("user@example.com".into(), 1.into());
    env["accounts"] = json!([]);
    assert_import_rejected(&env, "export file has no accounts to import");
    assert_import_rejected(&json!([1]), "export file must be a JSON object");
}

#[test]
fn malformed_top_level_json_rejected() {
    let env = TestEnv::new();
    let file = backup_file(&env, "bad.cswap");
    std::fs::write(&file, "{not json").unwrap();
    let out = import(&env, &file, &[]);
    out.assert_code(1);
    assert!(
        out.stderr
            .starts_with("Error: export file is not valid JSON: "),
        "{}",
        out.stderr
    );
}

#[test]
fn credential_shapes_rejected() {
    let mut env = sample_envelope("user@example.com".into(), 1.into());
    env["accounts"][0]["credentials"] = "a string".into();
    assert_import_rejected(
        &env,
        "API-key credentials for user@example.com must be a raw sk-ant-api… string",
    );
    env["accounts"][0]["credentials"] = json!([1, 2, 3]);
    assert_import_rejected(
        &env,
        "credentials for user@example.com must be a JSON object",
    );
    env["accounts"][0]["credentials"] = json!({"a": 1});
    env["accounts"][0]["config"] = json!("nope");
    assert_import_rejected(&env, "config for user@example.com must be a JSON object");
}

#[test]
fn string_fields_reject_non_string_types() {
    for field in [
        "organizationUuid",
        "organizationName",
        "uuid",
        "added",
        "alias",
    ] {
        for (bad, ty) in [
            (json!(["a", "b"]), "list"),
            (json!({"x": 1}), "dict"),
            (json!(42), "int"),
        ] {
            let mut env = sample_envelope("user@example.com".into(), 1.into());
            env["accounts"][0][field] = bad;
            assert_import_rejected(
                &env,
                &format!("{field} for user@example.com must be a string, got {ty}"),
            );
        }
    }
}

#[test]
fn missing_file_rejected() {
    let env = TestEnv::new();
    let file = backup_file(&env, "nope.cswap");
    let out = import(&env, &file, &[]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        format!("Error: import file not found: {}\n", file.display())
    );
}

#[test]
fn malformed_later_account_does_not_partial_write() {
    let mut envelope = sample_envelope("alice@example.com".into(), 1.into());
    let mut second = envelope["accounts"][0].clone();
    second["number"] = 2.into();
    second["email"] = "../../evil".into();
    envelope["accounts"].as_array_mut().unwrap().push(second);
    assert_import_rejected(
        &envelope,
        "invalid or missing email in imported account: '../../evil'",
    );
}

#[test]
fn duplicate_account_in_export_rejected() {
    let mut envelope = sample_envelope("alice@example.com".into(), 1.into());
    let mut second = envelope["accounts"][0].clone();
    second["number"] = 2.into();
    envelope["accounts"].as_array_mut().unwrap().push(second);
    assert_import_rejected(
        &envelope,
        "duplicate account in export: alice@example.com (org=personal)",
    );
}

#[test]
fn a_linux_export_imports_into_the_keychain() {
    let env = TestEnv::new();
    let file = backup_file(&env, "linux.cswap");
    write_envelope(&file, &sample_envelope("user@example.com".into(), 3.into()));
    import(&env, &file, &[]).assert_code(0);
    assert_eq!(
        stored_creds_text(&env, "3", "user@example.com").unwrap(),
        r#"{"accessToken": "tok-1", "refreshToken": "rtok-1", "expiresAt": 9999}"#
    );
    let seq = env.sequence();
    assert_eq!(seq["activeAccountNumber"], 3);
    assert_eq!(seq["accounts"]["3"]["uuid"], "u");
}

#[test]
fn export_to_stdout_writes_only_json() {
    let env = TestEnv::new();
    seed(&env, Seed::new(1, "alice@example.com"));
    let out = env.run(["export", "-"]);
    out.assert_code(0);
    assert_eq!(out.stderr, "");
    assert!(out.stdout.ends_with("}\n"));
    assert_eq!(emails(&out.json()), ["alice@example.com"]);
}

#[test]
fn import_from_stdin() {
    let env = TestEnv::new();
    let envelope = sample_envelope("alice@example.com".into(), 1.into());
    let out = env.cmd(["import", "-"]).stdin(&envelope.to_string()).run();
    out.assert_code(0);
    assert_eq!(
        env.sequence()["accounts"]["1"]["email"],
        "alice@example.com"
    );
}

#[test]
fn import_into_empty_home_initializes_store() {
    let src = TestEnv::new();
    seed(&src, Seed::new(1, "alice@example.com"));
    let file = backup_file(&src, "x.cswap");
    export_to(&src, &file, &[]);

    let dst = TestEnv::new();
    assert!(!dst.store_dir().exists());
    import(&dst, &file, &[]).assert_code(0);
    for dir in ["", "configs", "credentials"] {
        assert_eq!(mode(&dst.store_dir().join(dir)), 0o700, "{dir}");
    }
    let seq = dst.sequence();
    assert_eq!(seq["accounts"]["1"]["email"], "alice@example.com");
    assert_eq!(
        seq.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["activeAccountNumber", "lastUpdated", "sequence", "accounts"]
    );
}

#[test]
fn export_file_is_0600_and_leaves_no_temp() {
    let env = TestEnv::new();
    seed(&env, Seed::new(1, "alice@example.com"));
    let dir = env.root().join("out");
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join("x.cswap");
    export_to(&env, &file, &[]);
    assert_eq!(mode(&file), 0o600);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
}

#[test]
fn export_to_a_directory_is_refused() {
    let env = TestEnv::new();
    seed(&env, Seed::new(1, "alice@example.com"));
    let dir = env.root().join("out");
    std::fs::create_dir(&dir).unwrap();
    let out = env.run(["export", dir.to_str().unwrap()]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        format!(
            "Error: export destination must be a file path, not a directory: {}\n",
            dir.display()
        )
    );
}

#[test]
fn export_expands_tilde() {
    let env = TestEnv::new();
    seed(&env, Seed::new(1, "alice@example.com"));
    let out = env.run(["export", "~/b.cswap"]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        format!(
            "Exported 1 account(s) to {}\n",
            env.home().join("b.cswap").display()
        )
    );
    assert!(env.home().join("b.cswap").exists());
}

fn export_alice_bob_active_bob() -> (TestEnv, PathBuf) {
    let src = TestEnv::new();
    seed(&src, Seed::new(1, "alice@example.com"));
    seed(&src, Seed::new(2, "bob@example.com"));
    set_active(&src, json!(2));
    let file = backup_file(&src, "backup.cswap");
    export_to(&src, &file, &[]);
    (src, file)
}

#[test]
fn import_preserves_existing_active_account() {
    let (_src, file) = export_alice_bob_active_bob();
    let dst = TestEnv::new();
    seed(&dst, Seed::new(5, "local@example.com"));
    import(&dst, &file, &[]).assert_code(0);
    assert_eq!(dst.sequence()["activeAccountNumber"], 5);
}

#[test]
fn active_seeded_to_resolved_slot_not_envelope_slot() {
    let (_src, file) = export_alice_bob_active_bob();
    let dst = TestEnv::new();
    seed(&dst, Seed::new(2, "local@example.com"));
    set_active(&dst, Value::Null);
    import(&dst, &file, &[]).assert_code(0);
    let seq = dst.sequence();
    assert_eq!(seq["accounts"]["2"]["email"], "local@example.com");
    let bob = slot_of(&seq, "bob@example.com");
    assert_ne!(bob, "2");
    assert_eq!(
        seq["activeAccountNumber"],
        json!(bob.parse::<i64>().unwrap())
    );
}

#[test]
fn active_seeded_when_envelope_active_was_skipped() {
    let (_src, file) = export_alice_bob_active_bob();
    let dst = TestEnv::new();
    seed(&dst, Seed::new(7, "bob@example.com"));
    set_active(&dst, json!(0));
    import(&dst, &file, &[]).assert_code(0);
    let seq = dst.sequence();
    assert_eq!(seq["accounts"]["7"]["email"], "bob@example.com");
    assert_eq!(seq["activeAccountNumber"], 7);
}

fn bloated_config() -> Value {
    json!({
        "oauthAccount": {"emailAddress": "alice@example.com", "accountUuid": "acct-uuid",
                         "organizationUuid": "org-a", "organizationName": "Acme"},
        "userID": "host-machine-identity",
        "anonymousId": "anon-host-id",
        "projects": {"/Users/host/repo": {}},
        "tipsHistory": {"welcome": 1, "shift-enter": 5},
        "cachedGrowthBookFeatures": {"flag-a": true, "flag-b": false},
        "numStartups": 42,
        "ratio": 0.5,
        "name": "caf\u{e9}",
    })
}

fn siblinged_creds() -> Value {
    json!({
        "claudeAiOauth": {"accessToken": "sk-live", "refreshToken": "rt-live"},
        "mcpOAuth": {"linear": {"refreshToken": "mcp-rt"}},
        "trustedDeviceToken": "device-token-a",
        "someFutureField": {"value": 1},
    })
}

#[test]
fn default_export_keeps_only_the_login() {
    let env = TestEnv::new();
    seed(
        &env,
        Seed::new(1, "alice@example.com")
            .org("org-a", "")
            .config(bloated_config())
            .creds(siblinged_creds()),
    );
    let envelope = export_to(&env, &backup_file(&env, "slim.cswap"), &[]);
    assert_eq!(
        envelope["accounts"][0]["config"],
        json!({"oauthAccount": bloated_config()["oauthAccount"]})
    );
    assert_eq!(
        envelope["accounts"][0]["credentials"],
        json!({"claudeAiOauth": {"accessToken": "sk-live", "refreshToken": "rt-live"}})
    );
}

#[test]
fn full_export_keeps_everything_byte_for_byte() {
    let env = TestEnv::new();
    seed(
        &env,
        Seed::new(1, "alice@example.com")
            .org("org-a", "")
            .config(bloated_config())
            .creds(siblinged_creds()),
    );
    let file = backup_file(&env, "full.cswap");
    let envelope = export_to(&env, &file, &["--full"]);
    assert_eq!(envelope["accounts"][0]["config"], bloated_config());
    assert_eq!(envelope["accounts"][0]["credentials"], siblinged_creds());
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("\"name\": \"caf\\u00e9\""), "ensure_ascii");
    assert!(text.contains("\"ratio\": 0.5"));
}

#[test]
fn legacy_shape_without_claude_ai_oauth_exports_verbatim() {
    let legacy = json!({"accessToken": "tok-legacy", "refreshToken": "rt-legacy"});
    let env = TestEnv::new();
    seed(
        &env,
        Seed::new(1, "alice@example.com").creds(legacy.clone()),
    );
    let envelope = export_to(&env, &backup_file(&env, "legacy.cswap"), &[]);
    assert_eq!(envelope["accounts"][0]["credentials"], legacy);
}

#[test]
fn export_missing_oauth_account_raises() {
    let env = TestEnv::new();
    seed(
        &env,
        Seed::new(1, "alice@example.com").config(json!({"projects": {}, "userID": "x"})),
    );
    let out = env.run(["export", backup_file(&env, "x.cswap").to_str().unwrap()]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: config for alice@example.com is missing oauthAccount — cannot export\n"
    );
}

#[test]
fn api_key_accounts_round_trip_as_raw_strings() {
    let src = TestEnv::new();
    seed(&src, Seed::new(1, "api-key-1@token.local"));
    src.security().set_item(
        BACKUP_SERVICE,
        "account-1-api-key-1@token.local",
        "  sk-ant-api03-secret\n",
    );
    let mut data = src.sequence();
    data["accounts"]["1"]["kind"] = "api_key".into();
    src.seed_sequence(data);
    let file = backup_file(&src, "keys.cswap");
    let envelope = export_to(&src, &file, &[]);
    assert_eq!(
        envelope["accounts"][0]["credentials"],
        "sk-ant-api03-secret"
    );
    assert_eq!(envelope["accounts"][0]["kind"], "api_key");

    let dst = TestEnv::new();
    import(&dst, &file, &[]).assert_code(0);
    assert_eq!(
        stored_creds_text(&dst, "1", "api-key-1@token.local").unwrap(),
        "sk-ant-api03-secret"
    );
    let record = &dst.sequence()["accounts"]["1"];
    assert_eq!(record["kind"], "api_key");
    let keys: Vec<&str> = record
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "email",
            "uuid",
            "organizationUuid",
            "organizationName",
            "added",
            "kind"
        ]
    );
}

fn seed_alice_without_creds_and_bob(env: &TestEnv) {
    let alice = Seed::new(1, "alice@example.com");
    seed_backup_config(
        env,
        1,
        "alice@example.com",
        &config_for(1, "alice@example.com", "", ""),
    );
    seed_record(env, &alice);
    seed(env, Seed::new(2, "bob@example.com"));
}

#[test]
fn all_accounts_skips_missing_credentials_with_stderr_warning() {
    let env = TestEnv::new();
    seed_alice_without_creds_and_bob(&env);
    let file = backup_file(&env, "backup.cswap");
    let out = env.run(["export", file.to_str().unwrap()]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        hint(&format!(
            "Skipping Account-1 (alice@example.com): no stored credentials/config — re-add with: cswap --add-account --slot 1\nExported 1 account(s) to {}\n",
            file.display()
        ))
    );
    let envelope: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(emails(&envelope), ["bob@example.com"]);
    assert_eq!(
        envelope["activeAccountNumber"],
        Value::Null,
        "the skipped active slot is not advertised"
    );
}

#[test]
fn all_accounts_skips_missing_config() {
    let env = TestEnv::new();
    seed(&env, Seed::new(1, "alice@example.com"));
    seed(&env, Seed::new(2, "bob@example.com"));
    std::fs::remove_file(
        env.store_dir()
            .join("configs/.claude-config-1-alice@example.com.json"),
    )
    .unwrap();
    let out = env.run(["export", "-"]);
    out.assert_code(0);
    assert!(
        out.stderr
            .starts_with("Skipping Account-1 (alice@example.com)")
    );
    assert_eq!(emails(&out.json()), ["bob@example.com"]);
}

#[test]
fn explicit_account_with_missing_backup_hard_fails() {
    let env = TestEnv::new();
    seed_alice_without_creds_and_bob(&env);
    let out = env.run(["export", "-", "--account", "1"]);
    out.assert_code(1);
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "Error: no backup credentials found for account 1 (alice@example.com)\n"
    );

    std::fs::remove_file(
        env.store_dir()
            .join("configs/.claude-config-2-bob@example.com.json"),
    )
    .unwrap();
    let out = env.run(["export", "-", "--account", "2"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: no backup config found for account 2 (bob@example.com)\n"
    );
}

#[test]
fn all_slots_broken_raises() {
    let env = TestEnv::new();
    let alice = Seed::new(1, "alice@example.com");
    seed_record(&env, &alice);
    let out = env.run(["export", "-"]);
    out.assert_code(1);
    assert!(out.stderr.ends_with(
        &hint("Error: no exportable accounts — all managed slots are missing stored credentials/config. Re-add with: cswap --add-account --slot <number>\n")
    ), "{}", out.stderr);
}

#[test]
fn active_account_exports_from_the_live_store() {
    let env = TestEnv::new();
    seed(&env, Seed::new(1, "alice@example.com"));
    env.seed_login("alice@example.com", "", "");
    let live = json!({"claudeAiOauth": {"accessToken": "fresh", "refreshToken": "rt-fresh"}});
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &live.to_string());
    let envelope = env.run(["export", "-"]).json();
    assert_eq!(envelope["accounts"][0]["credentials"], live);
    assert_eq!(
        envelope["accounts"][0]["config"]["oauthAccount"]["accountUuid"],
        "uuid-alice@example.com"
    );
}

#[test]
fn active_account_without_live_credentials_fails() {
    let env = TestEnv::new();
    seed(&env, Seed::new(1, "alice@example.com"));
    env.seed_login("alice@example.com", "", "");
    let out = env.run(["export", "-"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: failed to read live credentials for active account alice@example.com\n"
    );
}

fn export_bob_with_marker(env: &TestEnv, marker: Option<&str>) -> PathBuf {
    let file = backup_file(env, "bob.cswap");
    let mut envelope = export_to(env, &file, &["--account", "2"]);
    if let Some(marker) = marker {
        envelope["accounts"][0]["credentials"]["claudeAiOauth"]["marker"] = marker.into();
        write_envelope(&file, &envelope);
    }
    file
}

fn assert_cleared(row: &Value) {
    assert_eq!(row["authDeadStrikes"], 0, "{row}");
    assert_eq!(row["struckFingerprint"], Value::Null, "{row}");
    assert_eq!(row["lastError"], Value::Null, "{row}");
}

#[test]
fn import_force_lifts_quarantine() {
    let env = TestEnv::new();
    seed(&env, Seed::new(2, "bob@example.com"));
    strike(&env, json!({"2": dead_row("bob@example.com", "", None)}));
    let file = export_bob_with_marker(&env, Some("BOB_NEW"));

    let out = import(&env, &file, &["--force"]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        "Overwrote bob@example.com (slot 2)\n  └ cleared this slot's stored dead-token strike\nDone: 0 imported, 1 overwritten, 0 skipped\n"
    );
    assert_cleared(&usage_row(&env, "2"));
    assert_eq!(
        stored_creds(&env, "2", "bob@example.com")["claudeAiOauth"]["marker"],
        "BOB_NEW"
    );
}

#[test]
fn force_import_of_the_struck_generation_lifts_and_narrates() {
    let env = TestEnv::new();
    seed(&env, Seed::new(2, "bob@example.com"));
    strike(
        &env,
        json!({"2": dead_row("bob@example.com", "", Some(RT_BOB_FP))}),
    );
    let file = export_bob_with_marker(&env, None);

    let out = import(&env, &file, &["--force"]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        "Overwrote bob@example.com (slot 2)\n  └ cleared this slot's stored dead-token strike\n  └ this import holds the same credential generation the strike condemned; another permanent auth failure will quarantine it again — recover with a newer export or a re-login\nDone: 0 imported, 1 overwritten, 0 skipped\n"
    );
    assert_cleared(&usage_row(&env, "2"));
}

#[test]
fn force_overwrite_with_a_newer_generation_omits_the_generation_note() {
    let env = TestEnv::new();
    seed(&env, Seed::new(2, "bob@example.com"));
    strike(
        &env,
        json!({"2": dead_row("bob@example.com", "", Some(RT_BOB_FP))}),
    );
    let file = backup_file(&env, "bob.cswap");
    let mut envelope = export_to(&env, &file, &["--account", "2"]);
    envelope["accounts"][0]["credentials"]["claudeAiOauth"]["refreshToken"] =
        "rt-bob-rotated".into();
    write_envelope(&file, &envelope);

    let out = import(&env, &file, &["--force"]);
    out.assert_code(0);
    assert!(
        out.stderr
            .contains("cleared this slot's stored dead-token strike")
    );
    assert!(!out.stderr.contains("same credential generation"));
}

#[test]
fn force_overwrite_narrates_only_the_struck_account() {
    let env = TestEnv::new();
    seed(&env, Seed::new(2, "bob@example.com"));
    seed(&env, Seed::new(3, "carol@example.com"));
    strike(&env, json!({"2": dead_row("bob@example.com", "", None)}));
    let file = backup_file(&env, "both.cswap");
    export_to(&env, &file, &[]);

    let out = import(&env, &file, &["--force"]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        "Overwrote bob@example.com (slot 2)\n  └ cleared this slot's stored dead-token strike\nOverwrote carol@example.com (slot 3)\nDone: 0 imported, 2 overwritten, 0 skipped\n"
    );
}

#[test]
fn force_overwrite_narrates_a_condemned_generation_without_a_refresh_token() {
    let env = TestEnv::new();
    seed(
        &env,
        Seed::new(2, "bob@example.com").creds(json!({"accessToken": "tok-1", "expiresAt": 9999})),
    );
    strike(
        &env,
        json!({"2": dead_row("bob@example.com", "", Some(LEGACY_NO_REFRESH_FP))}),
    );
    let file = backup_file(&env, "bob.cswap");
    export_to(&env, &file, &["--account", "2"]);

    let out = import(&env, &file, &["--force"]);
    out.assert_code(0);
    assert!(
        out.stderr.contains("same credential generation"),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains("invalid_grant"));
}

#[test]
fn reimport_after_removal_lifts_orphan_quarantine() {
    let env = TestEnv::new();
    seed(&env, Seed::new(2, "bob@example.com"));
    strike(&env, json!({"2": dead_row("bob@example.com", "", None)}));
    let file = export_bob_with_marker(&env, None);
    let mut data = env.sequence();
    data["accounts"].as_object_mut().unwrap().shift_remove("2");
    data["sequence"] = json!([]);
    env.seed_sequence(data);

    let out = import(&env, &file, &[]);
    out.assert_code(0);
    assert!(
        out.stderr
            .starts_with("Imported bob@example.com → slot 2\n")
    );
    assert_cleared(&usage_row(&env, "2"));
}

#[test]
fn plain_import_replaces_quarantined_slot() {
    let env = TestEnv::new();
    seed(&env, Seed::new(2, "bob@example.com"));
    strike(&env, json!({"2": dead_row("bob@example.com", "", None)}));
    let file = export_bob_with_marker(&env, Some("BOB_HEALED"));

    let out = import(&env, &file, &[]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        "Replaced bob@example.com (slot 2 was quarantined: refresh token dead)\nDone: 0 imported, 0 overwritten, 0 skipped, 1 replaced (dead token)\n"
    );
    assert_cleared(&usage_row(&env, "2"));
    assert_eq!(
        stored_creds(&env, "2", "bob@example.com")["claudeAiOauth"]["marker"],
        "BOB_HEALED"
    );
}

#[test]
fn a_healed_strike_does_not_license_a_plain_import_to_replace() {
    let env = TestEnv::new();
    seed(&env, Seed::new(2, "bob@example.com"));
    strike(
        &env,
        json!({"2": dead_row("bob@example.com", "", Some("sha256:someothergeneration"))}),
    );
    let file = export_bob_with_marker(&env, Some("BOB_OVERWRITTEN"));

    let out = import(&env, &file, &[]);
    out.assert_code(0);
    assert!(
        out.stderr.contains("already exists, use --force"),
        "{}",
        out.stderr
    );
    assert_eq!(
        stored_creds(&env, "2", "bob@example.com")["claudeAiOauth"]["marker"],
        "bob@example.com"
    );
}

#[test]
fn the_heal_finds_the_row_of_an_org_scoped_account() {
    let env = TestEnv::new();
    seed(
        &env,
        Seed::new(2, "bob@example.com").org("org-uuid-1234", "Org"),
    );
    strike(
        &env,
        json!({"2": dead_row("bob@example.com", "org-uuid-1234", None)}),
    );
    let file = export_bob_with_marker(&env, Some("BOB_HEALED"));
    let out = import(&env, &file, &[]);
    out.assert_code(0);
    assert!(
        out.stderr.starts_with("Replaced bob@example.com (slot 2"),
        "{}",
        out.stderr
    );
}

#[test]
fn import_heals_an_active_slot_struck_on_its_live_generation() {
    let env = TestEnv::new();
    let newer_backup = json!({"claudeAiOauth": {"accessToken": "sk-bk", "refreshToken": "rt-bk",
                                               "expiresAt": 9_999_999_999_000_i64}});
    seed(&env, Seed::new(2, "bob@example.com").creds(newer_backup));
    env.seed_login("bob@example.com", "", "");
    env.security().set_item(
        ACTIVE_OAUTH_SERVICE,
        KEYCHAIN_ACCOUNT,
        &json!({"claudeAiOauth": {"accessToken": "sk-live", "refreshToken": "rt-live",
                                  "expiresAt": 9_999_999_999_000_i64}})
        .to_string(),
    );
    strike(
        &env,
        json!({"2": dead_row("bob@example.com", "", Some(RT_LIVE_FP))}),
    );
    let file = backup_file(&env, "bob.cswap");
    let mut envelope = export_to(&env, &file, &["--account", "2"]);
    envelope["accounts"][0]["credentials"]["claudeAiOauth"]["marker"] = "BOB_HEALED".into();
    write_envelope(&file, &envelope);

    let out = import(&env, &file, &[]);
    out.assert_code(0);
    assert!(
        out.stderr.contains("was quarantined: refresh token dead"),
        "{}",
        out.stderr
    );
}

#[test]
fn plain_import_skips_healthy_slot() {
    let env = TestEnv::new();
    seed(&env, Seed::new(2, "bob@example.com"));
    let file = export_bob_with_marker(&env, Some("BOB_NEW"));
    let out = import(&env, &file, &[]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        "Skipped bob@example.com (already exists, use --force)\nDone: 0 imported, 0 overwritten, 1 skipped\n"
    );
    assert_eq!(
        stored_creds(&env, "2", "bob@example.com")["claudeAiOauth"]["marker"],
        "bob@example.com"
    );
}

#[test]
fn plain_import_ignores_foreign_dead_row_on_slot() {
    let env = TestEnv::new();
    seed(&env, Seed::new(2, "bob@example.com"));
    strike(&env, json!({"2": dead_row("alice@example.com", "", None)}));
    let file = export_bob_with_marker(&env, Some("BOB_NEW"));
    let out = import(&env, &file, &[]);
    out.assert_code(0);
    assert!(out.stderr.starts_with("Skipped bob@example.com"));
    assert_eq!(usage_row(&env, "2")["email"], "alice@example.com");
}

#[test]
fn fresh_slot_import_creates_no_quarantine() {
    let src = TestEnv::new();
    seed(&src, Seed::new(2, "bob@example.com"));
    let file = export_bob_with_marker(&src, None);
    let dst = TestEnv::new();
    import(&dst, &file, &[]).assert_code(0);
    let row = usage_row(&dst, "2");
    assert_eq!(row["email"], "bob@example.com");
    assert_cleared(&row);
}

fn normalize_volatile(text: &str) -> Value {
    let mut envelope: Value = serde_json::from_str(text).unwrap();
    envelope["exportedAt"] = Value::Null;
    envelope["swapVersion"] = Value::Null;
    envelope
}

#[test]
fn ccswap_and_cswap_write_the_same_envelope() {
    if !is_upstream() {
        return;
    }
    let env = TestEnv::new();
    seed(
        &env,
        Seed::new(1, "alice@example.com")
            .org("org-a", "Org A")
            .config(bloated_config())
            .creds(siblinged_creds())
            .alias("dev"),
    );
    seed(&env, Seed::new(3, "bob@example.com"));
    for extra in [&[][..], &["--full"][..]] {
        let mut ours = vec!["export", "-"];
        ours.extend_from_slice(extra);
        let a = env.cmd(&ours).using(Impl::Ccswap).run();
        let b = env.cmd(&ours).using(Impl::Upstream).run();
        a.assert_code(0);
        b.assert_code(0);
        assert_eq!(normalize_volatile(&a.stdout), normalize_volatile(&b.stdout));
        let strip = |s: &str| {
            s.lines()
                .filter(|l| !l.contains("\"exportedAt\"") && !l.contains("\"swapVersion\""))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            strip(&a.stdout),
            strip(&b.stdout),
            "byte-identical apart from volatile fields"
        );
    }
}

#[test]
fn files_cross_between_ccswap_and_cswap() {
    if !is_upstream() {
        return;
    }
    for (writer, reader) in [
        (Impl::Ccswap, Impl::Upstream),
        (Impl::Upstream, Impl::Ccswap),
    ] {
        let src = TestEnv::new();
        seed(
            &src,
            Seed::new(1, "alice@example.com")
                .org("org-a", "Org A")
                .alias("dev"),
        );
        seed(&src, Seed::new(2, "bob@example.com"));
        let file = backup_file(&src, "x.cswap");
        src.cmd(["export", file.to_str().unwrap()])
            .using(writer)
            .run()
            .assert_code(0);

        let dst = TestEnv::new();
        let out = dst
            .cmd(["import", file.to_str().unwrap()])
            .using(reader)
            .run();
        out.assert_code(0);
        assert!(out.stderr.contains("Done: 2 imported"), "{}", out.stderr);
        let seq = dst.sequence();
        assert_eq!(seq["accounts"]["1"]["alias"], "dev");
        assert_eq!(
            stored_creds(&dst, "1", "alice@example.com"),
            creds_for("alice@example.com")
        );
    }
}
