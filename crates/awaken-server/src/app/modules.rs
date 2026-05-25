use std::sync::Arc;
use std::time::Instant;

use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::event_store::EventStore;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_ext_observability::RuntimeStatsRegistry;
use awaken_ext_observability::trace_store::TraceStore;
use awaken_runtime::credentials::CredentialBroker;
use awaken_runtime::{AgentResolver, AgentRuntime};

use awaken_contract::RedactedString;

use super::{AdminApiConfig, AuditLogConfig, ReplayBufferMap, ServerState, SkillCatalogProvider};
use crate::eval_limits::EvalLimits;
use crate::mailbox::Mailbox;
use crate::services::audit_log::AuditLogger;

#[derive(Clone)]
pub struct RunModuleState {
    pub runtime: Arc<AgentRuntime>,
    pub mailbox: Arc<Mailbox>,
    pub resolver: Arc<dyn AgentResolver>,
    pub store: Arc<dyn ThreadRunStore>,
    pub credential_broker: Arc<dyn CredentialBroker>,
    pub runtime_stats: Option<Arc<RuntimeStatsRegistry>>,
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
        }
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

    /// Borrow the thread/run store for read paths. Writes must be routed
    /// through `mailbox.coordinator()`.
    pub fn store(&self) -> &Arc<dyn ThreadRunStore> {
        &self.store
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

#[derive(Clone)]
pub struct ProtocolModuleState {
    pub replay_buffers: ReplayBufferMap,
    pub mcp_http: Arc<crate::protocols::mcp::http::McpHttpState>,
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
}

#[derive(Clone)]
pub struct AdminRunRoutesState {
    pub admin: AdminModuleState,
    pub run: RunModuleState,
}

#[derive(Clone)]
pub struct ConfigRoutesState {
    pub admin: AdminModuleState,
    pub config: ConfigModuleState,
    pub run: RunModuleState,
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
}

#[derive(Clone)]
pub struct ProtocolRoutesState {
    pub admin: AdminModuleState,
    pub run: RunModuleState,
    pub protocol: ProtocolModuleState,
    pub sse_buffer_size: usize,
    pub replay_buffer_capacity: usize,
    pub a2a_extended_card_bearer_token: Option<RedactedString>,
}

impl ProtocolRoutesState {
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
}

#[derive(Clone)]
pub struct TraceRoutesState {
    pub admin: AdminModuleState,
    pub trace: TraceModuleState,
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
            protocol: ProtocolModuleState {
                replay_buffers: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
                mcp_http: Arc::new(crate::protocols::mcp::http::McpHttpState::new()),
            },
            admin: AdminModuleState {
                admin_api_config: super::AdminApiConfig::default(),
                audit_log_config: super::AuditLogConfig::default(),
                started_at: Instant::now(),
            },
            server_config,
        };
        crate::protocol_replay_state::register_a2a_push_webhook_relay_for_buffers(
            &state.protocol.replay_buffers,
            Arc::new(awaken_stores::InMemoryOutboxStore::new()),
            crate::protocol_replay_state::A2aPushWebhookRelayConfig::default(),
        )
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
        self.protocol = protocol;
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
        }
    }

    pub fn admin_run_routes_state(&self) -> AdminRunRoutesState {
        AdminRunRoutesState {
            admin: self.admin_module(),
            run: self.run_module(),
        }
    }

    pub fn config_routes_state(&self) -> Option<ConfigRoutesState> {
        Some(ConfigRoutesState {
            admin: self.admin_module(),
            config: self.config_module()?,
            run: self.run_module(),
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
        }
    }

    pub fn system_routes_state(&self) -> SystemRoutesState {
        SystemRoutesState {
            admin: self.admin_module(),
            mounted_modules: self.mounted_modules(),
            config_store_enabled: self.config.is_some(),
            audit_log_enabled: self.audit_log().is_some(),
            runtime_stats_enabled: self.runtime_stats().is_some(),
        }
    }

    pub fn trace_routes_state(&self) -> Option<TraceRoutesState> {
        Some(TraceRoutesState {
            admin: self.admin_module(),
            trace: self.trace_module()?,
        })
    }
}
