// @formal surface requirement=protocol.acp.acp_normalization
// @formal reason: Static owner: ACP permission resolution split out of the executor, including the
// @formal reason: attempt's audit ledger. Dynamic path: every CLI permission request is projected onto the
// @formal reason: one neutral ToolPermissionPolicy, a per-Run narrowing may refuse before the session
// @formal reason: policy answers, a resumed one-shot decision is consumed at most once for its exact
// @formal reason: semantic identity, and whichever authority decided records that decision into the commit
// @formal reason: carrying the step it governed; nothing here can widen authority or execute a tool.
// @formal reason: Evidence: protocol.acp.acp_normalization plus this module's resolver tests and
// @formal reason: every_acp_permission_decision_is_recorded_with_the_policy_that_made_it; this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! ACP permission resolution: the bridge from the driver's `PermissionResolver`
//! port onto the one neutral `ToolPermissionPolicy`, the per-Run narrowing, MCP
//! alias consensus, resumed one-shot decisions, and the attempt's audit ledger.
//! Every decision made here is recorded the way the native gate records its
//! own, so a session run by an external CLI leaves the same committed facts.
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// Bridges the ACP driver's [`PermissionResolver`] port onto the single neutral
/// [`ToolPermissionPolicy`] authority (G21). It projects the wire ask into a neutral
/// [`ToolCall`], asks the policy, and maps the decision back to a wire
/// verdict — so an external CLI's tool requests are decided by the same policy that
/// governs native tools. `Ask` closes the current process at a durable permission
/// boundary; the Managed resume path relaunches the ACP session and supplies the
/// exact call's one-shot decision. Immediate `Allow`/`Deny` pass straight through.
pub(crate) struct NeutralPermissionResolver {
    pub(crate) policy: Arc<dyn ToolPermissionPolicy>,
    pub(crate) grant_observer: Option<Arc<dyn PermissionGrantObserver>>,
    /// The attempt's audit ledger. The executor-wide resolver has none; each
    /// attempt derives its own with [`Self::recording_into`], so a decision is
    /// committed with its own step and never with another attempt's.
    pub(crate) audit: Option<Arc<PermissionAudit>>,
}

impl NeutralPermissionResolver {
    pub(crate) fn recording_into(&self, audit: Arc<PermissionAudit>) -> Self {
        Self {
            policy: self.policy.clone(),
            grant_observer: self.grant_observer.clone(),
            audit: Some(audit),
        }
    }
}

pub use awaken_runtime_contract::permission::PermissionGrantObserver;

/// The permission decisions one attempt made since its last commit.
///
/// The native loop stages a `PermissionDecided` audit fact in the same commit
/// as the tool batch it governs. An ACP session is governed by the same neutral
/// policy but runs its tools inside an external CLI, so without this ledger it
/// left no record at all — and the Managed projection, having nothing to read,
/// fell back to reporting every such call as permitted. `commit` drains it, so
/// a decision is durable with exactly the step it governed and never outlives
/// the attempt that made it.
#[derive(Default)]
pub(crate) struct PermissionAudit(
    awaken_sync::LeafLock<Vec<awaken_agent_contract::audit::draft::Draft>>,
);

impl PermissionAudit {
    pub(crate) fn record(&self, call: &ToolCall, ruling: &ToolPermissionRuling) {
        let draft: awaken_agent_contract::audit::draft::Draft =
            GateAuthority::from(ruling.authority.clone())
                .permission_decided(
                    call,
                    ruling.verdict.kind().gate_outcome_kind().audit_decision(),
                )
                .into();
        self.0.lock().push(draft);
    }

    pub(crate) fn drain(&self) -> Vec<awaken_agent_contract::audit::draft::Draft> {
        std::mem::take(&mut *self.0.lock())
    }
}

/// Per-Run narrowing in front of the Session's configured ACP authority. Only
/// when the narrowing allows does the base resolver get a chance to decide.
pub(crate) struct NarrowedPermissionResolver<'a> {
    pub(crate) base: &'a dyn PermissionResolver,
    pub(crate) narrowing: Option<&'a dyn ToolPermissionPolicy>,
    pub(crate) capability: awaken_runtime_contract::permission::ToolCapabilityNarrowing,
    pub(crate) audit: &'a PermissionAudit,
}

