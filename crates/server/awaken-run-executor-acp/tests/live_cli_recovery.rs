//! Strict real-adapter/real-model instruction and recovery acceptance.
//! Run with ACP_LIVE_CLI, ACP_LIVE_BASE_URL, ACP_LIVE_MODEL and
//! ACP_LIVE_API_KEY_FILE. Artifact adapters also require an already encoded
//! ACP_LIVE_ARTIFACT_FILE. No ambient key or operator HOME is inherited.
//! Three fresh processes share one committed Thread; catalog-local-dir adapters
//! move across config homes through the production DirSessionHome provider.
//! This qualifies the executor/vendor boundary, not distributed placement.
#![cfg(feature = "real-acp")]

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::{
    AcpCli, AcpLaunchEvent, AcpLaunchStage, AcpRunExecutor, AgentChannelSource, AgentSession,
    ChannelOpenDoor, Codec, ConfigHome, CredentialArtifactRequirement, DirSessionHome,
    FsSessionBlobStore, LaunchObserver, LaunchResolver, OpenError, ProcessSecretRequirement,
    ResolvedModel, SessionPersistence, SubprocessChannelSource, acp_cli, open_agent_session,
    project_launch,
};
use awaken_runtime_contract::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_session_configuration_contract::{SessionContextSeed, SessionTrigger};
use awaken_store_inmem::MemoryCommitCoordinator;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const THREAD: &str = "thread-live";
const POLICY: &str = "AWAKEN_LIVE_SYSTEM_POLICY_7391";

struct FileBroker;
#[async_trait]
impl pc::SecretBroker for FileBroker {
    async fn materialize_process(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.materialize(reference).await
    }
    async fn write_back(
        &self,
        _: pc::SecretWritebackTarget<'_>,
        _: Vec<u8>,
    ) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "live API-key fixtures have no refresh writeback",
        ))
    }
    async fn materialize(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        let name = match reference {
            "live-secret" => "ACP_LIVE_API_KEY_FILE",
            "live-artifact" => "ACP_LIVE_ARTIFACT_FILE",
            _ => {
                return Err(pc::SandboxError::unsupported(
                    "unknown live credential reference",
                ));
            }
        };
        let path = std::env::var(name)
            .map_err(|_| pc::SandboxError::unsupported("live credential file is required"))?;
        std::fs::read(path)
            .map_err(|_| pc::SandboxError::unsupported("live credential file is unreadable"))
    }
}

struct LiveResolver {
    cli: AcpCli,
    home: ConfigHome,
}
#[async_trait]
impl LaunchResolver for LiveResolver {
    async fn model(
        &self,
        _: &RunActivation,
        _: &RuntimeRunContext,
    ) -> Result<ResolvedModel, OpenError> {
        let artifact = self
            .cli
            .managed_credential_delivery
            .credential_artifact(false)
            .map(|spec| CredentialArtifactRequirement::new("live-artifact", spec.relative_path));
        let secret = artifact
            .is_none()
            .then(|| ProcessSecretRequirement::new("live-secret"));
        Ok(ResolvedModel::managed(
            std::env::var("ACP_LIVE_BASE_URL").expect("explicit live endpoint"),
            std::env::var("ACP_LIVE_MODEL").expect("explicit live model"),
            secret,
            artifact,
        ))
    }
    fn extra_env(&self, _: &RunActivation) -> Result<Vec<(String, String)>, OpenError> {
        let home = self.home.root().to_string_lossy().into_owned();
        let mut env = vec![
            ("HOME".into(), home.clone()),
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
                "1".into(),
            ),
        ];
        if let Some(name) = self.cli.config_home_env {
            env.push((name.into(), home.clone()));
        }
        env.extend(
            self.cli
                .config_home_aliases
                .iter()
                .map(|name| ((*name).into(), home.clone())),
        );
        Ok(env)
    }
}

struct LiveSource {
    resolver: LiveResolver,
    workspace: PathBuf,
    replace: bool,
}
#[async_trait]
impl AgentChannelSource for LiveSource {
    fn check_tool_narrowing(
        &self,
        narrowing: &awaken_runtime_contract::permission::ToolCapabilityNarrowing,
    ) -> std::result::Result<(), awaken_run_executor_acp::OpenError> {
        awaken_run_executor_acp::opaque_tool_narrowing(narrowing)
    }

