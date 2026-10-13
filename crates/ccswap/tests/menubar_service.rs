//! Ported from upstream tests/test_launch_agent.py and the service cases of
//! tests/test_cli.py.
#![cfg(target_os = "macos")]

mod support;

use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use support::{Impl, TestEnv, is_upstream, prog};

const LABEL: &str = "com.cswap.menubar";

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn plist_path(env: &TestEnv) -> PathBuf {
    env.home()
        .join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

/// The plist as launchd's own parser reads it.
fn read_plist(env: &TestEnv) -> Value {
    let out = Command::new("plutil")
        .args(["-convert", "json", "-o", "-", "--"])
        .arg(plist_path(env))
        .output()
        .expect("run plutil");
    assert!(
        out.status.success(),
        "plutil: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn service_target() -> String {
    format!("gui/{}/{LABEL}", uid())
}

#[test]
fn install_writes_the_plist_and_bootstraps_it() {
    let env = TestEnv::new();
    let out = env.run(["menubar", "--install-service"]);
    out.assert_code(0);

    let plist = plist_path(&env);
    assert!(plist.is_file());
    let calls: Vec<Vec<String>> = env
        .launchctl()
        .calls()
        .into_iter()
        .map(|c| c.argv)
        .collect();
    assert!(calls.contains(&vec![
        "bootstrap".to_string(),
        format!("gui/{}", uid()),
        plist.display().to_string(),
    ]));
    assert!(env.launchctl().is_loaded());
}

fn running_print(pid: &str) -> String {
    format!(
        "{} = {{\n\tstate = running\n\tpid = {pid}\n}}\n",
        service_target()
    )
}

#[test]
fn install_points_launchd_at_this_binary_and_prints_where_it_went() {
    let env = TestEnv::new();
    let out = env.run(["menubar", "--install-service"]);
    out.assert_code(0);

    let plist = read_plist(&env);
    let logs = env.home().join("Library/Logs");
    if !is_upstream() {
        assert_eq!(
            plist["ProgramArguments"],
            json!([env!("CARGO_BIN_EXE_ccswap"), "menubar"])
        );
    }
    assert_eq!(plist["Label"], LABEL);
    assert_eq!(plist["RunAtLoad"], true);
    assert_eq!(plist["KeepAlive"], json!({"SuccessfulExit": false}));
    assert_eq!(plist["ProcessType"], "Interactive");
    assert_eq!(
        plist["StandardErrorPath"],
        logs.join(format!("{LABEL}.err")).display().to_string()
    );
    assert!(logs.is_dir());
    assert_eq!(
        out.stdout,
        format!(
            "Menu bar service installed ({LABEL}).\n  plist: {}\n  logs:  {}\n\
             It starts at login from now on. Re-run this after a {} upgrade to point launchd at the new build.\n",
            plist_path(&env).display(),
            logs.join(format!("{LABEL}.err")).display(),
            prog()
        )
    );
}

#[test]
fn install_replaces_an_already_loaded_service_by_booting_it_out_first() {
    let env = TestEnv::new();
    env.write_file(plist_path(&env), "<plist>python cswap's</plist>");
    env.launchctl().load(&running_print("111"));

    env.run(["menubar", "--install-service"]).assert_code(0);

    let calls = env.launchctl().calls();
    let subs = env.launchctl().subcommands();
    let bootout = subs
        .iter()
        .position(|s| s == "bootout")
        .expect("booted out");
    assert_eq!(calls[bootout].argv, ["bootout", &service_target()]);
    assert!(bootout < subs.iter().position(|s| s == "bootstrap").unwrap());
    assert!(env.launchctl().is_loaded());
    assert_eq!(read_plist(&env)["Label"], LABEL);
}

#[test]
fn install_does_not_boot_out_when_nothing_is_loaded() {
    let env = TestEnv::new();
    env.run(["menubar", "--install-service"]).assert_code(0);
    assert_eq!(env.launchctl().subcommands(), ["print", "bootstrap"]);
}

#[test]
fn install_waits_for_the_old_job_to_go_away_before_bootstrapping() {
    let env = TestEnv::new();
    env.launchctl().load(&running_print("111"));
    let lingering = (0, "\tstate = running\n", "");
    env.launchctl()
        .script("print", &[lingering, lingering, lingering, lingering]);

    env.run(["menubar", "--install-service"]).assert_code(0);

    let subs = env.launchctl().subcommands();
    let bootout = subs.iter().position(|s| s == "bootout").unwrap();
    let bootstrap = subs.iter().position(|s| s == "bootstrap").unwrap();
    assert_eq!(subs[bootout + 1..bootstrap], ["print"; 4]);
}

#[test]
fn install_gives_up_waiting_and_names_the_lingering_predecessor_when_bootstrap_fails() {
    let env = TestEnv::new();
    env.launchctl().load(&running_print("111"));
    env.launchctl().script("print", &[(0, "", ""); 200]);
    env.launchctl()
        .script("bootstrap", &[(5, "", "Operation already in progress\n")]);

    let started = std::time::Instant::now();
    let out = env.run(["menubar", "--install-service"]);
    out.assert_code(1);

    assert!(started.elapsed() >= std::time::Duration::from_secs(5));
    assert_eq!(
        out.stderr,
        "Error: launchctl bootstrap failed (exit 5): Operation already in progress; the previous instance was still shutting down\n"
    );
}

#[test]
fn install_raises_with_launchctl_detail_when_bootstrap_fails() {
    let env = TestEnv::new();
    env.launchctl()
        .script("bootstrap", &[(5, "", "Input/output error\n")]);
    let out = env.run(["menubar", "--install-service"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: launchctl bootstrap failed (exit 5): Input/output error\n"
    );
}

#[test]
fn uninstall_boots_out_and_removes_the_plist() {
    let env = TestEnv::new();
    env.run(["menubar", "--install-service"]).assert_code(0);

    let out = env.run(["menubar", "--uninstall-service"]);
    out.assert_code(0);

    assert_eq!(out.stdout, "Menu bar service removed.\n");
    assert!(!plist_path(&env).exists());
    assert!(!env.launchctl().is_loaded());
    let calls: Vec<Vec<String>> = env
        .launchctl()
        .calls()
        .into_iter()
        .map(|c| c.argv)
        .collect();
    assert!(calls.contains(&vec!["bootout".to_string(), service_target()]));
}

#[test]
fn uninstall_is_quiet_when_nothing_is_installed() {
    let env = TestEnv::new();
    let out = env.run(["menubar", "--uninstall-service"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Menu bar service was not installed.\n");
    assert_eq!(env.launchctl().subcommands(), ["print"]);
}

#[test]
fn uninstall_removes_a_plist_that_was_never_bootstrapped() {
    let env = TestEnv::new();
    env.write_file(plist_path(&env), "x");
    let out = env.run(["menubar", "--uninstall-service"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Menu bar service removed.\n");
    assert!(!plist_path(&env).exists());
}

#[test]
fn uninstall_raises_when_bootout_fails_and_the_service_stays_loaded() {
    let env = TestEnv::new();
    env.write_file(plist_path(&env), "x");
    env.launchctl().load(&running_print("111"));
    env.launchctl().script("bootout", &[(3, "", "in use\n")]);

    let out = env.run(["menubar", "--uninstall-service"]);
    out.assert_code(1);

    assert_eq!(
        out.stderr,
        "Error: launchctl bootout failed (exit 3): in use\n"
    );
    assert!(plist_path(&env).exists());
}

#[test]
fn uninstall_tolerates_a_bootout_race_that_already_unloaded_it() {
    let env = TestEnv::new();
    env.launchctl().script("print", &[(0, "", "")]);
    env.launchctl()
        .script("bootout", &[(3, "", "No such process\n")]);

    let out = env.run(["menubar", "--uninstall-service"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Menu bar service removed.\n");
}

#[test]
fn status_reads_the_jobs_own_state_and_pid_not_a_nested_blocks() {
    let env = TestEnv::new();
    env.run(["menubar", "--install-service"]).assert_code(0);
    env.launchctl().load(&format!(
        "{} = {{\n\tactive count = 1\n\tstate = running\n\tpid = 25026\n\
         \tpid-local endpoints = {{\n\t\tstate = active\n\t\tpid = 999\n\t}}\n}}\n",
        service_target()
    ));

    let out = env.run(["menubar", "--service-status"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!(
            "Menu bar service: running (pid 25026)\n  plist: {}\n",
            plist_path(&env).display()
        )
    );
}

#[test]
fn status_keeps_multi_word_launchd_states_and_ignores_a_non_numeric_pid() {
    let env = TestEnv::new();
    env.write_file(plist_path(&env), "x");
    env.launchctl()
        .load("\tstate = spawn scheduled\n\tpid = (none)\n");

    let out = env.run(["menubar", "--service-status"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout.lines().next().unwrap(),
        "Menu bar service: spawn scheduled"
    );
}

#[test]
fn status_reports_not_installed_with_the_install_hint() {
    let env = TestEnv::new();
    let out = env.run(["menubar", "--service-status"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!(
            "Menu bar service is not installed.\nInstall it with: {} menubar --install-service\n",
            prog()
        )
    );
}

#[test]
fn status_separates_installed_from_loaded() {
    let env = TestEnv::new();
    env.write_file(plist_path(&env), "x");
    let out = env.run(["menubar", "--service-status"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!(
            "Menu bar service: stopped\n  plist: {}\n",
            plist_path(&env).display()
        )
    );
}

#[test]
fn status_notices_a_loaded_service_whose_plist_is_gone() {
    let env = TestEnv::new();
    env.launchctl().load("\tpid = 7\n");
    let out = env.run(["menubar", "--service-status"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!(
            "Menu bar service: loaded (pid 7)\n  plist: {}\nlaunchd still has it loaded, but the plist is gone.\n",
            plist_path(&env).display()
        )
    );
}

#[test]
fn the_plist_is_byte_identical_to_what_plistlib_writes() {
    let env = TestEnv::new();
    env.run(["menubar", "--install-service"]).assert_code(0);
    let bin = env!("CARGO_BIN_EXE_ccswap");
    let bin_dir = std::path::Path::new(bin)
        .parent()
        .unwrap()
        .display()
        .to_string();
    let home = env.home().display().to_string();
    // Captured from plistlib.dumps of the same dict.
    let expected = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>{bin_dir}:{home}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
	</dict>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>Label</key>
	<string>com.cswap.menubar</string>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>ProgramArguments</key>
	<array>
		<string>{bin}</string>
		<string>menubar</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>StandardErrorPath</key>
	<string>{home}/Library/Logs/com.cswap.menubar.err</string>
	<key>StandardOutPath</key>
	<string>{home}/Library/Logs/com.cswap.menubar.log</string>
</dict>
</plist>
"#
    );
    assert_eq!(std::fs::read_to_string(plist_path(&env)).unwrap(), expected);
}

#[test]
fn ccswap_install_replaces_a_python_cswap_service_and_uninstall_removes_it() {
    if !is_upstream() {
        return;
    }
    let env = TestEnv::new();
    env.cmd(["menubar", "--install-service"])
        .using(Impl::Upstream)
        .run()
        .assert_code(0);
    assert_ne!(
        read_plist(&env)["ProgramArguments"][0],
        env!("CARGO_BIN_EXE_ccswap")
    );

    env.cmd(["menubar", "--install-service"])
        .using(Impl::Ccswap)
        .run()
        .assert_code(0);
    assert_eq!(
        read_plist(&env)["ProgramArguments"],
        json!([env!("CARGO_BIN_EXE_ccswap"), "menubar"])
    );
    assert!(
        env.launchctl()
            .subcommands()
            .contains(&"bootout".to_string())
    );
    assert!(env.launchctl().is_loaded());

    env.cmd(["menubar", "--uninstall-service"])
        .using(Impl::Ccswap)
        .run()
        .assert_code(0);
    let status = env
        .cmd(["menubar", "--service-status"])
        .using(Impl::Upstream)
        .run();
    assert_eq!(
        status.stdout.lines().next(),
        Some("Menu bar service is not installed.")
    );
}

#[test]
fn service_flags_are_rejected_outside_menubar() {
    let env = TestEnv::new();
    let out = env.run(["list", "--install-service"]);
    out.assert_code(2);
    assert!(out.stderr.contains(
        "--install-service, --uninstall-service and --service-status can only be used with 'menubar'"
    ));
    assert!(env.launchctl().calls().is_empty());
}
