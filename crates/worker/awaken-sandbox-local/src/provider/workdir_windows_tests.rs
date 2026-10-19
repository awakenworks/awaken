// @formal surface obligation=sandbox.fail_closed_policy_never_downgrades
// @formal reason: The Windows provider probe rejects an actual reparse directory;
// @formal reason: this test is executable evidence, not a universal proof.
//! Windows Workdir filesystem admission at the LocalProvider boundary.

use super::*;

#[tokio::test]
async fn windows_workdir_probe_requires_a_plain_local_ntfs_ancestor() {
    // Causes: C1 base absent below a plain fixed-NTFS ancestor; C2 a real
    // reparse directory is made by the test process or pre-created by the
    // platform harness when this host refuses test-launched `mklink`.
    // Effects: E1 prospective base is ready without creation; E2 either C2
    // fixture is independently verified as a reparse point, then rejected
    // before any Sandbox directory is created. Rules W1=C1->E1;
    // W2=C2(internal|external)->E2. No absent or ordinary external directory
    // can silently replace the junction coverage.
    let temp = tempfile::tempdir().unwrap();
    let prospective = temp.path().join("future").join("sandboxes");
    let provider = LocalProvider::new(&prospective);
    pc::SandboxReadiness::probe_ready(&provider)
        .await
        .expect("E1");
    assert!(!prospective.exists(), "E1 read-only probe");

    let junction = if let Some(external) = std::env::var_os("AWAKEN_TEST_WINDOWS_WORKDIR_JUNCTION")
    {
        std::path::PathBuf::from(external)
    } else {
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let junction = temp.path().join("junction");
        let output = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "E2 fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        junction
    };
    use std::os::windows::fs::MetadataExt as _;
    let attributes = std::fs::symlink_metadata(&junction)
        .unwrap()
        .file_attributes();
    assert_ne!(
        attributes & 0x400,
        0,
        "E2 fixture must be a real reparse point"
    );
    assert!(
        pc::SandboxReadiness::probe_ready(&LocalProvider::new(&junction))
            .await
            .is_err(),
        "E2"
    );
}
