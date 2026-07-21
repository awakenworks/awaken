//! Real-model e2e for the auxiliary agents (memory extraction, context
//! compaction) driven by [`SharedHost`] over a **live** Anthropic-compatible
//! endpoint (e.g. Kimi) through the genai provider. These prove the real effect:
//! a live model, triggered after a turn, actually writes a memory file / produces
//! a summary that keeps a long conversation going.
//!
//! Gated with `#[ignore]` — needs network + credentials. Run explicitly:
//!
//! ```sh
//! KIMI_API_KEY=sk-... cargo test -p awaken-server --test real_model_aux -- --ignored --nocapture
//! ```
//!
//! Env: `KIMI_API_KEY` (required), `KIMI_BASE_URL` (default the Kimi coding
//! endpoint), `KIMI_MODEL` (default `kimi-for-coding`).

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::RunState;
use awaken_protocol_managed::resource_plane::{
    ConfigVersion, MemoryStoreConfigVersion, ResourceAccess, ResourceBindingValidator,
    ResourceCatalogError,
};
use awaken_provider_genai::GenaiExecutor;
use awaken_server::SharedHost;

fn live_host() -> Option<(SharedHost, String)> {
    let key = std::env::var("KIMI_API_KEY").ok()?;
    let base = std::env::var("KIMI_BASE_URL")
        .unwrap_or_else(|_| "https://api.kimi.com/coding/v1/".to_string());
    let model = std::env::var("KIMI_MODEL").unwrap_or_else(|_| "kimi-for-coding".to_string());
    let executor = GenaiExecutor::anthropic_compatible(base, key);
    Some((SharedHost::new(Arc::new(executor), model.clone()), model))
}

fn user(text: &str) -> Vec<Message> {
    vec![Message::text(MessageId("u".into()), Role::User, text)]
}

