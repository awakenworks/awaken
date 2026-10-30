//! Real-model e2e for the auxiliary agents (memory extraction, context
//! compaction) driven by [`ExecutorComponents`] over a **live** Anthropic-compatible
//! endpoint (e.g. Kimi) through the genai provider. These prove the real effect:
//! a live model, triggered after a Run, actually writes a memory file / produces
//! a summary that keeps a long conversation going.
//!
//! Gated with `#[ignore]` — needs network + credentials. Run explicitly:
//!
//! ```sh
//! KIMI_API_KEY=sk-... cargo test -p awaken-coordinator --test real_model_aux -- --ignored --nocapture
//! ```
//!
//! Env: `KIMI_API_KEY` (required), `KIMI_BASE_URL` (default the Kimi coding
//! endpoint), `KIMI_MODEL` (default `kimi-for-coding`).

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::RunState;
use awaken_provider_genai::GenaiExecutor;
use awaken_resource_contract::{
    ConfigVersion, LiveResourceBindingVerifier, MemoryStoreConfigVersion, ResourceAccess,
    ResourceRegistryError,
};
use awaken_session_executor::ExecutorComponents;
use awaken_session_executor::HostOutcomeDrive;

async fn live_platform() -> Option<(
    awaken_session_executor::SessionExecutorBuilder,
    awaken_resource_application::ResourcesApplication,
    String,
)> {
    let key = std::env::var("KIMI_API_KEY").ok()?;
    let base = std::env::var("KIMI_BASE_URL")
        .unwrap_or_else(|_| "https://api.kimi.com/coding/v1/".to_string());
    let model = std::env::var("KIMI_MODEL").unwrap_or_else(|_| "kimi-for-coding".to_string());
    let executor = GenaiExecutor::from_materialized_endpoint(
        awaken_provider_genai::AdapterKind::Anthropic,
        base,
        key,
    )
    .expect("explicit client construction");
    let (dependencies, resources) =
        awaken_scenario_host::build_unmounted_platform(Arc::new(executor), model.clone()).await;
    Some((dependencies, resources, model))
}

async fn live_dependencies() -> Option<(awaken_session_executor::SessionExecutorBuilder, String)> {
    live_platform()
        .await
        .map(|(dependencies, _resources, model)| (dependencies, model))
}

async fn live_host() -> Option<(Arc<ExecutorComponents>, String)> {
    live_dependencies().await.map(|(dependencies, model)| {
        (
            (dependencies).build().expect("validated Host dependencies"),
            model,
        )
    })
}

fn user(text: &str) -> Vec<Message> {
    vec![Message::text(MessageId("u".into()), Role::User, text)]
}

