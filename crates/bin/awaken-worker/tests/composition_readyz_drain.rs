//! The legacy standalone worker binary must not recreate the deleted
//! environment-configured composition path.

use std::process::Command;

#[test]
fn standalone_worker_fails_closed_with_the_canonical_migration_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_awaken-worker"))
        .output()
        .expect("run standalone worker binary");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("awaken worker --config <PATH> --server <URL>"),
        "stable migration guidance: {stderr}"
    );
}
