//! The Coordinator executable runs the shared service lifecycle in the
//! Coordinator role.
//!
//! Causal decision table:
//! | invocation | config/storage/network | outcome |
//! | --- | --- | --- |
//! | `--help` | unavailable | the Coordinator role's service usage, success |
//! | an unknown argument | unavailable | usage on failure, no service started |
//! | serve with a missing configuration file | unavailable | failure naming the executable, no data written |

use std::process::Command;

fn coordinator() -> Command {
    let executable = std::env::var("CARGO_BIN_EXE_awaken-coordinator")
        .or_else(|_| std::env::var("NEXTEST_BIN_EXE_awaken-coordinator"))
        .expect("Cargo must provide the awaken-coordinator path when this integration test runs");
    Command::new(executable)
}

#[test]
fn coordinator_executable_serves_the_coordinator_role_usage() {
    let help = coordinator().arg("--help").output().expect("run --help");
    assert!(help.status.success(), "{:?}", help.stderr);
    let usage = String::from_utf8(help.stdout).expect("utf-8 usage");
    assert!(usage.starts_with("awaken-coordinator\n"), "{usage}");
    assert!(
        usage.contains("awaken-coordinator database migrate"),
        "{usage}"
    );

    let unknown = coordinator()
        .arg("--not-an-option")
        .output()
        .expect("run an unknown argument");
    assert!(!unknown.status.success());
    let stderr = String::from_utf8(unknown.stderr).expect("utf-8 error");
    assert!(stderr.starts_with("awaken-coordinator: "), "{stderr}");
}

#[test]
fn coordinator_executable_refuses_a_missing_configuration_without_effects() {
    let root = std::env::temp_dir().join(format!(
        "awaken-coordinator-missing-config-{}",
        std::process::id()
    ));
    let missing = root.join("absent.toml");
    let output = coordinator()
        .args(["--config", missing.to_str().expect("utf-8 path")])
        .output()
        .expect("run serve");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf-8 error");
    assert!(stderr.starts_with("awaken-coordinator: "), "{stderr}");
    assert!(!root.exists(), "no data root was created");
}
