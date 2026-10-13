//! Ported from upstream tests/test_cli.py::TestCLI::test_version_flag.

mod support;

use support::{TestEnv, is_upstream, prog};

#[test]
fn version_flag_prints_program_name_and_version() {
    let env = TestEnv::new();

    let out = env.run(["--version"]);

    out.assert_code(0);
    let version = if is_upstream() {
        "0.26.0"
    } else {
        env!("CARGO_PKG_VERSION")
    };
    assert_eq!(out.stdout, format!("{} {version}\n", prog()));
}

#[test]
fn version_names_ccswap_whatever_the_invoked_name() {
    if is_upstream() {
        return;
    }
    let env = TestEnv::new();

    let out = env.cmd(["--version"]).arg0("/opt/bin/cs").run();

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!("ccswap {}\n", env!("CARGO_PKG_VERSION"))
    );
}
