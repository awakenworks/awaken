//! T01-C03: the Session MCP effect owner the dispatch resolver holds is
//! released with its Host; a retained strong owner is detectable.

use super::*;

/// The release oracle: after every Host view and MCP effect owner is dropped,
/// no strong owner of the assembly remains.
fn assembly_released(weak: &std::sync::Weak<crate::RunAssembly>) -> Result<(), String> {
    match weak.upgrade() {
        None => Ok(()),
        Some(alive) => Err(format!(
            "assembly retains {} strong owner(s)",
            Arc::strong_count(&alive) - 1
        )),
    }
}

/// | Rule | owners dropped | retained clone | Effect |
/// |---|---|---|---|
/// | R1 | Host + `RunMcpEffects` | none | assembly released |
/// | NC | Host + `RunMcpEffects` | one leaked `Arc` | check reports the owner; released after it drops |
#[tokio::test]
async fn direct_mcp_effects_release_the_assembly_and_a_retained_owner_is_detected() {
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let before = Arc::strong_count(&host.run_assembly);
    // The same constructor the Worker dispatch resolver uses
    // (host/dispatch_execution.rs): the local Direct realizer holds the assembly.
    let effects = crate::session_effects::RunMcpEffects::new(host.run_assembly.clone());
    assert_eq!(
        Arc::strong_count(&host.run_assembly),
        before + 1,
        "R1 the Direct realizer owns exactly one assembly reference"
    );
    let weak = Arc::downgrade(&host.run_assembly);
    drop(effects);
    drop(host);
    assembly_released(&weak).expect("R1 no cycle through the MCP effect owner");

    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let effects = crate::session_effects::RunMcpEffects::new(host.run_assembly.clone());
    let weak = Arc::downgrade(&host.run_assembly);
    let leaked = host.run_assembly.clone();
    drop(effects);
    drop(host);
    let detected = assembly_released(&weak).expect_err("NC a retained owner is detected");
    assert!(
        detected.contains("1 strong owner"),
        "NC exactly the leaked owner: {detected}"
    );
    drop(leaked);
    assembly_released(&weak).expect("NC released once the leaked owner drops");
}
