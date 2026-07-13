//! Gated real-CLI cross-directory session-recovery e2e (no stubs — drives the real
//! adapter binary). `#[ignore]` by default: it needs a real ACP CLI reachable
//! (`npx @agentclientprotocol/claude-agent-acp`, `codex-acp`, or `gemini
//! --experimental-acp`) **and** provider credentials, so it self-skips unless
//! `ACP_LIVE_CLI` is set to a known adapter and the model env is present.
//!
//! It proves the whole recovery chain end to end: run 1 in config-home **A** tells
//! the CLI a code and the executor harvests A's session subtree; run 2 in a
//! **different** config-home **B** restores it before launch, and the same CLI —
//! resuming via `session/load` — recalls the code. A different directory (or, with a
//! shared blob store, a different machine) recovers the session.
//!
//! Run one runtime, e.g. Claude:
//!   ACP_LIVE_CLI=claude ANTHROPIC_BASE_URL=… ANTHROPIC_API_KEY=… \
//!     cargo test -p awaken-run-executor-acp --features real-acp \
//!       --test live_cli_recovery -- --ignored --nocapture
//!
//! NOTE: not executed in CI or in the authoring environment (no matching
//! CLI+credentials present); it is the runnable spec for when they are.
#![cfg(feature = "real-acp")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::commit::staged::{CommitRecord, ThreadCommit};
use awaken_run_executor_acp::{
    AcpLaunch, AcpRunExecutor, Codec, SessionHomeKey, SessionHomePlan, SessionHomeProvider,
    SessionPersistence, SubprocessChannelSource, acp_cli,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

/// Captures the committed assistant text of one run.
#[derive(Default)]
struct RecordingCoordinator {
    commits: Mutex<Vec<ThreadCommit>>,
}

#[async_trait]
impl Coordinator for RecordingCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        self.commits.lock().unwrap().push(commit);
        Ok(CommitRecord { sequence: 1 })
    }
}

/// The essence of `DirSessionHome` for the test: harvest the config-home's session
/// subtree to a blob keyed by thread, restore it back — the same shape the host's
/// real provider uses, so the CLI's own session files move between config homes.
struct TmpHome {
    blobs: PathBuf,
    config_home: PathBuf,
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), to).unwrap();
        }
    }
}

#[async_trait]
impl SessionHomeProvider for TmpHome {
    async fn restore(&self, key: &SessionHomeKey, plan: &SessionHomePlan) {
        let blob = self.blobs.join(&key.thread_id);
        if blob.is_dir() {
            copy_dir(&blob, &self.config_home.join(&plan.session_subpath));
        }
    }
    async fn harvest(&self, key: &SessionHomeKey, plan: &SessionHomePlan) {
        let src = self.config_home.join(&plan.session_subpath);
        if src.is_dir() {
            let dst = self.blobs.join(&key.thread_id);
            let _ = std::fs::remove_dir_all(&dst);
            copy_dir(&src, &dst);
        }
    }
}

fn activation(cli: &str, prompt: &str) -> RunActivation {
    RunActivation {
        run_id: RunId("run-live".into()),
        thread_id: ThreadId("thread-live".into()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: "be helpful".into(),
                max_steps: 8,
                model_binding: ModelBinding::new("prov", "model", format!("acp:{cli}")),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        },
        input: vec![Message::text(MessageId("u1".into()), Role::User, prompt)],
        trace: Default::default(),
    }
}

/// Build a real launch for `cli`: its catalog argv + the config-home env pointing
/// at `config_home`, plus any model-delivery env keys present in the process env
/// (the operator supplies real base-url/model/key values).
fn live_launch(cli: &awaken_run_executor_acp::AcpCli, config_home: &Path) -> AcpLaunch {
    let mut argv = vec![cli.command.to_string()];
    argv.extend(cli.args.iter().map(|a| (*a).to_string()));
    let mut env = vec![(
        cli.config_home_env.to_string(),
        config_home.display().to_string(),
    )];
    for key in [
        cli.model_delivery.base_url,
        cli.model_delivery.model,
        cli.model_delivery.key,
    ] {
        if let Ok(val) = std::env::var(key) {
            env.push((key.to_string(), val));
        }
    }
    AcpLaunch::custom(argv, env)
}

async fn run_once(cli_id: &str, config_home: &Path, blobs: &Path, prompt: &str) -> Vec<String> {
    let cli = acp_cli(cli_id).expect("known cli");
    let source = SubprocessChannelSource::new(live_launch(cli, config_home)).with_codec(Codec::Acp);
    let home = Arc::new(TmpHome {
        blobs: blobs.to_path_buf(),
        config_home: config_home.to_path_buf(),
    });
    let coord = Arc::new(RecordingCoordinator::default());
    AcpRunExecutor::new(Arc::new(source))
        .with_session_home(home)
        .execute(
            activation(cli_id, prompt),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .expect("the run completes");
    let commits = coord.commits.lock().unwrap();
    commits
        .iter()
        .flat_map(|c| c.messages.iter())
        .map(|m| m.text_content())
        .collect()
}

#[tokio::test]
#[ignore = "needs a real ACP CLI + provider creds; set ACP_LIVE_CLI and the model env"]
async fn a_real_cli_session_recovers_across_directories() {
    let Ok(cli_id) = std::env::var("ACP_LIVE_CLI") else {
        eprintln!("SKIP: set ACP_LIVE_CLI=claude|codex|gemini + the model env");
        return;
    };
    let cli = acp_cli(&cli_id).expect("ACP_LIVE_CLI must name a known adapter");
    // A gateway/stateless adapter has no local session to recover — not this test.
    assert!(
        matches!(cli.session_persistence, SessionPersistence::LocalDir { .. }),
        "{cli_id} is not a local-dir session adapter"
    );

    let root = std::env::temp_dir().join(format!("acp-live-recovery-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (dir_a, dir_b, blobs) = (root.join("a"), root.join("b"), root.join("blobs"));
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    // Run 1 in config-home A: tell the CLI a code; the executor harvests A's session.
    let _ = run_once(
        &cli_id,
        &dir_a,
        &blobs,
        "Remember this code for later: BANANA-42. Reply only: ok.",
    )
    .await;

    // Run 2 in a *different* config-home B: the executor restores the harvested
    // session first, so the CLI resumes via session/load and recalls the code.
    let texts = run_once(
        &cli_id,
        &dir_b,
        &blobs,
        "What was the code I told you to remember? Reply with only the code.",
    )
    .await;

    assert!(
        texts.iter().any(|t| t.contains("BANANA-42")),
        "the session recovered across the directory change: {texts:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
