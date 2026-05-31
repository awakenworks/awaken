use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use awaken_ext_observability::RuntimeStatsRegistry;
use awaken_ext_observability::trace_store::TraceStore;
use awaken_runtime::credentials::CredentialBroker;
use awaken_runtime::{AgentResolver, AgentRuntime};
use awaken_server_contract::contract::config_store::ConfigStore;
use awaken_server_contract::contract::event_store::EventStore;
use awaken_server_contract::contract::mailbox::{MailboxInterrupt, RunDispatch};
use awaken_server_contract::contract::outbox::OutboxStore;
use awaken_server_contract::contract::run::{RunInput, RunKind};
use awaken_server_contract::contract::scope::{scoped_key, unscoped_key};
use awaken_server_contract::contract::storage::{ScopedThreadRunStore, ThreadRunStore};
use awaken_server_contract::{DEFAULT_SCOPE_ID, ScopeContext, ScopeId};

use awaken_server_contract::RedactedString;

use super::{AdminApiConfig, AuditLogConfig, ReplayBufferMap, ServerState, SkillCatalogProvider};
use crate::eval_limits::EvalLimits;
use crate::mailbox::Mailbox;
use crate::mailbox::MailboxSubmitResult;
use crate::outbox_relay::OutboxRelayError;
use crate::protocol_replay_state::A2aPushWebhookRelayConfig;
use crate::scope::{HttpScopeProvider, SingleScopeProvider};
use crate::services::audit_log::AuditLogger;
use crate::services::frozen_registry::ScopedServerResolverFactory;

#[derive(Clone)]
pub struct RunModuleState {
    pub runtime: Arc<AgentRuntime>,
    pub mailbox: Arc<Mailbox>,
    pub resolver: Arc<dyn AgentResolver>,
    pub store: Arc<dyn ThreadRunStore>,
    pub credential_broker: Arc<dyn CredentialBroker>,
    pub runtime_stats: Option<Arc<RuntimeStatsRegistry>>,
    pub scope_id: Option<ScopeId>,
    pub resolver_factory: Option<Arc<ScopedServerResolverFactory>>,
}

impl RunModuleState {
    pub fn new(
        runtime: Arc<AgentRuntime>,
        mailbox: Arc<Mailbox>,
        store: Arc<dyn ThreadRunStore>,
        resolver: Arc<dyn AgentResolver>,
    ) -> Self {
        Self {
            runtime,
            mailbox,
            resolver,
            store,
            credential_broker: Arc::new(awaken_runtime::credentials::AwakenCredentialBroker::new()),
            runtime_stats: None,
            scope_id: None,
            resolver_factory: None,
        }
    }

    pub fn mailbox(&self) -> Arc<Mailbox> {
        self.mailbox.clone()
    }

    #[must_use]
    pub fn with_credential_broker(mut self, broker: Arc<dyn CredentialBroker>) -> Self {
        self.credential_broker = broker;
        self
    }

    #[must_use]
    pub fn with_runtime_stats(mut self, registry: Arc<RuntimeStatsRegistry>) -> Self {
        self.runtime_stats = Some(registry);
        self
    }

    #[must_use]
    pub fn with_scoped_resolver_factory(
        mut self,
        factory: Arc<ScopedServerResolverFactory>,
    ) -> Self {
        self.resolver_factory = Some(factory);
        self
    }

    /// Borrow the thread/run store for read paths. Writes must be routed
    /// through `mailbox.coordinator()`.
    pub fn store(&self) -> &Arc<dyn ThreadRunStore> {
        &self.store
    }

    #[must_use]
    pub fn scoped_id(&self, id: &str) -> String {
        self.scope_id
            .as_ref()
            .map_or_else(|| id.to_string(), |scope| scoped_key(scope, id))
    }

    #[must_use]
    pub fn unscoped_id(&self, id: &str) -> String {
        self.scope_id.as_ref().map_or_else(
            || id.to_string(),
            |scope| unscoped_key(scope, id).unwrap_or(id).to_string(),
        )
    }