    async fn open(
        &self,
        activation: &RunActivation,
        context: &RuntimeRunContext,
        _: ChannelOpenDoor,
    ) -> Result<AgentSession, OpenError> {
        let cli = &self.resolver.cli;
        let mut resolved = project_launch(cli, &self.resolver, activation, context, None).await?;
        // Replacement exercises Claude's verified metadata extension with no
        // tool work. The main acceptance rows retain their real builtin tools.
        if self.replace {
            resolved
                .launch
                .tool_aliases
                .insert("Bash".into(), "mcp__awaken_session__bash".into());
            resolved.launch.project_instructions(
                cli.instructions,
                &activation.snapshot.resolved_spec.instructions,
            )?;
        }
        if let Some(file) = resolved
            .launch
            .bind_instructions_file(&self.resolver.home.root().to_string_lossy())?
        {
            self.resolver
                .home
                .write(&file.relative_path, file.contents.as_bytes())
                .map_err(|e| OpenError(e.to_string()))?;
        }
        if let Some(artifact) = resolved.credential_artifact {
            let bytes = pc::SecretBroker::materialize(&FileBroker, artifact.reference())
                .await
                .map_err(|e| OpenError(e.to_string()))?;
            self.resolver
                .home
                .write(artifact.relative_path(), &bytes)
                .map_err(|e| OpenError(e.to_string()))?;
        }
        let source = SubprocessChannelSource::new(resolved.launch)
            .with_codec(Codec::Acp)
            .with_secret_broker(Arc::new(FileBroker));
        let mut session = open_agent_session(&source, activation, context).await?;
        session.workspace_cwd = Some(self.workspace.to_string_lossy().into_owned());
        Ok(session)
    }
}

