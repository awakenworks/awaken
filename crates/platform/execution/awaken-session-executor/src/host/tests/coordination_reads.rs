use super::*;

#[tokio::test]
async fn coordination_read_role_retains_threads_without_the_command_components() {
    // Cause/effect graph: C1 retain only the selected read role; C2 release
    // the executor composition and other roles; C3 release the read role.
    // S1=C1+C2 -> release ChildRunCoordination and SessionRuns, retain the
    // original Threads/read source and its existing embedded lowering owner,
    // and answer committed reads. S2=S1+C3 -> release those collaborators.
    // The embedded lowering still needs RunAssembly: this test must not claim
    // that selecting a narrow read trait erases that existing dependency.
    let host = crate::SessionExecutorBuilder::ephemeral(Arc::new(MemoryHostModel), "stub")
        .await
        .build()
        .expect("existing ephemeral executor");
    let parent = Arc::downgrade(&host);
    let commands = Arc::downgrade(&host.coordination);
    let runs = Arc::downgrade(&host.session_runs);
    let threads = Arc::downgrade(&host.session_runs.threads);
    let reads = Arc::downgrade(&host.thread_reads);
    let assembly = Arc::downgrade(&host.run_assembly);
    let roles = host.session_execution_roles();
    let selected = roles.coordination_reads.clone();
    drop(roles);
    drop(host);
    assert!(parent.upgrade().is_none(), "S1 composition released");
    assert!(commands.upgrade().is_none(), "S1 commands released");
    assert!(runs.upgrade().is_none(), "S1 Run delivery released");
    assert!(threads.upgrade().is_some(), "S1 original Threads retained");
    assert!(
        reads.upgrade().is_some(),
        "S1 original committed reads retained"
    );
    assert!(
        assembly.upgrade().is_some(),
        "S1 original embedded lowering retained"
    );
    assert!(
        selected
            .delegated_runs("coordination-read-role")
            .await
            .expect("S1 query")
            .is_empty()
    );
    drop(selected);
    assert!(threads.upgrade().is_none(), "S2 Threads released");
    assert!(reads.upgrade().is_none(), "S2 committed reads released");
    assert!(
        assembly.upgrade().is_none(),
        "S2 embedded lowering released"
    );
}
