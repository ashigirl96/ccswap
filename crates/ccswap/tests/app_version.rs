//! Ported from upstream tests/test_cli.py::TestCLI::test_version_flag.

mod support;

use support::TestEnv;

#[test]
fn version_flag_prints_program_name_and_version() {
    let env = TestEnv::new();

    let out = env.run(["--version"]);

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!("ccswap {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn version_names_ccswap_whatever_the_invoked_name() {
    let env = TestEnv::new();

    let out = env.cmd(["--version"]).arg0("/opt/bin/cs").run();

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!("ccswap {}\n", env!("CARGO_PKG_VERSION"))
    );
}
