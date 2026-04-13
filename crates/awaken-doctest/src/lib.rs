//! Compile-tests for book documentation code examples.
//!
//! This crate uses `doc_comment::doctest!` to compile-test Rust code blocks
//! in the mdbook documentation. Any `rust` or `rust,no_run` block in the
//! included markdown files will be compiled (but not executed) as part of
//! `cargo test -p awaken-doctest`.
//!
//! Code blocks tagged `rust,ignore` are skipped.
//!
//! To add a new doc file: add it to `book_doctests!()` below and convert its
//! key examples from `rust,ignore` to `rust,no_run` when they can be compiled
//! without executing external services.

macro_rules! book_doctests {
    ($($path:literal),+ $(,)?) => {
        #[allow(dead_code)]
        const BOOK_DOCTEST_FILES: &[&str] = &[$($path),+];

        #[cfg(doctest)]
        mod book_tests {
            use doc_comment::doctest;

            $(doctest!($path);)+
        }
    };
}

book_doctests!(
    "../../../docs/book/src/introduction.md",
    "../../../docs/book/src/tutorials/first-agent.md",
    "../../../docs/book/src/tutorials/first-tool.md",
    "../../../docs/book/src/how-to/add-a-plugin.md",
    "../../../docs/book/src/how-to/add-a-tool.md",
    "../../../docs/book/src/how-to/build-an-agent.md",
    "../../../docs/book/src/how-to/configure-stop-policies.md",
    "../../../docs/book/src/how-to/enable-observability.md",
    "../../../docs/book/src/how-to/enable-tool-permission-hitl.md",
    "../../../docs/book/src/how-to/expose-http-sse.md",
    "../../../docs/book/src/how-to/integrate-ai-sdk-frontend.md",
    "../../../docs/book/src/how-to/integrate-copilotkit-ag-ui.md",
    "../../../docs/book/src/how-to/optimize-context-window.md",
    "../../../docs/book/src/how-to/report-tool-progress.md",
    "../../../docs/book/src/how-to/testing-strategy.md",
    "../../../docs/book/src/how-to/use-agent-handoff.md",
    "../../../docs/book/src/how-to/use-deferred-tools.md",
    "../../../docs/book/src/how-to/use-file-store.md",
    "../../../docs/book/src/how-to/use-generative-ui.md",
    "../../../docs/book/src/how-to/use-mcp-tools.md",
    "../../../docs/book/src/how-to/use-postgres-store.md",
    "../../../docs/book/src/how-to/use-reminder-plugin.md",
    "../../../docs/book/src/how-to/use-shared-state.md",
    "../../../docs/book/src/how-to/use-skills-subsystem.md",
    "../../../docs/book/src/reference/cancellation.md",
    "../../../docs/book/src/reference/config.md",
    "../../../docs/book/src/reference/effects.md",
    "../../../docs/book/src/reference/errors.md",
    "../../../docs/book/src/reference/events.md",
    "../../../docs/book/src/reference/http-api.md",
    "../../../docs/book/src/reference/overview.md",
    "../../../docs/book/src/reference/protocols/a2a.md",
    "../../../docs/book/src/reference/protocols/ag-ui.md",
    "../../../docs/book/src/reference/protocols/ai-sdk-v6.md",
    "../../../docs/book/src/reference/provider-model-config.md",
    "../../../docs/book/src/reference/scheduled-actions.md",
    "../../../docs/book/src/reference/state-keys.md",
    "../../../docs/book/src/reference/thread-model.md",
    "../../../docs/book/src/reference/tool-execution-modes.md",
    "../../../docs/book/src/reference/tool-trait.md",
    "../../../docs/book/src/explanation/agent-resolution.md",
    "../../../docs/book/src/explanation/architecture.md",
    "../../../docs/book/src/explanation/design-tradeoffs.md",
    "../../../docs/book/src/explanation/hitl-and-mailbox.md",
    "../../../docs/book/src/explanation/multi-agent-patterns.md",
    "../../../docs/book/src/explanation/plugin-internals.md",
    "../../../docs/book/src/explanation/run-lifecycle-and-phases.md",
    "../../../docs/book/src/explanation/state-and-snapshot-model.md",
    "../../../docs/book/src/explanation/tool-and-plugin-boundary.md",
    "../../../docs/book/src/appendix/faq.md",
    "../../../docs/book/src/appendix/glossary.md",
    "../../../docs/book/src/appendix/migration-from-tirea.md",
    "../../../docs/book/src/zh-CN/introduction.md",
    "../../../docs/book/src/zh-CN/tutorials/first-agent.md",
    "../../../docs/book/src/zh-CN/tutorials/first-tool.md",
    "../../../docs/book/src/zh-CN/explanation/agent-resolution.md",
    "../../../docs/book/src/zh-CN/explanation/architecture.md",
    "../../../docs/book/src/zh-CN/explanation/plugin-internals.md",
    "../../../docs/book/src/zh-CN/how-to/testing-strategy.md",
    "../../../docs/book/src/zh-CN/how-to/use-deferred-tools.md",
);

