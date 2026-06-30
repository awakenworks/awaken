//! Interactive coding-agent TUI backed by a real model through `genai`.
//!
//! ```text
//! cargo run -p awaken-runtime-examples --example coding_agent \
//!     --features coding-agent-tui
//! ```
//!
//! Model selection (env):
//! - `AWAKEN_MODEL` — the model ref the binding selects (default `MiniMax-M3`).
//! - MiniMax (Anthropic-compatible endpoint): set `MINIMAX_API_KEY` and
//!   optionally `MINIMAX_BASE_URL` (default `https://api.minimaxi.com/anthropic`).
//! - Otherwise `genai`'s default client reads `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`.
//!
//! The agent reads/searches/edits files under the current directory; it asks
//! before each `write`/`edit`/`bash` (answer `y` to allow).

use std::sync::Arc;

use awaken_runtime_examples::coding_agent::model::build_executor;
use awaken_runtime_examples::coding_agent::{CodingSession, build_runtime, coding_config, tui};

fn main() -> anyhow::Result<()> {
    let model = std::env::var("AWAKEN_MODEL").unwrap_or_else(|_| "MiniMax-M3".to_string());
    let llm = Arc::new(build_executor()?);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let session = CodingSession::new(build_runtime(llm), coding_config(&model));
    tui::run(runtime, session, &model)?;
    Ok(())
}