fn bind_memory(host: &ExecutorComponents, thread: &str, store: &str) {
    struct ActiveMemory;
    #[async_trait::async_trait]
    impl LiveResourceBindingVerifier for ActiveMemory {
        async fn verify_memory_binding(
            &self,
            _workspace_id: &str,
            _id: &str,
            _version: ConfigVersion,
        ) -> Result<(), ResourceRegistryError> {
            Ok(())
        }

        async fn verify_repository_binding(
            &self,
            _workspace_id: &str,
            _id: &str,
            _version: ConfigVersion,
        ) -> Result<(), ResourceRegistryError> {
            Ok(())
        }
    }
    host.run_assembly()
        .bind_resolved_memory(
            thread,
            "default",
            &MemoryStoreConfigVersion {
                memory_store_id: store.into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
            ResourceAccess::ReadWrite,
            Arc::new(ActiveMemory),
        )
        .expect("healthy registry");
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn live_memory_extraction_writes_a_memory_file() {
    // Causes: the fixtures below establish `live memory extraction` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `writes a memory file` and every asserted state transition or
    // side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Coverage rationale: `live memory extraction` is one independent branch selecting `writes a
    // memory file`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let (host, _model) = live_host()
        .await
        .expect("set KIMI_API_KEY to run this test");
    let store = "live-memory-extraction";
    bind_memory(&host, "mem-e2e", store);

    // A Run stating a clear, durable preference the extractor should save.
    let state = host
        .session_runs().run(None, "mem-e2e",
            user("Please remember this for the future: my name is Ada, and I strongly prefer Rust over Python for all backend work. Reply with a brief acknowledgement."),
        )
        .await
        .expect("main Run");
    assert!(
        matches!(state.state, RunState::Ended(_)),
        "main Run should end"
    );

    assert!(
        awaken_session_executor::drain_session_dispatch(host.as_ref(), Duration::from_secs(90))
            .await,
        "memory extraction should finish"
    );

    let files = awaken_session_executor::test_memory_content(&host)
        .queries()
        .list(store, "/")
        .await
        .unwrap();
    assert!(
        !files.is_empty(),
        "the extractor should have written at least one governed memory"
    );
    let mut combined = String::new();
    for file in &files {
        let memory = awaken_session_executor::test_memory_content(&host)
            .queries()
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
    // Causes: the fixtures below establish `live memory` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `is generated then recalled and used in a new conversation`
    // and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Coverage rationale: `live memory` is one independent branch selecting `is generated then
    // recalled and used in a new conversation`; a multi-row decision table is not applicable, and
    // sibling tests own alternate causes.
    let (host, _model) = live_host()
        .await
        .expect("set KIMI_API_KEY to run this test");
    let store = "live-memory-loop";
    bind_memory(&host, "conv-1", store);
    bind_memory(&host, "conv-2", store);

    // Conversation 1: the user states a durable preference; extraction saves it.
    host.session_runs().run(None, "conv-1",
        user("Remember for the future: my favorite programming language is Rust. Acknowledge briefly."),
    )
    .await
    .expect("conversation 1 Run");
    assert!(
        awaken_session_executor::drain_session_dispatch(host.as_ref(), Duration::from_secs(90))
            .await,
        "memory extraction should finish"
    );
    let files = awaken_session_executor::test_memory_content(&host)
        .queries()
        .list(store, "/")
        .await
        .unwrap();
    assert!(!files.is_empty(), "a memory should have been generated");

    // Conversation 2 (a fresh thread, no shared transcript): the saved memory is
    // recalled into context and the live model uses it to answer.
    let state = host
        .session_runs().run(None, "conv-2",
            user("Based on what you remember about me, what is my favorite programming language? Answer with just the language name."),
        )
        .await
        .expect("conversation 2 Run");
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
    // Causes: the fixtures below establish `live relevance selection picks the right memory via the
    // selector agent` with the concrete inputs, state, dependencies, and failure triggers used by
    // this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Coverage rationale: `live relevance selection picks the right memory via the selector agent`
    // is one independent branch selecting `all output, state, side-effect, error, and terminal
    // assertions below hold together`; a multi-row decision table is not applicable, and sibling
    // tests own alternate causes.
    let (host, _model) = live_host()
        .await
        .expect("set KIMI_API_KEY to run this test");
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
        awaken_session_executor::test_memory_content(&host)
            .writer()
            .create(store, &format!("/noise-{i}.md"), n)
            .await
            .unwrap();
    }
    awaken_session_executor::test_memory_content(&host)
        .writer()
        .create(store, "/pet.md", "the user's dog is named Rex")
        .await
        .unwrap();
    bind_memory(&host, "select-e2e", store);

    // A pointed question: the selector sub-agent must pick the dog memory out of 13,
    // and the main model must answer from it.
    let state = host
        .session_runs().run(None, "select-e2e",
            user("Based on what you remember about me, what is my dog's name? Answer with just the name."),
        )
        .await
        .expect("Run");
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
    // Cause/effect rule J1: one live provider and its exact Resources owner feed
    // canonical local Managed composition; existing profiled creation freezes
    // this Session before a borrowed admission reaches the Outcome controller.
    // Effects: the configured judge, original BANANA Worker prompt/rubric and
    // max=3 still produce the committed satisfied report asserted below.
    // Constraint: no test-local executor may discard prepared activation fields.
    let (dependencies, resources, _model) = live_platform()
        .await
        .expect("set KIMI_API_KEY to run this test");
    // The judge is now an ordinary agent resolved by id; grade with a real one.
    let managed = awaken_scenario_host::local_managed_state(
        dependencies
            .with_judge("judge")
            .expect("live host model reference is an executable binding"),
        resources.authorities().resource_registry(),
    )
    .await;
    let host = managed.host;
    let admissions = managed.state.sessions.run_admissions();
    admissions
        .admit_run_session(host.local_workspace(), "goal-e2e", "assistant")
        .await
        .expect("create the canonical profiled Session");
    let admission = admissions.outcome_run_admission("goal-e2e");
    let progress = host
        .outcomes()
        .define_outcome(
            "goal-e2e",
            "Reply with exactly the single word BANANA and nothing else.",
            "the reply contains the word BANANA",
            3,
            &awaken_session_run_delivery::OutcomeRunExecutor::new(&admission),
        )
        .await
        .expect("define_outcome");
    let HostOutcomeDrive::Completed(report) = progress else {
        panic!("live Outcome unexpectedly awaited external input");
    };

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
    // Causes: the fixtures below establish `live compaction summarizes and the conversation
    // continues` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Coverage rationale: `live compaction summarizes and the conversation continues` is one
    // independent branch selecting `all output, state, side-effect, error, and terminal assertions
    // below hold together`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    let (dependencies, _model) = live_dependencies()
        .await
        .expect("set KIMI_API_KEY to run this test");
    // Compact once history passes 4 messages, keeping the last 2 verbatim.
    let host = (dependencies.with_compaction_tokens(4, 2))
        .build()
        .expect("validated Host dependencies");

    // Several short Runs to build history past the threshold.
    for i in 0..3 {
        let state = host
            .session_runs()
            .run(
                None,
                "compact-e2e",
                user(&format!(
                    "Fact number {i}: item {i} is important. Acknowledge in a few words."
                )),
            )
            .await
            .expect("Run");
        assert!(matches!(state.state, RunState::Ended(_)));
    }

    // A follow-up Run still completes: the compact plugin summarized the older slice
    // inline (BeforeInference) and the windowed context is coherent for the live model.
    let state = host
        .session_runs()
        .run(
            None,
            "compact-e2e",
            user("Briefly, how many facts have I told you so far?"),
        )
        .await
        .expect("follow-up Run");
    assert!(
        matches!(state.state, RunState::Ended(_)),
        "the post-compaction Run should complete"
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
