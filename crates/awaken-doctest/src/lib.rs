//! Compile-tests for book documentation code examples.
//!
//! This crate uses `doc_comment::doctest!` to compile-test Rust code blocks
//! in the mdbook documentation. Any `rust` or `rust,no_run` block in the
//! included markdown files will be compiled (but not executed) as part of
//! `cargo test -p awaken-doctest`.
//!
//! Code blocks tagged `rust,ignore` are skipped.
//!
//! To add a new doc file: add a `doctest!()` line below and convert its key
//! code blocks from `rust,ignore` to `rust,no_run` (with hidden `# ` setup
//! lines as needed).

#[cfg(doctest)]
mod book_tests {
    use doc_comment::doctest;

    // ── Introduction ───────────────────────────────────────────
    doctest!("../../../docs/book/src/introduction.md");

    // ── Tutorials ──────────────────────────────────────────────
    doctest!("../../../docs/book/src/tutorials/first-agent.md");
    doctest!("../../../docs/book/src/tutorials/first-tool.md");

    // ── How-to guides ──────────────────────────────────────────
    doctest!("../../../docs/book/src/how-to/add-a-plugin.md");
    doctest!("../../../docs/book/src/how-to/add-a-tool.md");
    doctest!("../../../docs/book/src/how-to/build-an-agent.md");
    doctest!("../../../docs/book/src/how-to/configure-stop-policies.md");
    doctest!("../../../docs/book/src/how-to/enable-observability.md");
    doctest!("../../../docs/book/src/how-to/enable-tool-permission-hitl.md");
    doctest!("../../../docs/book/src/how-to/expose-http-sse.md");
    doctest!("../../../docs/book/src/how-to/integrate-ai-sdk-frontend.md");
    doctest!("../../../docs/book/src/how-to/integrate-copilotkit-ag-ui.md");
    doctest!("../../../docs/book/src/how-to/optimize-context-window.md");
    doctest!("../../../docs/book/src/how-to/report-tool-progress.md");
    doctest!("../../../docs/book/src/how-to/testing-strategy.md");
    doctest!("../../../docs/book/src/how-to/use-agent-handoff.md");
    doctest!("../../../docs/book/src/how-to/use-deferred-tools.md");
    doctest!("../../../docs/book/src/how-to/use-file-store.md");
    doctest!("../../../docs/book/src/how-to/use-generative-ui.md");
    doctest!("../../../docs/book/src/how-to/use-mcp-tools.md");
    doctest!("../../../docs/book/src/how-to/use-postgres-store.md");
    doctest!("../../../docs/book/src/how-to/use-reminder-plugin.md");
    doctest!("../../../docs/book/src/how-to/use-skills-subsystem.md");

    // ── Reference ──────────────────────────────────────────────
    doctest!("../../../docs/book/src/reference/cancellation.md");
    doctest!("../../../docs/book/src/reference/config.md");
    doctest!("../../../docs/book/src/reference/effects.md");
    doctest!("../../../docs/book/src/reference/errors.md");
    doctest!("../../../docs/book/src/reference/events.md");
    doctest!("../../../docs/book/src/reference/http-api.md");
    doctest!("../../../docs/book/src/reference/overview.md");
    doctest!("../../../docs/book/src/reference/protocols/a2a.md");
    doctest!("../../../docs/book/src/reference/protocols/ag-ui.md");
    doctest!("../../../docs/book/src/reference/protocols/ai-sdk-v6.md");
    doctest!("../../../docs/book/src/reference/scheduled-actions.md");
    doctest!("../../../docs/book/src/reference/state-keys.md");
    doctest!("../../../docs/book/src/reference/thread-model.md");
    doctest!("../../../docs/book/src/reference/tool-execution-modes.md");
    doctest!("../../../docs/book/src/reference/tool-trait.md");

    // ── Explanation ────────────────────────────────────────────
    doctest!("../../../docs/book/src/explanation/agent-resolution.md");
    doctest!("../../../docs/book/src/explanation/architecture.md");
    doctest!("../../../docs/book/src/explanation/design-tradeoffs.md");
    doctest!("../../../docs/book/src/explanation/hitl-and-mailbox.md");
    doctest!("../../../docs/book/src/explanation/multi-agent-patterns.md");
    doctest!("../../../docs/book/src/explanation/plugin-internals.md");
    doctest!("../../../docs/book/src/explanation/run-lifecycle-and-phases.md");
    doctest!("../../../docs/book/src/explanation/state-and-snapshot-model.md");
    doctest!("../../../docs/book/src/explanation/tool-and-plugin-boundary.md");

    // ── Appendix ───────────────────────────────────────────────
    doctest!("../../../docs/book/src/appendix/faq.md");
    doctest!("../../../docs/book/src/appendix/glossary.md");
    doctest!("../../../docs/book/src/appendix/migration-from-tirea.md");

    // ── Chinese translations ───────────────────────────────────
    doctest!("../../../docs/book/src/zh-CN/introduction.md");
    doctest!("../../../docs/book/src/zh-CN/tutorials/first-agent.md");
    doctest!("../../../docs/book/src/zh-CN/tutorials/first-tool.md");
    doctest!("../../../docs/book/src/zh-CN/explanation/agent-resolution.md");
    doctest!("../../../docs/book/src/zh-CN/explanation/architecture.md");
    doctest!("../../../docs/book/src/zh-CN/explanation/plugin-internals.md");
    doctest!("../../../docs/book/src/zh-CN/how-to/testing-strategy.md");
    doctest!("../../../docs/book/src/zh-CN/how-to/use-deferred-tools.md");
}
