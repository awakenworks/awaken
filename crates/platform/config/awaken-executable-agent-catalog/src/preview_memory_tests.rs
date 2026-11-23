use super::*;
use crate::test_support::registration;
use std::process::Command;

const CHILD_CASE: &str = "AWAKEN_CATALOG_PREVIEW_RESOURCE_CASE";
const HISTORY_COUNT: u64 = 256;
const PAYLOAD_BYTES: usize = 128 * 1024;

fn peak_resident_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn large_registration(revision: u64) -> ExecutableAgentRegistration {
    let mut command = registration(revision, &format!("fp-{revision}"));
    command.session_profile.system = Some("x".repeat(PAYLOAD_BYTES));
    command
}

#[test]
fn point_preview_memory_does_not_scale_with_retained_history() {
    // Cause graph: C1 a large immutable history exists; C2 the point command
    // registers a new revision, replays an exact revision, or withdraws current.
    // Effect E1: temporary preview memory is bounded by the touched command,
    // not all historical profiles; E2: preview leaves history/current intact.
    // Decision rules R1/R2/R3 select each C2 with C1. Each rule gets a fresh
    // process so parallel tests and previous allocation peaks cannot hide E1.
    // The relative allowance includes eight command payloads plus 4 MiB of
    // allocator slack, well below the 32 MiB retained profile history. This
    // Linux resource regression diagnoses copying, not a production leak or
    // long-term OOM qualification. Semantic combinations live in preview_tests.
    let Ok(case) = std::env::var(CHILD_CASE) else {
        let mut failures = Vec::new();
        for case in ["new", "replay", "withdraw"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "preview_memory_tests::point_preview_memory_does_not_scale_with_retained_history",
                    "--nocapture",
                ])
                .env(CHILD_CASE, case)
                .output()
                .unwrap();
            if !output.status.success() {
                failures.push(format!(
                    "{case}: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            } else {
                print!("{}", String::from_utf8_lossy(&output.stdout));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        return;
    };

    let catalog = ExecutableAgentCatalog::new();
    for revision in 1..=HISTORY_COUNT {
        ExecutableAgentCatalog::register_locked(
            &mut catalog.write_state().unwrap(),
            large_registration(revision),
        )
        .unwrap();
    }
    let before = peak_resident_kib();
    match case.as_str() {
        "new" => assert_eq!(
            catalog.preview_registration(large_registration(HISTORY_COUNT + 1)),
            Ok(ExecutableAgentRegistrationOutcome::RegisteredCurrent)
        ),
        "replay" => assert_eq!(
            catalog.preview_registration(large_registration(1)),
            Ok(ExecutableAgentRegistrationOutcome::AlreadyRegistered)
        ),
        "withdraw" => assert_eq!(
            catalog.preview_withdrawal(ExecutableAgentWithdrawal {
                workspace_id: "workspace-a".into(),
                agent_id: "agent-a".into(),
                lifecycle_revision: HISTORY_COUNT,
            }),
            Ok(ExecutableAgentWithdrawalOutcome::WithdrawnCurrent)
        ),
        other => panic!("unknown resource regression case {other}"),
    }
    let extra_kib = peak_resident_kib().saturating_sub(before);
    let allowance_kib = (PAYLOAD_BYTES as u64 / 1024) * 8 + 4096;
    println!("{case}: preview peak increase {extra_kib} KiB; allowance {allowance_kib} KiB");
    assert!(extra_kib <= allowance_kib, "R1/R2/R3 history-sized copy");
    assert_eq!(
        catalog.read_state().unwrap().revisions.len(),
        HISTORY_COUNT as usize,
        "E2 history unchanged"
    );
    assert_eq!(
        catalog
            .current("workspace-a", "agent-a")
            .unwrap()
            .source_revision,
        HISTORY_COUNT,
        "E2 current unchanged"
    );
}