    #[must_use]
    pub fn scope_activation(
        &self,
        mut request: awaken_runtime::RunActivation,
    ) -> awaken_runtime::RunActivation {
        if self.scope_id.is_none() {
            return request;
        }
        if let Some(scope_id) = self.scope_id.clone()
            && let Some(factory) = &self.resolver_factory
        {
            request = request.with_run_resolver(factory.resolver_for_scope(scope_id));
        }
        request.intent.thread_id = self.scoped_id(&request.intent.thread_id);
        request.intent.kind = match request.intent.kind {
            RunKind::NewIntent => RunKind::NewIntent,
            RunKind::HitlResume { run_id } => RunKind::HitlResume {
                run_id: self.scoped_id(&run_id),
            },
            RunKind::ContinuationFromRun { run_id } => RunKind::ContinuationFromRun {
                run_id: self.scoped_id(&run_id),
            },
        };
        if let RunInput::AlreadyPersisted(input) = &mut request.input {
            input.thread_id = self.scoped_id(&input.thread_id);
        }
        if let Some(parent_run_id) = request.trace.parent_run_id.take() {
            request.trace.parent_run_id = Some(self.scoped_id(&parent_run_id));
        }
        if let Some(parent_thread_id) = request.trace.parent_thread_id.take() {
            request.trace.parent_thread_id = Some(self.scoped_id(&parent_thread_id));
        }
        if let Some(dispatch_id) = request.trace.dispatch_id.take() {
            request.trace.dispatch_id = Some(self.scoped_id(&dispatch_id));
        }
        if let Some(run_id_hint) = request.persistence.run_id_hint.take() {
            request.persistence.run_id_hint = Some(self.scoped_id(&run_id_hint));
        }
        if let Some(dispatch_id_hint) = request.persistence.dispatch_id_hint.take() {
            request.persistence.dispatch_id_hint = Some(self.scoped_id(&dispatch_id_hint));
        }
        request
    }

    #[must_use]
    pub fn unscope_submit_result(&self, mut result: MailboxSubmitResult) -> MailboxSubmitResult {
        result.dispatch_id = self.unscoped_id(&result.dispatch_id);
        result.run_id = self.unscoped_id(&result.run_id);
        result.thread_id = self.unscoped_id(&result.thread_id);
        result
    }

    #[must_use]
    pub fn unscope_dispatch(&self, mut dispatch: RunDispatch) -> RunDispatch {
        dispatch.dispatch_id = self.unscoped_id(&dispatch.dispatch_id);
        dispatch.thread_id = self.unscoped_id(&dispatch.thread_id);
        dispatch.run_id = self.unscoped_id(&dispatch.run_id);
        dispatch
    }

    #[must_use]
    pub fn unscope_interrupt(&self, mut interrupt: MailboxInterrupt) -> MailboxInterrupt {
        interrupt.active_dispatch = interrupt
            .active_dispatch
            .map(|dispatch| self.unscope_dispatch(dispatch));
        interrupt
    }
}

#[derive(Clone)]
pub struct ConfigModuleState {
    pub config_store: Arc<dyn ConfigStore>,
    pub runtime_manager: Arc<crate::services::config_runtime::ConfigRuntimeManager>,
    pub audit_log: Option<Arc<AuditLogger>>,
    pub skill_catalog_provider: Option<Arc<dyn SkillCatalogProvider>>,
}

impl ConfigModuleState {
    pub fn new(
        config_store: Arc<dyn ConfigStore>,
        runtime_manager: Arc<crate::services::config_runtime::ConfigRuntimeManager>,
    ) -> Self {
        Self {
            config_store,
            runtime_manager,
            audit_log: None,
            skill_catalog_provider: None,
        }
    }

    #[must_use]
    pub fn with_audit_log(mut self, audit_log: Arc<AuditLogger>) -> Self {
        self.audit_log = Some(audit_log);
        self
    }

    #[must_use]
    pub fn with_skill_catalog_provider(mut self, provider: Arc<dyn SkillCatalogProvider>) -> Self {
        self.skill_catalog_provider = Some(provider);
        self
    }
}

#[derive(Clone)]
pub struct EventModuleState {
    pub event_store: Arc<dyn EventStore>,
}

#[derive(Clone)]
pub struct EvalModuleState {
    pub eval_run_store: Arc<dyn awaken_eval::EvalRunStore>,
}