/// Some ACP adapters report an MCP server's protocol-advertised name instead
/// of the alias supplied in `session/new`. The Session aliases remain the only
/// policy namespaces. This adapter evaluates a drifted name against those
/// existing namespaces and combines their verdicts conservatively; it neither
/// owns nor widens policy.
pub(crate) struct AliasedMcpPermissionResolver<'a> {
    pub(crate) base: &'a dyn PermissionResolver,
    pub(crate) mcp_server_names: &'a [String],
}

/// The in-Session MCP server through which Awaken bridges its own builtin
/// tools. A call on it is the builtin tool, not an external MCP tool.
const AWAKEN_SESSION_MCP_SERVER: &str = "awaken_session";

/// Project whatever spelling an ACP agent reported onto the one canonical tool
/// identity, so a permission decision, a committed ToolUse and a resume match
/// all compare the same string.
///
/// Both spellings are decomposed by the owning type: this function used to
/// build `mcp__{server}__{name}` from an unsanitized ACP-reported server and
/// split the dotted form on its *first* dot, so `mcp.a.b.c` on server `a.b`
/// produced `mcp__a__b.c` — an id belonging to no configured namespace, which
/// therefore matched no policy.
pub(crate) fn canonical_permission_tool_id(tool: &str) -> String {
    let Some(id) = McpToolId::parse(tool).or_else(|| McpToolId::from_dotted(tool)) else {
        return tool.to_string();
    };
    if id.is_served_by(AWAKEN_SESSION_MCP_SERVER) {
        return id.tool_name();
    }
    id.as_str().to_string()
}

#[async_trait]
impl PermissionResolver for NarrowedPermissionResolver<'_> {
    async fn resolve(&self, ask: &PermissionAsk) -> PermissionVerdict {
        let call = ToolCall {
            tool_id: canonical_permission_tool_id(&ask.tool),
            call_id: ask.call_id.clone(),
            arguments: ask.arguments.clone(),
        };
        let Some(ruling) = self
            .capability
            .evaluate_run_policy(self.narrowing, &call)
            .await
        else {
            return self.base.resolve(ask).await;
        };
        match ruling.verdict.clone() {
            // An allowing narrowing is not the answer; the base resolver's is,
            // and it records its own.
            ToolPermissionVerdict::Allow => self.base.resolve(ask).await,
            ToolPermissionVerdict::Deny { .. } => {
                self.audit.record(&call, &ruling);
                PermissionVerdict::Deny
            }
            ToolPermissionVerdict::RequireConfirmation { correlation_id } => {
                self.audit.record(&call, &ruling);
                PermissionVerdict::Await { correlation_id }
            }
        }
    }
}

#[async_trait]
impl PermissionResolver for AliasedMcpPermissionResolver<'_> {
    async fn resolve(&self, ask: &PermissionAsk) -> PermissionVerdict {
        let Some(reported) = McpToolId::from_dotted(&ask.tool) else {
            return self.base.resolve(ask).await;
        };
        let tool_name = reported.tool_name();
        if self.mcp_server_names.is_empty()
            || self
                .mcp_server_names
                .iter()
                .any(|name| reported.is_served_by(name))
        {
            return self.base.resolve(ask).await;
        }

        let mut consensus = PermissionConsensus::Allow;
        let mut confirmation = None;
        for server_name in self.mcp_server_names {
            let candidate = PermissionAsk {
                tool: McpToolId::dotted(server_name, &tool_name),
                call_id: ask.call_id.clone(),
                arguments: ask.arguments.clone(),
            };
            let verdict = self.base.resolve(&candidate).await;
            consensus =
                combine_permission_consensus(consensus, permission_consensus_class(&verdict));
            match verdict {
                PermissionVerdict::Deny => return PermissionVerdict::Deny,
                PermissionVerdict::Await { correlation_id } => {
                    confirmation.get_or_insert(correlation_id);
                }
                PermissionVerdict::Allow => {}
            }
        }
        match consensus {
            PermissionConsensus::Allow => PermissionVerdict::Allow,
            PermissionConsensus::Await => PermissionVerdict::Await {
                correlation_id: confirmation
                    .expect("await consensus retains the first correlation id"),
            },
            PermissionConsensus::Deny => PermissionVerdict::Deny,
        }
    }
}

