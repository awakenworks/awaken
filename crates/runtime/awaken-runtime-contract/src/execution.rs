use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use thiserror::Error;

use awaken_agent_contract::agent::run::RunState;

use crate::resolved::Backend;

/// Exact Worker capability for the built-in in-process runtime.
pub const NATIVE_RUNTIME_CAPABILITY: &str = "native-runtime";

/// Worker capability for the built-in generic A2A attempt executor.
///
/// The immutable `backend_ref` still carries the exact remote endpoint. Placement
/// admits against this finite capability because one installed A2A transport can
/// execute any valid endpoint; a Worker manifest cannot enumerate future URLs.
pub const A2A_RUNTIME_CAPABILITY: &str = "a2a-runtime";

/// Manifest/placement capability required by an immutable `backend_ref`.
#[must_use]
pub fn execution_capability(backend_ref: &str) -> String {
    match Backend::from_ref(backend_ref) {
        Backend::Native => NATIVE_RUNTIME_CAPABILITY.to_string(),
        Backend::Acp(_) => backend_ref.to_string(),
        Backend::Remote(_) => A2A_RUNTIME_CAPABILITY.to_string(),
        Backend::Invalid(invalid) => format!("invalid-backend-ref:{}", invalid.as_str()),
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("runtime resolution failed: {0}")]
    Resolution(String),
    #[error("runtime execution failed: {0}")]
    Execution(String),
    #[error("runtime commit failed: {0}")]
    Commit(String),
    /// Internal signal consumed by the runtime loop and converted into the
    /// terminal `Failure::StateConflict`; it must not escape `RunExecutor`.
    #[error("runtime state batch conflicts")]
    StateConflict,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Verify the one claim-bound authority immediately before an external
/// execution boundary.
///
/// Direct and embedded callers have no dispatch claim, so an absent verifier
/// preserves that topology. Once ingress supplies an authority, both a lost
/// claim and an unavailable authority fail closed through the ordinary attempt
/// error path.
pub async fn verify_attempt_ownership(
    ownership: Option<&dyn crate::runtime_context::AttemptOwnershipVerifier>,
) -> Result<()> {
    match ownership {
        Some(ownership) => ownership.verify_current().await.map_err(|error| {
            Error::Execution(format!(
                "Run attempt no longer owns external execution: {error}"
            ))
        }),
        None => Ok(()),
    }
}

/// How an executor can be stopped in flight. The host branches on this before it
/// offers cancel/interrupt for a run — the axis is worth typing because it differs
/// across execution altitudes (ADR-0055): the native loop observes a cooperative
/// token at a boundary; an ACP/A2A backend aborts an opaque remote attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cancellation {
    /// The executor cannot be stopped in flight.
    None,
    /// A cooperative cancellation token observed at the next safe boundary (the
    /// native engine).
    CooperativeToken,
    /// The executor aborts an opaque remote/CLI attempt (ACP interrupt / A2A cancel).
    RemoteAbort,
}

/// What an executor can pause a run to wait for (await-and-resume). Kept minimal —
/// only what the host branches on today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// The executor never awaits awaiting for out-of-band input.
    None,
    /// It can await for input (a decision or a steered message).
    Input,
    /// It can await for authorization.
    Auth,
    /// It can await for input or authorization.
    Both,
}

/// The in-flight-control surface an executor supports, so the host adapts rather
/// than assuming the native-engine model for every backend (ADR-0055). Only the
/// axes the host branches on are modeled; more are added when a consumer needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorCapabilities {
    pub cancellation: Cancellation,
    pub wait: Wait,
}

impl ExecutorCapabilities {
    /// The native in-process loop: cooperative-token cancellation and durable
    /// await-and-resume for input or authorization.
    pub const NATIVE: Self = Self {
        cancellation: Cancellation::CooperativeToken,
        wait: Wait::Both,
    };
}

#[async_trait::async_trait]
pub trait RunExecutor: Send + Sync {
    async fn execute(
        &self,
        activation: crate::activation::RunActivation,
        context: crate::runtime_context::RuntimeRunContext,
    ) -> Result<RunState>;

