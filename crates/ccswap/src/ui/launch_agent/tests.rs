use super::*;

use std::io::Write;
use std::os::unix::fs::symlink;
use std::process::Stdio;

use serde_json::{Value, json};

const PROGRAM: &str = "/Users/x/.local/bin/ccswap";

/// The serialized plist as Apple's own parser reads it.
fn parsed(home: &Path) -> Value {
    let xml = build_plist(&[PROGRAM.to_string()], home).to_xml();
    let mut child = Command::new("plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run plutil");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(xml.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "plutil rejected:\n{xml}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn path_entries(home: &Path) -> Vec<String> {
    parsed(home)["EnvironmentVariables"]["PATH"]
        .as_str()
        .unwrap()
        .split(':')
        .map(str::to_string)
        .collect()
}

#[test]
fn build_plist_is_parseable_and_runs_the_menubar_subcommand() {
    let plist = parsed(Path::new("/Users/x"));
    assert_eq!(plist["Label"], "com.cswap.menubar");
    assert_eq!(plist["ProgramArguments"], json!([PROGRAM, "menubar"]));
    assert_eq!(plist["RunAtLoad"], true);
}

#[test]
fn build_plist_keepalive_restarts_a_crash_but_respects_a_quit() {
    let plist = parsed(Path::new("/Users/x"));
    assert_eq!(plist["KeepAlive"], json!({"SuccessfulExit": false}));
}

#[test]
fn build_plist_marks_the_agent_interactive_not_background() {
    assert_eq!(parsed(Path::new("/Users/x"))["ProcessType"], "Interactive");
}

#[test]
fn build_plist_logs_to_the_users_library_logs() {
    let plist = parsed(Path::new("/Users/x"));
    assert_eq!(
        plist["StandardOutPath"],
        "/Users/x/Library/Logs/com.cswap.menubar.log"
    );
    assert_eq!(
        plist["StandardErrorPath"],
        "/Users/x/Library/Logs/com.cswap.menubar.err"
    );
}

#[test]
fn build_plist_survives_paths_that_would_break_hand_written_xml() {
    let odd = Path::new("/tmp/home & <co>");
    assert_eq!(
        parsed(odd)["StandardErrorPath"],
        "/tmp/home & <co>/Library/Logs/com.cswap.menubar.err"
    );
}

#[test]
fn build_plist_path_env_leads_with_the_programs_own_directory() {
    assert_eq!(
        path_entries(Path::new("/Users/x"))[0],
        "/Users/x/.local/bin"
    );
}

#[test]
fn build_plist_path_env_includes_the_user_bin_dir() {
    let entries = path_entries(Path::new("/Users/y"));
    assert!(entries.contains(&"/Users/y/.local/bin".to_string()));
}

#[test]
fn build_plist_path_env_keeps_the_launchd_defaults() {
    let entries = path_entries(Path::new("/Users/x"));
    assert_eq!(
        entries,
        [
            "/Users/x/.local/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin"
        ]
    );
}

fn script(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "#!/bin/sh\n").unwrap();
}

fn no_exe() -> Option<PathBuf> {
    panic!("fell back to the running executable")
}

#[test]
fn resolve_program_prefers_the_installed_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = tmp.path().join("ccswap");
    script(&binary);
    let found = resolve_program(Some(binary.as_os_str()), tmp.path(), None, no_exe);
    assert_eq!(found, [binary.display().to_string()]);
}

#[test]
fn resolve_program_keeps_the_symlink_and_does_not_follow_it() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("Cellar/ccswap/0.1.0/bin/ccswap");
    script(&real);
    let link = tmp.path().join("local/bin/ccswap");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&real, &link).unwrap();
    let found = resolve_program(Some(link.as_os_str()), tmp.path(), None, no_exe);
    assert_eq!(found, [link.display().to_string()]);
}

#[test]
fn resolve_program_makes_a_relative_argv0_absolute() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = tmp.path().join("ccswap");
    script(&binary);
    let found = resolve_program(Some(OsStr::new("./ccswap")), tmp.path(), None, no_exe);
    assert_eq!(found, [binary.display().to_string()]);
}

#[test]
fn resolve_program_ignores_an_argv0_that_is_not_ccswap() {
    let tmp = tempfile::tempdir().unwrap();
    let other = tmp.path().join("cargo-test-runner");
    script(&other);
    let on_path = PathBuf::from("/opt/homebrew/bin/ccswap");
    let found = resolve_program(Some(other.as_os_str()), tmp.path(), Some(on_path), no_exe);
    assert_eq!(found, ["/opt/homebrew/bin/ccswap"]);
}

#[test]
fn resolve_program_looks_up_a_bare_argv0_on_path() {
    let tmp = tempfile::tempdir().unwrap();
    let on_path = PathBuf::from("/opt/homebrew/bin/ccswap");
    let found = resolve_program(
        Some(OsStr::new("ccswap")),
        tmp.path(),
        Some(on_path),
        no_exe,
    );
    assert_eq!(found, ["/opt/homebrew/bin/ccswap"]);
}

#[test]
fn resolve_program_falls_back_to_the_running_executable() {
    let tmp = tempfile::tempdir().unwrap();
    let gone = tmp.path().join("gone");
    let found = resolve_program(Some(gone.as_os_str()), tmp.path(), None, || {
        Some(PathBuf::from("/x/target/release/ccswap"))
    });
    assert_eq!(found, ["/x/target/release/ccswap"]);
}