#[derive(Clone)]
pub struct TraceModuleState {
    pub trace_store: Arc<dyn TraceStore>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct A2aPushDriverKey {
    tenant: Option<String>,
    task_id: String,
    config_id: String,
}

impl A2aPushDriverKey {
    fn new(task_id: &str, tenant: Option<&str>, config_id: &str) -> Self {
        Self {
            tenant: tenant.map(ToOwned::to_owned),
            task_id: task_id.to_string(),
            config_id: config_id.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct ProtocolModuleState {
    pub replay_buffers: ReplayBufferMap,
    pub mcp_http: Arc<crate::protocols::mcp::http::McpHttpState>,
    pub a2a_push_outbox: Arc<dyn OutboxStore>,
    pub a2a_push_relay_config: A2aPushWebhookRelayConfig,
    a2a_push_driver_keys: Arc<parking_lot::Mutex<HashSet<A2aPushDriverKey>>>,
}

impl ProtocolModuleState {
    /// Build protocol state with a process-local, in-memory A2A push outbox.
    ///
    /// The default outbox is best-effort and non-durable: queued webhook
    /// deliveries are lost on restart and are not shared across replicas. Because
    /// an outbox relay is registered, the A2A agent card advertises
    /// `pushNotifications: true` for the default state. For durable or
    /// multi-replica webhook delivery, inject an external outbox via
    /// [`ServerState::with_a2a_push_webhook_outbox`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_a2a_push_outbox(
            Arc::new(awaken_stores::InMemoryOutboxStore::new()),
            A2aPushWebhookRelayConfig::default(),
        )
    }

    #[must_use]
    pub fn with_a2a_push_outbox(
        outbox: Arc<dyn OutboxStore>,
        relay_config: A2aPushWebhookRelayConfig,
    ) -> Self {
        Self {
            replay_buffers: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            mcp_http: Arc::new(crate::protocols::mcp::http::McpHttpState::new()),
            a2a_push_outbox: outbox,
            a2a_push_relay_config: relay_config,
            a2a_push_driver_keys: Arc::new(parking_lot::Mutex::new(HashSet::new())),
        }
    }

    pub(crate) fn register_a2a_push_webhook_relay(&self) -> Result<(), OutboxRelayError> {
        crate::protocol_replay_state::register_a2a_push_webhook_relay_for_buffers(
            &self.replay_buffers,
            self.a2a_push_outbox.clone(),
            self.a2a_push_relay_config.clone(),
        )
    }

    pub(crate) fn register_a2a_push_driver(
        &self,
        task_id: &str,
        tenant: Option<&str>,
        config_id: &str,
    ) -> bool {
        let key = A2aPushDriverKey::new(task_id, tenant, config_id);
        self.a2a_push_driver_keys.lock().insert(key) // true if newly inserted
    }

    pub(crate) fn unregister_a2a_push_driver(
        &self,
        task_id: &str,
        tenant: Option<&str>,
        config_id: &str,
    ) {
        let key = A2aPushDriverKey::new(task_id, tenant, config_id);
        self.a2a_push_driver_keys.lock().remove(&key);
    }
}

impl Default for ProtocolModuleState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct AdminModuleState {
    pub admin_api_config: AdminApiConfig,
    pub audit_log_config: AuditLogConfig,
    pub started_at: Instant,
}

#[derive(Clone)]
pub struct RunRoutesState {
    pub run: RunModuleState,
    pub events: Option<EventModuleState>,
    pub sse_buffer_size: usize,
    pub scope_provider: Arc<dyn HttpScopeProvider>,
}

impl RunRoutesState {
    pub fn scoped(&self, scope: &ScopeContext) -> Self {
        if scope.scope_id.as_str() == DEFAULT_SCOPE_ID {
            return self.clone();
        }
        let mut next = self.clone();
        next.run.store = Arc::new(ScopedThreadRunStore::new(
            self.run.store.clone(),
            scope.scope_id.clone(),
        ));
        next.run.scope_id = Some(scope.scope_id.clone());
        next
    }
}

#[derive(Clone)]
pub struct AdminRunRoutesState {
    pub admin: AdminModuleState,
    pub run: RunModuleState,
    pub scope_provider: Arc<dyn HttpScopeProvider>,
}

#[derive(Clone)]
pub struct ConfigRoutesState {
    pub admin: AdminModuleState,
    pub config: ConfigModuleState,
    pub run: RunModuleState,
    pub scope_provider: Arc<dyn HttpScopeProvider>,
}

#[derive(Clone)]
pub struct EvalRoutesState {
    pub admin: AdminModuleState,
    pub config: ConfigModuleState,
    pub eval: EvalModuleState,
    pub run: RunModuleState,
    pub trace: Option<TraceModuleState>,
    pub events: Option<EventModuleState>,
    pub limits: EvalLimits,
    pub scope_provider: Arc<dyn HttpScopeProvider>,
}

#[derive(Clone)]
pub struct ProtocolRoutesState {
    pub admin: AdminModuleState,
    pub run: RunModuleState,
    pub protocol: ProtocolModuleState,
    pub sse_buffer_size: usize,
    pub replay_buffer_capacity: usize,
    pub a2a_extended_card_bearer_token: Option<RedactedString>,
    pub scope_provider: Arc<dyn HttpScopeProvider>,
}

impl ProtocolRoutesState {
    pub fn scoped(&self, scope: &ScopeContext) -> Self {
        if scope.scope_id.as_str() == DEFAULT_SCOPE_ID {
            return self.clone();
        }
        let mut next = self.clone();
        next.run.store = Arc::new(ScopedThreadRunStore::new(
            self.run.store.clone(),
            scope.scope_id.clone(),
        ));
        next.run.scope_id = Some(scope.scope_id.clone());
        next
    }