#[derive(Default)]
struct Observer(Mutex<Vec<AcpLaunchEvent>>);
impl LaunchObserver for Observer {
    fn on_launch(&self, _: &str, event: &AcpLaunchEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

#[tokio::test]
#[ignore = "real ACP adapter and real provider credential; explicit live lane only"]
async fn real_instructions_context_and_committed_session_survive_relaunch() {
    // Cause graph: C1 catalog adapter+real model; C2 append/Claude replace;
    // C3 committed external Session id; C4 fresh process; C5 catalog-local-dir
    // move; C6 request-only date/timezone. Effects: E1 hidden policy reaches
    // inference; E2 exact calendar/source output; E3 memory survives load; E4
    // real task writes exact file bytes (append); E5 no context commit.
    // L1 fresh=>E1+E2; L2 C3+C4(+C5 when supported)=>E1+E2+E3;
    // L3 append+explicit filesystem task=>E4+real tool facts; L4 all=>E5.
    // Missing credentials fail this explicit lane; ignore is never a PASS.
    let id = std::env::var("ACP_LIVE_CLI").expect("ACP_LIVE_CLI is required");
    let cli = *acp_cli(&id).expect("known ACP adapter");
    let replace = std::env::var("ACP_LIVE_REPLACE").as_deref() == Ok("1");
    assert!(
        !replace || cli.supports_tool_aliases,
        "replace only with certified capability"
    );
    let model = std::env::var("ACP_LIVE_MODEL").expect("live model");
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let blobs = Arc::new(FsSessionBlobStore::new(root.path().join("blobs")));
    let clock = awaken_time::system_clock();
    let commits = Arc::new(MemoryCommitCoordinator::new(clock.clone()));
    let observer = Arc::new(Observer::default());
    let seed = SessionContextSeed::from_metadata(
        SessionTrigger::Interactive {},
        &std::collections::BTreeMap::from([("awaken.timezone".into(), "Asia/Singapore".into())]),
        None,
    )
    .unwrap();
    let code = format!("BANANA_LIVE_{}", std::process::id());
    let movable = matches!(cli.session_persistence, SessionPersistence::LocalDir { .. });
    for phase in 0..3 {
        let store = root.path().join(if movable {
            format!("store-{phase}")
        } else {
            "store-0".into()
        });
        let home = ConfigHome::open(Some(&store), THREAD).unwrap();
        let source = LiveSource {
            resolver: LiveResolver { cli, home },
            workspace: workspace.clone(),
            replace,
        };
        let output = workspace.join(format!("acceptance-{phase}.txt"));
        let memory = if phase == 0 {
            format!("Remember this code for all later requests: {code}.")
        } else {
            "Recall the code from our earlier conversation; it is not supplied in this request."
                .into()
        };
        let task = if replace {
            format!("{memory} Report the required JSON. Use no tools.")
        } else {
            format!(
                "{memory} Use your filesystem tools to write ONLY that exact code, without newline, to {}. Then report the required JSON.",
                output.display()
            )
        };
        let instructions = format!(
            "Published Agent policy: {POLICY}. Respond with exactly one compact JSON object (no fences or prose) containing policy (the exact policy tag), current_date and timezone (from authoritative Awaken environment context), trigger_source, and remembered_code. Follow the explicit task, do not substitute your CLI process date. Filesystem changes are allowed only at the exact path explicitly requested by the task."
        );
        let activation = RunActivation::new(
            RunId(format!("run-live-{phase}")),
            ThreadId(THREAD.into()),
            ExecutableAgentSnapshot::builder("live-agent")
                .model(ModelBinding::parse("live", &model, format!("acp:{id}")).unwrap())
                .instructions(instructions)
                .max_steps(8)
                .build(),
            vec![Message::text(
                MessageId(format!("user-live-{phase}")),
                Role::User,
                task,
            )],
        );
        let env = seed.render_at(clock.now().as_epoch_ms()).unwrap();
        let date = env
            .lines()
            .find_map(|line| line.strip_prefix("current_date="))
            .unwrap()
            .to_owned();
        let mut context = RuntimeRunContext::new()
            .with_commit(commits.clone())
            .with_reader(commits.clone());
        context.request_timezone = Some(seed.timezone().into());
        context.request_context = vec![Message::text(
            MessageId(format!("live-env-{phase}")),
            Role::System,
            env,
        )];
        let before = commits.committed().messages.len();
        let state = AcpRunExecutor::new(Arc::new(source))
            .with_launch_observer(observer.clone())
            .with_session_home(Arc::new(DirSessionHome::new(Some(store), blobs.clone())))
            .execute(activation, context)
            .await
            .unwrap();
        assert_eq!(
            state,
            RunState::Ended(EndCause::NaturalEnd),
            "real adapter reaches natural terminal outcome"
        );
        let committed = commits.committed();
        let delta = &committed.messages[before..];
        let text = delta
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && !m.text_content().trim().is_empty())
            .expect("final assistant message")
            .text_content();
        let report: serde_json::Value =
            serde_json::from_str(text.trim()).expect("strict JSON report");
        for (field, expected) in [
            ("policy", POLICY),
            ("current_date", date.as_str()),
            ("timezone", "Asia/Singapore"),
            ("trigger_source", "interactive"),
            ("remembered_code", code.as_str()),
        ] {
            assert_eq!(
                report[field].as_str(),
                Some(expected),
                "real semantic output {id}/{phase}/{field}: {text}"
            );
        }
        assert!(
            delta.iter().all(|m| !m.id.0.starts_with("live-env-")
                && !m.text_content().contains("Awaken environment context (")),
            "request context never becomes Thread truth"
        );
        if !replace {
            assert_eq!(
                std::fs::read_to_string(&output).unwrap(),
                code,
                "actual file side effect"
            );
            assert!(
                delta
                    .iter()
                    .flat_map(|m| &m.content)
                    .any(|block| matches!(block, ContentBlock::ToolUse { .. })),
                "actual tool fact"
            );
        }
        println!(
            "PASS real {id} phase={phase} replace={replace} moved_home={movable} policy/date/source/memory{}",
            if replace { "" } else { "/file/tool" }
        );
    }
    let events = observer.0.lock().unwrap();
    assert!(
        events
            .iter()
            .filter(|event| event.detail.as_deref() == Some("session/load"))
            .count()
            >= 2,
        "fresh processes load the committed external Session id"
    );
    assert!(
        events
            .iter()
            .filter(|event| event.stage == AcpLaunchStage::Ready)
            .all(
                |event| event.detail.as_deref().unwrap_or("").contains(if replace {
                    "instructions=system_replace"
                } else {
                    "instructions=system_append"
                })
            ),
        "no hidden first-prompt fallback"
    );
}