#[async_trait]
impl PermissionResolver for NeutralPermissionResolver {
    async fn resolve(&self, ask: &PermissionAsk) -> PermissionVerdict {
        resolve_neutral_permission(
            self.policy.as_ref(),
            self.grant_observer.as_deref(),
            self.audit.as_deref(),
            ask,
        )
        .await
    }
}

pub(crate) async fn resolve_neutral_permission(
    policy: &dyn ToolPermissionPolicy,
    grant_observer: Option<&dyn PermissionGrantObserver>,
    audit: Option<&PermissionAudit>,
    ask: &PermissionAsk,
) -> PermissionVerdict {
    let ctx = ToolCall {
        tool_id: canonical_permission_tool_id(&ask.tool),
        call_id: ask.call_id.clone(),
        arguments: ask.arguments.clone(),
    };
    // The basis is asked for, not inferred: the same policy that decides also
    // names which authored policy decided, exactly as the native gate records it.
    let ruling = policy.evaluate(&ctx).await;
    if let Some(audit) = audit {
        audit.record(&ctx, &ruling);
    }
    match ruling.verdict.clone() {
        ToolPermissionVerdict::Allow => {
            if let Some(observer) = grant_observer {
                observer.on_allow(&ctx);
            }
            PermissionVerdict::Allow
        }
        ToolPermissionVerdict::Deny { .. } => PermissionVerdict::Deny,
        ToolPermissionVerdict::RequireConfirmation { correlation_id } => {
            PermissionVerdict::Await { correlation_id }
        }
    }
}

/// One resumed durable decision, scoped to the exact ACP tool and arguments held
/// by the committed ticket. A replacement ACP process may assign a new wire call
/// id when it reconstructs the interrupted request, so the immutable semantic
/// identity is authoritative; the decision is still consumed at most once. Any
/// different or later request goes through current policy.
pub(crate) struct ResumedPermissionResolver<'a> {
    pub(crate) base: &'a dyn PermissionResolver,
    pub(crate) decision: &'a PermissionResume,
    pub(crate) consumed: std::sync::atomic::AtomicBool,
    pub(crate) grant_observer: Option<&'a dyn PermissionGrantObserver>,
}

impl<'a> ResumedPermissionResolver<'a> {
    /// Absence of consumption means the guarded original request never received
    /// execution authority in this continuation. A changed request can obtain
    /// its own decision, but it cannot consume or complete this one.
    pub(crate) fn unconsumed_decision(&self) -> Option<&PermissionResume> {
        (!self.consumed.load(std::sync::atomic::Ordering::Acquire)).then_some(self.decision)
    }

    pub(crate) fn new(
        base: &'a dyn PermissionResolver,
        decision: &'a PermissionResume,
        grant_observer: Option<&'a dyn PermissionGrantObserver>,
    ) -> Self {
        Self {
            base,
            decision,
            consumed: std::sync::atomic::AtomicBool::new(false),
            grant_observer,
        }
    }
}

#[async_trait]
impl PermissionResolver for ResumedPermissionResolver<'_> {
    async fn resolve(&self, ask: &PermissionAsk) -> PermissionVerdict {
        if canonical_permission_tool_id(&ask.tool)
            == canonical_permission_tool_id(&self.decision.tool_id)
            && ask.arguments == self.decision.arguments
            && !self
                .consumed
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return if self.decision.allow {
                if let Some(observer) = self.grant_observer {
                    observer.on_allow(&ToolCall {
                        tool_id: canonical_permission_tool_id(&ask.tool),
                        call_id: ask.call_id.clone(),
                        arguments: ask.arguments.clone(),
                    });
                }
                PermissionVerdict::Allow
            } else {
                PermissionVerdict::Deny
            };
        }
        self.base.resolve(ask).await
    }
}