fn bind_memory(host: &SharedHost, thread: &str, store: &str) {
    struct ActiveMemory;
    impl ResourceBindingValidator for ActiveMemory {
        fn validate_memory_binding(
            &self,
            _workspace_id: &str,
            _id: &str,
            _version: ConfigVersion,
        ) -> Result<(), ResourceCatalogError> {
            Ok(())
        }

        fn validate_repository_binding(
            &self,
            _workspace_id: &str,
            _id: &str,
            _version: ConfigVersion,
        ) -> Result<(), ResourceCatalogError> {
            Ok(())
        }
    }
    host.bind_resolved_memory(
        thread,
        "default",
        &MemoryStoreConfigVersion {
            memory_store_id: store.into(),
            version: ConfigVersion::INITIAL,
            recall_policy: Default::default(),
            extraction_policy: Default::default(),
            retention_policy: Default::default(),
        },
        ResourceAccess::ReadWrite,
        Arc::new(ActiveMemory),
    );
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn live_memory_extraction_writes_a_memory_file() {
    let (host, _model) = live_host().expect("set KIMI_API_KEY to run this test");
    let store = "live-memory-extraction";
    bind_memory(&host, "mem-e2e", store);

    // A turn stating a clear, durable preference the extractor should save.
    let state = host
        .run(None, "mem-e2e",
            user("Please remember this for the future: my name is Ada, and I strongly prefer Rust over Python for all backend work. Reply with a brief acknowledgement."),
        )
        .await
        .expect("main turn");
    assert!(
        matches!(state.state, RunState::Ended(_)),
        "main turn should end"
    );

    assert!(
        host.drain_memory(Duration::from_secs(90)).await,
        "memory extraction should finish"
    );

    let files = host.memory_repository().list(store, "/").await.unwrap();
    assert!(
        !files.is_empty(),
        "the extractor should have written at least one governed memory"
    );
    let mut combined = String::new();
    for file in &files {
        let memory = host
            .memory_repository()
            .get_by_path(store, &file.path)
            .await
            .unwrap()
            .unwrap();
        combined.push_str(memory.content.as_deref().unwrap_or_default());
        combined.push('\n');
    }
    eprintln!("live memory files: {files:?}\n---\n{combined}\n---");
    assert!(
        !combined.trim().is_empty(),
        "memory files should have content"
    );
    let lower = combined.to_lowercase();
    assert!(
        lower.contains("rust") || lower.contains("ada"),
        "a saved memory should mention the stated preference/name; got: {combined}"
    );
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn live_memory_is_generated_then_recalled_and_used_in_a_new_conversation() {
    let (host, _model) = live_host().expect("set KIMI_API_KEY to run this test");
    let store = "live-memory-loop";
    bind_memory(&host, "conv-1", store);
    bind_memory(&host, "conv-2", store);

    // Conversation 1: the user states a durable preference; extraction saves it.
    host.run(None, "conv-1",
        user("Remember for the future: my favorite programming language is Rust. Acknowledge briefly."),
    )
    .await
    .expect("conversation 1 turn");
    assert!(
        host.drain_memory(Duration::from_secs(90)).await,
        "memory extraction should finish"
    );
    let files = host.memory_repository().list(store, "/").await.unwrap();
    assert!(!files.is_empty(), "a memory should have been generated");

    // Conversation 2 (a fresh thread, no shared transcript): the saved memory is
    // recalled into context and the live model uses it to answer.
    let state = host
        .run(None, "conv-2",
            user("Based on what you remember about me, what is my favorite programming language? Answer with just the language name."),
        )
        .await
        .expect("conversation 2 turn");
    let reply = state
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.text_content())
        .unwrap_or_default();
    eprintln!("recall-based reply: {reply:?}");
    assert!(
        reply.to_lowercase().contains("rust"),
        "the fresh conversation should recall and use the saved memory; got: {reply:?}"
    );
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn live_relevance_selection_picks_the_right_memory_via_the_selector_agent() {
    let (host, _model) = live_host().expect("set KIMI_API_KEY to run this test");
    let store = "live-memory-selection";

    // Pre-populate MORE than the recall `select_over` threshold (12) so the recall
    // hook uses the relevance selector (a `memory-selector` sub-agent), not the
    // whole-store bounded path. 12 unrelated memories + 1 answer.
    let noise = [
        "the user's favorite color is blue",
        "the user works in Berlin",
        "the user prefers tea over coffee",
        "the user drives a red bicycle",
        "the user's favorite language is Rust",
        "the user reads science fiction",
        "the user wakes up at 6am",
        "the user likes hiking on weekends",
        "the user's office is on the third floor",
        "the user plays the guitar",
        "the user was born in spring",
        "the user enjoys cooking pasta",
    ];
    for (i, n) in noise.iter().enumerate() {
        host.memory_repository()
            .create(store, &format!("/noise-{i}.md"), n)
            .await
            .unwrap();
    }
    host.memory_repository()
        .create(store, "/pet.md", "the user's dog is named Rex")
        .await
        .unwrap();
    bind_memory(&host, "select-e2e", store);

    // A pointed question: the selector sub-agent must pick the dog memory out of 13,
    // and the main model must answer from it.
    let state = host
        .run(None, "select-e2e",
            user("Based on what you remember about me, what is my dog's name? Answer with just the name."),
        )
        .await
        .expect("turn");
    let reply = state
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.text_content())
        .unwrap_or_default();
    eprintln!("selector-based reply: {reply:?}");
    assert!(
        reply.to_lowercase().contains("rex"),
        "the selector should surface the dog memory and the model use it; got: {reply:?}"
    );
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn live_judge_grades_a_deliverable_as_a_configurable_agent() {
    let (host, _model) = live_host().expect("set KIMI_API_KEY to run this test");
    // The judge is now an ordinary agent resolved by id; grade with a real one.
    let host = host.with_judge("judge");

    let report = host
        .define_outcome(
            "goal-e2e",
            "Reply with exactly the single word BANANA and nothing else.",
            "the reply contains the word BANANA",
            3,
        )
        .await
        .expect("define_outcome");

    let last = report.iterations.last().expect("at least one round");
    eprintln!(
        "judge rounds: {:?}",
        report
            .iterations
            .iter()
            .map(|i| (i.result.clone(), i.explanation.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        last.result, "satisfied",
        "the live judge should mark the BANANA deliverable satisfied"
    );
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn live_compaction_summarizes_and_the_conversation_continues() {
    let (host, _model) = live_host().expect("set KIMI_API_KEY to run this test");
    // Compact once history passes 4 messages, keeping the last 2 verbatim.
    let host = host.with_compaction(4, 2);

    // Several short turns to build history past the threshold.
    for i in 0..3 {
        let state = host
            .run(
                None,
                "compact-e2e",
                user(&format!(
                    "Fact number {i}: item {i} is important. Acknowledge in a few words."
                )),
            )
            .await
            .expect("turn");
        assert!(matches!(state.state, RunState::Ended(_)));
    }

    // A follow-up turn still completes: the compact plugin summarized the older slice
    // inline (BeforeInference) and the windowed context is coherent for the live model.
    let state = host
        .run(
            None,
            "compact-e2e",
            user("Briefly, how many facts have I told you so far?"),
        )
        .await
        .expect("follow-up turn");
    assert!(
        matches!(state.state, RunState::Ended(_)),
        "the post-compaction turn should complete"
    );
    let reply = state
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.text_content())
        .unwrap_or_default();
    eprintln!("post-compaction reply: {reply:?}");
    assert!(
        !reply.trim().is_empty(),
        "the follow-up reply should be non-empty"
    );
}