    pub fn insert_replay_buffer(
        &self,
        key: String,
        buffer: Arc<crate::transport::replay_buffer::EventReplayBuffer>,
    ) {
        self.protocol
            .replay_buffers
            .lock()
            .insert(key, (buffer, Instant::now()));
    }

    pub fn get_replay_buffer(
        &self,
        key: &str,
    ) -> Option<Arc<crate::transport::replay_buffer::EventReplayBuffer>> {
        self.protocol
            .replay_buffers
            .lock()
            .get(key)
            .map(|(buf, _)| Arc::clone(buf))
    }

    pub fn remove_replay_buffer(&self, key: &str) {
        self.protocol.replay_buffers.lock().remove(key);
    }
}

#[derive(Clone)]
pub struct SystemRoutesState {
    pub admin: AdminModuleState,
    pub mounted_modules: Vec<&'static str>,
    pub config_store_enabled: bool,
    pub audit_log_enabled: bool,
    pub runtime_stats_enabled: bool,
    pub scope_provider: Arc<dyn HttpScopeProvider>,
}

#[derive(Clone)]
pub struct TraceRoutesState {
    pub admin: AdminModuleState,
    pub trace: TraceModuleState,
    pub scope_provider: Arc<dyn HttpScopeProvider>,
}

impl ServerState {
    #[must_use]
    pub fn from_modules(run: RunModuleState, server_config: super::ServerConfig) -> Self {
        let state = Self {
            run,
            config: None,
            events: None,
            eval: None,
            trace: None,
            protocol: ProtocolModuleState::new(),
            admin: AdminModuleState {
                admin_api_config: super::AdminApiConfig::default(),
                audit_log_config: super::AuditLogConfig::default(),
                started_at: Instant::now(),
            },
            server_config,
            scope_provider: Arc::new(SingleScopeProvider::default()),
        };
        state
            .protocol
            .register_a2a_push_webhook_relay()
            .expect("default A2A push webhook relay config is valid");
        state
    }

    #[must_use]
    pub fn with_config(mut self, config: ConfigModuleState) -> Self {
        self.config = Some(config);
        self
    }

    #[must_use]
    pub fn with_events(mut self, events: EventModuleState) -> Self {
        self.events = Some(events);
        self
    }

    #[must_use]
    pub fn with_eval(mut self, eval: EvalModuleState) -> Self {
        self.eval = Some(eval);
        self
    }

    #[must_use]
    pub fn with_trace(mut self, trace: TraceModuleState) -> Self {
        self.trace = Some(trace);
        self
    }

    #[must_use]
    pub fn with_protocol(mut self, protocol: ProtocolModuleState) -> Self {
        let previous_buffers = self.protocol.replay_buffers.clone();
        self.protocol = protocol;
        // Replacing the protocol module swaps the replay-buffer identity used by
        // the relay registry. Migrate attachments registered against the previous
        // buffers (log/projector/fanout) so a relay configured before this call is
        // not silently orphaned, then (re)register the replacement module's A2A
        // push outbox instead of falling back to a hidden in-memory store.
        crate::protocol_replay_state::migrate_protocol_attachments(
            &previous_buffers,
            &self.protocol.replay_buffers,
        );
        self.protocol
            .register_a2a_push_webhook_relay()
            .expect("default A2A push webhook relay config is valid");
        self
    }

    #[must_use]
    pub fn with_admin(mut self, admin: AdminModuleState) -> Self {
        self.admin = admin;
        self
    }

