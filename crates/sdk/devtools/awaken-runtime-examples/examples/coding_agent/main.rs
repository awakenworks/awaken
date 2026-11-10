//! Interactive coding-agent TUI backed by a real model through `genai`.
//!
//! ```text
//! cargo run -p awaken-runtime-examples --example coding_agent \
//!     --features coding-agent-tui
//! ```
//!
//! Model selection (env):
//! - `AWAKEN_MODEL` — required for generic endpoints; named MiniMax and Kimi
//!   configurations supply defaults.
//! - Kimi Code (OpenAI-compatible): set `KIMI_API_KEY`, optionally `KIMI_BASE_URL`
//!   (default `https://api.kimi.com/coding/v1`).
//! - MiniMax (Anthropic-compatible): set `MINIMAX_API_KEY`, optionally
//!   `MINIMAX_BASE_URL` (default `https://api.minimaxi.com/anthropic`).
//! - Generic endpoints: set `OPENAI_API_KEY` / `OPENAI_BASE_URL` or
//!   `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL`, plus `AWAKEN_MODEL`.
//!
//! The agent reads/searches/edits files under the current directory; it asks
//! before each `write`/`edit`/`bash` (answer `y` to allow).

use awaken_runtime_examples::coding_agent::model::build_executor;
use awaken_runtime_examples::coding_agent::{CodingSession, build_runtime, coding_config, tui};

fn main() -> anyhow::Result<()> {
    let (llm, model) = build_executor()?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let clock = awaken_time::system_clock();
    let session = CodingSession::new(build_runtime(llm), coding_config(&model)?, clock);
    tui::run(runtime, session, &model)?;
    Ok(())
}