    /// The in-flight-control surface this executor supports. Defaults to the
    /// native-engine model; a backend over an opaque remote/CLI attempt overrides it.
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::NATIVE
    }
}

/// A Run executor that can drive both a fresh activation and a committed resume
/// boundary. Durable dispatch depends on this port instead of assuming every Run
/// is owned by the in-process native `Runtime`; Native, ACP, and future adapters
/// therefore share one claim/fence/settle path.
#[async_trait::async_trait]
pub trait RunAttemptExecutor: RunExecutor {
    /// Resume `activation` using the already-validated, durable identity carried
    /// by `command`. The live reader/commit/cancellation handles remain in
    /// `context`, exactly as for [`RunExecutor::execute`].
    async fn resume(
        &self,
        activation: crate::activation::RunActivation,
        command: crate::resume::ResumeCommand,
        context: crate::runtime_context::RuntimeRunContext,
    ) -> Result<RunState>;

    /// Abort external work retained by this Run, if the executor owns any.
    ///
    /// Durable ingress invokes this after claiming and fencing a cancellation
    /// intent but before it commits the local terminal `Cancelled` fact. Native
    /// and local-only executors have nothing external to release, so the default
    /// is an idempotent no-op. Remote executors recover their opaque execution
    /// reference from committed state in `context` and fail the cancellation when
    /// delivery cannot be confirmed, leaving the durable intent retryable.
    async fn cancel(
        &self,
        _activation: crate::activation::RunActivation,
        _context: crate::runtime_context::RuntimeRunContext,
    ) -> Result<()> {
        Ok(())
    }
}

/// Invalid or conflicting executor registration.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AttemptExecutorRegistryError {
    #[error("backend_ref is not an exact ACP or A2A route: {0}")]
    InvalidBackendRef(String),
    #[error("attempt executor is already registered for {0}")]
    DuplicateBackend(String),
}

/// Exact-match router for immutable publication `backend_ref` values.
///
/// Native provider refs share one in-process executor. ACP CLI ids and A2A
/// endpoints are registered under their complete `acp:*` / `a2a:*` refs, so a
/// nearby but unregistered route fails closed.
#[derive(Clone, Default)]
pub struct AttemptExecutorRegistry {
    native: Option<Arc<dyn RunAttemptExecutor>>,
    exact: BTreeMap<String, Arc<dyn RunAttemptExecutor>>,
}

impl AttemptExecutorRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_native(
        &mut self,
        executor: Arc<dyn RunAttemptExecutor>,
    ) -> std::result::Result<(), AttemptExecutorRegistryError> {
        if self.native.is_some() {
            return Err(AttemptExecutorRegistryError::DuplicateBackend(
                NATIVE_RUNTIME_CAPABILITY.to_string(),
            ));
        }
        self.native = Some(executor);
        Ok(())
    }

    pub fn register(
        &mut self,
        backend_ref: impl Into<String>,
        executor: Arc<dyn RunAttemptExecutor>,
    ) -> std::result::Result<(), AttemptExecutorRegistryError> {
        let backend_ref = backend_ref.into();
        let valid = matches!(
            Backend::from_ref(&backend_ref),
            Backend::Acp(_) | Backend::Remote(_)
        );
        if !valid {
            return Err(AttemptExecutorRegistryError::InvalidBackendRef(backend_ref));
        }
        if self.exact.contains_key(&backend_ref) {
            return Err(AttemptExecutorRegistryError::DuplicateBackend(backend_ref));
        }
        self.exact.insert(backend_ref, executor);
        Ok(())
    }

    /// Capabilities derived from executable registrations, never free-form
    /// declarations.
    #[must_use]
    pub fn manifest_capabilities(&self) -> BTreeSet<String> {
        let mut capabilities = self.exact.keys().cloned().collect::<BTreeSet<_>>();
        if self.native.is_some() {
            capabilities.insert(NATIVE_RUNTIME_CAPABILITY.to_string());
        }
        capabilities
    }

    #[must_use]
    pub fn supports(&self, backend_ref: &str) -> bool {
        match Backend::from_ref(backend_ref) {
            Backend::Native => self.native.is_some(),
            Backend::Acp(_) | Backend::Remote(_) => self.exact.contains_key(backend_ref),
            Backend::Invalid(_) => false,
        }
    }

    fn executor(
        &self,
        activation: &crate::activation::RunActivation,
    ) -> Result<Arc<dyn RunAttemptExecutor>> {
        let backend_ref = &activation.snapshot.resolved_spec.model_binding.backend_ref;
        match Backend::from_ref(backend_ref) {
            Backend::Native => self.native.clone().ok_or_else(|| {
                Error::Resolution(format!(
                    "no native attempt executor is registered for {backend_ref}"
                ))
            }),
            Backend::Acp(_) | Backend::Remote(_) => {
                self.exact.get(backend_ref).cloned().ok_or_else(|| {
                    Error::Resolution(format!(
                        "no attempt executor is registered for exact backend_ref {backend_ref}"
                    ))
                })
            }
            Backend::Invalid(invalid) => Err(Error::Resolution(format!(
                "invalid backend_ref {}",
                invalid.as_str()
            ))),
        }
    }
}