#[cfg(test)]
mod coverage_tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::BOOK_DOCTEST_FILES;

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .components()
            .collect()
    }

    fn doctest_rel_path(path: &str) -> String {
        path.strip_prefix("../../../")
            .unwrap_or(path)
            .replace('\\', "/")
    }

    fn collect_summary_links(root: &Path, summary_rel: &str, base_rel: &str) -> Vec<String> {
        let summary = fs::read_to_string(root.join(summary_rel))
            .unwrap_or_else(|err| panic!("read {summary_rel}: {err}"));
        let mut links = Vec::new();

        for line in summary.lines() {
            let mut rest = line;
            while let Some(start) = rest.find("](./") {
                let after_marker = &rest[start + 2..];
                let Some(end) = after_marker.find(')') else {
                    break;
                };
                let rel = &after_marker[..end];
                let Some(rel) = rel.strip_prefix("./") else {
                    rest = &after_marker[end + 1..];
                    continue;
                };
                if rel.ends_with(".md") {
                    links.push(format!("{base_rel}/{rel}"));
                }
                rest = &after_marker[end + 1..];
            }
        }

        links
    }

    fn has_compiled_rust_fence(root: &Path, rel: &str) -> bool {
        let text =
            fs::read_to_string(root.join(rel)).unwrap_or_else(|err| panic!("read {rel}: {err}"));
        text.lines().any(|line| {
            let line = line.trim_start();
            (line.starts_with("```rust") || line.starts_with("```rs")) && !line.contains("ignore")
        })
    }

    fn hidden_lines_in_compiled_rust_fences(root: &Path, rel: &str) -> Vec<usize> {
        let text =
            fs::read_to_string(root.join(rel)).unwrap_or_else(|err| panic!("read {rel}: {err}"));
        let mut in_fence = false;
        let mut compiled_rust = false;
        let mut hidden_lines = Vec::new();

        for (idx, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                if in_fence {
                    in_fence = false;
                    compiled_rust = false;
                } else {
                    let info = trimmed.trim_start_matches("```").trim();
                    compiled_rust = (info.starts_with("rust") || info.starts_with("rs"))
                        && !info.contains("ignore");
                    in_fence = true;
                }
                continue;
            }

            if in_fence && compiled_rust && (line == "#" || line.starts_with("# ")) {
                hidden_lines.push(idx + 1);
            }
        }

        hidden_lines
    }

    #[test]
    fn listed_book_files_exist() {
        let root = workspace_root();
        let missing = BOOK_DOCTEST_FILES
            .iter()
            .map(|path| doctest_rel_path(path))
            .filter(|rel| !root.join(rel).is_file())
            .collect::<Vec<_>>();

        assert!(
            missing.is_empty(),
            "book doctest list references missing files:\n{}",
            missing.join("\n")
        );
    }

    #[test]
    fn summary_rust_examples_are_doctested() {
        let root = workspace_root();
        let listed = BOOK_DOCTEST_FILES
            .iter()
            .map(|path| doctest_rel_path(path))
            .collect::<BTreeSet<_>>();

        let mut summary_files =
            collect_summary_links(&root, "docs/book/src/SUMMARY.md", "docs/book/src");
        summary_files.extend(collect_summary_links(
            &root,
            "docs/book/src/zh-CN/SUMMARY.md",
            "docs/book/src/zh-CN",
        ));

        let missing = summary_files
            .into_iter()
            .filter(|rel| has_compiled_rust_fence(&root, rel) && !listed.contains(rel))
            .collect::<Vec<_>>();

        assert!(
            missing.is_empty(),
            "SUMMARY pages with non-ignored Rust examples must be listed in awaken-doctest:\n{}",
            missing.join("\n")
        );
    }

    #[test]
    fn compiled_book_examples_do_not_use_hidden_setup_lines() {
        let root = workspace_root();
        let offenders = BOOK_DOCTEST_FILES
            .iter()
            .map(|path| doctest_rel_path(path))
            .flat_map(|rel| {
                hidden_lines_in_compiled_rust_fences(&root, &rel)
                    .into_iter()
                    .map(move |line| format!("{rel}:{line}"))
            })
            .collect::<Vec<_>>();

        assert!(
            offenders.is_empty(),
            "compiled book examples should stay readable and must not use rustdoc hidden setup lines:\n{}",
            offenders.join("\n")
        );
    }
}