    pub fn run_module(&self) -> RunModuleState {
        self.run.clone()
    }

    pub fn config_module(&self) -> Option<ConfigModuleState> {
        self.config.clone()
    }

    pub fn event_module(&self) -> Option<EventModuleState> {
        self.events.clone()
    }

    pub fn eval_module(&self) -> Option<EvalModuleState> {
        self.eval.clone()
    }

    pub fn trace_module(&self) -> Option<TraceModuleState> {
        self.trace.clone()
    }

    pub fn protocol_module(&self) -> ProtocolModuleState {
        self.protocol.clone()
    }

    pub fn admin_module(&self) -> AdminModuleState {
        AdminModuleState {
            admin_api_config: super::admin_api_config(self),
            audit_log_config: self.admin.audit_log_config,
            started_at: self.admin.started_at,
        }
    }

    pub fn mounted_modules(&self) -> Vec<&'static str> {
        let mut modules = vec!["run", "admin", "protocol"];
        if self.config_module().is_some() {
            modules.push("config");
        }
        if self.event_module().is_some() {
            modules.push("events");
        }
        if self.eval_module().is_some() {
            modules.push("eval");
        }
        if self.trace_module().is_some() {
            modules.push("trace");
        }
        modules
    }

    pub fn run_routes_state(&self) -> RunRoutesState {
        RunRoutesState {
            run: self.run_module(),
            events: self.event_module(),
            sse_buffer_size: self.server_config.sse_buffer_size,
            scope_provider: self.scope_provider.clone(),
        }
    }

    pub fn admin_run_routes_state(&self) -> AdminRunRoutesState {
        AdminRunRoutesState {
            admin: self.admin_module(),
            run: self.run_module(),
            scope_provider: self.scope_provider.clone(),
        }
    }

    pub fn config_routes_state(&self) -> Option<ConfigRoutesState> {
        Some(ConfigRoutesState {
            admin: self.admin_module(),
            config: self.config_module()?,
            run: self.run_module(),
            scope_provider: self.scope_provider.clone(),
        })
    }

    pub fn eval_routes_state(&self) -> Option<EvalRoutesState> {
        Some(EvalRoutesState {
            admin: self.admin_module(),
            config: self.config_module()?,
            eval: self.eval_module()?,
            run: self.run_module(),
            trace: self.trace_module(),
            events: self.event_module(),
            limits: self.server_config.eval_limits.clone(),
            scope_provider: self.scope_provider.clone(),
        })
    }

    pub fn protocol_routes_state(&self) -> ProtocolRoutesState {
        ProtocolRoutesState {
            admin: self.admin_module(),
            run: self.run_module(),
            protocol: self.protocol_module(),
            sse_buffer_size: self.server_config.sse_buffer_size,
            replay_buffer_capacity: self.server_config.replay_buffer_capacity,
            a2a_extended_card_bearer_token: self
                .server_config
                .a2a_extended_card_bearer_token
                .clone(),
            scope_provider: self.scope_provider.clone(),
        }
    }

    pub fn system_routes_state(&self) -> SystemRoutesState {
        SystemRoutesState {
            admin: self.admin_module(),
            mounted_modules: self.mounted_modules(),
            config_store_enabled: self.config.is_some(),
            audit_log_enabled: self.audit_log().is_some(),
            runtime_stats_enabled: self.runtime_stats().is_some(),
            scope_provider: self.scope_provider.clone(),
        }
    }

    pub fn trace_routes_state(&self) -> Option<TraceRoutesState> {
        Some(TraceRoutesState {
            admin: self.admin_module(),
            trace: self.trace_module()?,
            scope_provider: self.scope_provider.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_push_driver_registry_is_single_flight_per_task_tenant_config() {
        let protocol = ProtocolModuleState::new();

        assert!(protocol.register_a2a_push_driver("task-1", Some("tenant-1"), "cfg-1"));
        assert!(!protocol.register_a2a_push_driver("task-1", Some("tenant-1"), "cfg-1"));

        assert!(protocol.register_a2a_push_driver("task-1", Some("tenant-1"), "cfg-2"));
        assert!(protocol.register_a2a_push_driver("task-2", Some("tenant-1"), "cfg-1"));

        protocol.unregister_a2a_push_driver("task-1", Some("tenant-1"), "cfg-1");
        assert!(protocol.register_a2a_push_driver("task-1", Some("tenant-1"), "cfg-1"));
    }
}
