//! Compile-guard for the interactive TUI feature (`coding-agent-tui`).
//!
//! The TUI itself (`src/coding_agent/tui.rs`) drives a real terminal, so it has
//! no unit-constructible render state to exercise headless. This guard exists so
//! the feature path can't rot uncaught: `cargo test -p awaken-runtime-examples
//! --features coding-agent-tui` must compile the whole feature (tui + model +
//! genai/ratatui/crossterm deps) and pass.
//!
//! It pins the public entry points the `coding_agent` example wires — `tui::run`
//! and `model::build_executor` — to their exact signatures, and exercises the
//! non-render session pieces the TUI drives (config + runtime + one turn) with a
//! scripted model, so a rename or signature drift breaks the build here.
#![cfg(feature = "coding-agent-tui")]

use std::sync::Arc;

use awaken_runtime_examples::coding_agent::model::build_executor;
use awaken_runtime_examples::coding_agent::{
    Approval, CodingSession, ScriptedCoder, build_runtime, coding_config, tui,
};

/// Pin the TUI's public entry points to their exact signatures. This forces the
/// whole `coding-agent-tui` feature path (tui.rs + model.rs and their optional
/// deps) to type-check, so it can't silently rot out of CI.
#[test]
fn tui_public_entry_points_keep_their_signatures() {
    // `tui::run(runtime, session, model) -> io::Result<()>` — the loop the
    // example launches. Held as a fn pointer; naming it type-checks the module.
    let _run: fn(tokio::runtime::Runtime, CodingSession, &str) -> std::io::Result<()> = tui::run;

    // `model::build_executor() -> anyhow::Result<Arc<dyn LlmExecutor>>` — the
    // real genai-backed model port the TUI drives.
    let _build: fn() -> anyhow::Result<Arc<dyn awaken_runtime_contract::llm::LlmExecutor>> =
        build_executor;
}

/// Exercise the non-render session pieces the TUI's `run` loop drives — config,
/// runtime, and one `CodingSession::turn` — with a scripted model, so the feature
/// path is compiled and lightly run without a terminal.
#[tokio::test]
async fn tui_session_pieces_run_one_turn_without_a_terminal() {
    let dir = std::env::temp_dir().join(format!("awaken_coding_tui_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("notes.txt");
    std::fs::write(&file, "status: TODO\n").unwrap();
    let path = file.to_str().unwrap().to_string();

    // The same assembly the example builds: scripted model → runtime → session
    // on a `coding_config` (the model ref the TUI passes through).
    let llm = Arc::new(ScriptedCoder::new(path.clone(), "TODO", "DONE"));
    let session = CodingSession::new(build_runtime(llm), coding_config("scripted-model"));

    // Drive one turn, approving the mutating tool — the same `session.turn` call
    // the TUI issues on Enter, minus the terminal draw.
    session
        .turn("Mark the status done.", |_ticket| Approval::Allow)
        .await
        .expect("turn runs");

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "status: DONE\n",
        "the session the TUI drives applied the approved edit"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