#[async_trait::async_trait]
impl RunExecutor for AttemptExecutorRegistry {
    async fn execute(
        &self,
        activation: crate::activation::RunActivation,
        context: crate::runtime_context::RuntimeRunContext,
    ) -> Result<RunState> {
        self.executor(&activation)?
            .execute(activation, context)
            .await
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for AttemptExecutorRegistry {
    async fn resume(
        &self,
        activation: crate::activation::RunActivation,
        command: crate::resume::ResumeCommand,
        context: crate::runtime_context::RuntimeRunContext,
    ) -> Result<RunState> {
        self.executor(&activation)?
            .resume(activation, command, context)
            .await
    }

    async fn cancel(
        &self,
        activation: crate::activation::RunActivation,
        context: crate::runtime_context::RuntimeRunContext,
    ) -> Result<()> {
        self.executor(&activation)?
            .cancel(activation, context)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use crate::snapshot::{AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId};
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
    use awaken_agent_contract::agent::thread::Id as ThreadId;

    struct FixedOwnership(std::result::Result<(), crate::runtime_context::AttemptOwnershipError>);

    #[async_trait::async_trait]
    impl crate::runtime_context::AttemptOwnershipVerifier for FixedOwnership {
        async fn verify_current(
            &self,
        ) -> std::result::Result<(), crate::runtime_context::AttemptOwnershipError> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn external_execution_ownership_is_optional_but_fail_closed_when_bound() {
        // Cause/effect graph: C1=dispatch authority is absent, current, lost, or
        // unavailable. E1=admit the external execution boundary; E2=return an
        // attempt error before that boundary. Constraint: absence is valid only
        // for direct/embedded execution; a bound authority is never bypassed.
        //
        // | Rule | C1          | Effect |
        // | O1   | absent      | E1     |
        // | O2   | current     | E1     |
        // | O3   | lost        | E2     |
        // | O4   | unavailable | E2     |
        verify_attempt_ownership(None).await.expect("O1/E1");
        let current = FixedOwnership(Ok(()));
        verify_attempt_ownership(Some(&current))
            .await
            .expect("O2/E1");

        let lost = FixedOwnership(Err(crate::runtime_context::AttemptOwnershipError::Lost));
        assert!(
            verify_attempt_ownership(Some(&lost)).await.is_err(),
            "O3/E2"
        );
        let unavailable = FixedOwnership(Err(
            crate::runtime_context::AttemptOwnershipError::Unavailable("authority down".into()),
        ));
        assert!(
            verify_attempt_ownership(Some(&unavailable)).await.is_err(),
            "O4/E2"
        );
    }

    struct DefaultExecutor;

    #[async_trait::async_trait]
    impl RunExecutor for DefaultExecutor {
        async fn execute(
            &self,
            _activation: crate::activation::RunActivation,
            _context: crate::runtime_context::RuntimeRunContext,
        ) -> Result<RunState> {
            unreachable!("capabilities-only test")
        }
    }

    #[test]
    fn default_capabilities_are_the_native_model() {
        let caps = DefaultExecutor.capabilities();
        assert_eq!(caps, ExecutorCapabilities::NATIVE);
        assert_eq!(caps.cancellation, Cancellation::CooperativeToken);
        assert_eq!(caps.wait, Wait::Both);
    }

    struct NamedExecutor(&'static str);

    #[async_trait::async_trait]
    impl RunExecutor for NamedExecutor {
        async fn execute(
            &self,
            _activation: crate::activation::RunActivation,
            _context: crate::runtime_context::RuntimeRunContext,
        ) -> Result<RunState> {
            Ok(RunState::Ended(EndCause::Stopped(self.0.to_string())))
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for NamedExecutor {
        async fn resume(
            &self,
            activation: crate::activation::RunActivation,
            _command: crate::resume::ResumeCommand,
            context: crate::runtime_context::RuntimeRunContext,
        ) -> Result<RunState> {
            self.execute(activation, context).await
        }
    }

    fn activation(backend_ref: &str) -> crate::activation::RunActivation {
        crate::activation::RunActivation::new(
            RunId(format!("run-{backend_ref}")),
            ThreadId("thread-registry".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId(format!("snapshot-{backend_ref}")),
                metadata: Default::default(),
                root_agent_id: AgentId("agent-registry".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("catalog-registry".into()),
                    instructions: String::new(),
                    max_steps: 1,
                    delegation_limits: Default::default(),
                    model_binding: crate::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("provider", "model", backend_ref),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("snapshot-registry".into()),
            },
            vec![Message::text(MessageId("input".into()), Role::User, "go")],
        )
    }

    #[tokio::test]
    async fn registry_routes_exact_backends_and_derives_capabilities() {
        let mut registry = AttemptExecutorRegistry::new();
        registry
            .register_native(Arc::new(NamedExecutor("native")))
            .unwrap();
        registry
            .register("acp:claude", Arc::new(NamedExecutor("claude")))
            .unwrap();
        registry
            .register("acp:codex", Arc::new(NamedExecutor("codex")))
            .unwrap();
        registry
            .register(
                "a2a:https://agent.example",
                Arc::new(NamedExecutor("remote")),
            )
            .unwrap();

        assert_eq!(
            registry.manifest_capabilities(),
            BTreeSet::from([
                NATIVE_RUNTIME_CAPABILITY.to_string(),
                "a2a:https://agent.example".to_string(),
                "acp:claude".to_string(),
                "acp:codex".to_string(),
            ])
        );
        for (backend_ref, expected) in [
            ("genai", "native"),
            ("acp:claude", "claude"),
            ("acp:codex", "codex"),
            ("a2a:https://agent.example", "remote"),
        ] {
            assert_eq!(
                registry
                    .execute(
                        activation(backend_ref),
                        crate::runtime_context::RuntimeRunContext::new(),
                    )
                    .await
                    .unwrap(),
                RunState::Ended(EndCause::Stopped(expected.to_string()))
            );
        }
        assert!(
            registry
                .execute(
                    activation("acp:gemini"),
                    crate::runtime_context::RuntimeRunContext::new(),
                )
                .await
                .is_err(),
            "an unregistered neighboring CLI fails closed"
        );
    }

    #[test]
    fn placement_capability_matches_the_installed_executor_altitude() {
        assert_eq!(execution_capability("genai"), NATIVE_RUNTIME_CAPABILITY);
        assert_eq!(execution_capability("acp:claude"), "acp:claude");
        assert_eq!(
            execution_capability("a2a:https://agent.example"),
            A2A_RUNTIME_CAPABILITY
        );
    }

    #[test]
    fn registry_rejects_ambiguous_or_duplicate_registration() {
        let mut registry = AttemptExecutorRegistry::new();
        assert!(matches!(
            registry.register("genai", Arc::new(NamedExecutor("wrong"))),
            Err(AttemptExecutorRegistryError::InvalidBackendRef(_))
        ));
        assert!(matches!(
            registry.register("a2a:", Arc::new(NamedExecutor("empty"))),
            Err(AttemptExecutorRegistryError::InvalidBackendRef(_))
        ));
        registry
            .register("acp:claude", Arc::new(NamedExecutor("first")))
            .unwrap();
        assert!(matches!(
            registry.register("acp:claude", Arc::new(NamedExecutor("second"))),
            Err(AttemptExecutorRegistryError::DuplicateBackend(_))
        ));
    }
}
