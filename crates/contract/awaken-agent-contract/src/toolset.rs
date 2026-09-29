// @formal entrypoint tool_policy.managed_agent_and_mcp_defaults_are_source_exact_and_configurable
// @formal entrypoint tool_policy.selector_fails_closed_for_unregistered_widened_or_mismatched_calls
//! Neutral, resolved tool-family policy shared by authoring and execution.

use serde::{Deserialize, Serialize};

/// Model-visible tool executed by the protocol client rather than by a Host
/// executor. Identity, description, and schema are frozen with an executable
/// Agent publication and may be copied into a Session's neutral tool policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsetPolicy {
    pub source: ToolsetSource,
    pub default: ToolExecutionPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<ToolPolicyOverride>,
}

/// Frozen caller toolsets for one Run, ordered nearest caller to outermost
/// caller. Extensions interpret only their own configuration values. This is
/// execution input, never an edit to an immutable Agent publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "version", content = "sources", deny_unknown_fields)]
pub enum InheritedToolsets {
    #[serde(rename = "v1")]
    V1(Vec<Vec<ToolsetPolicy>>),
}

impl InheritedToolsets {
    /// Freeze the caller's own settings before its already-frozen ancestors.
    #[must_use]
    pub fn with_caller(caller: Vec<ToolsetPolicy>, inherited: Option<&Self>) -> Self {
        let mut sources = vec![caller];
        if let Some(inherited) = inherited {
            sources.extend_from_slice(inherited.sources());
        }
        Self::V1(sources)
    }

    #[must_use]
    pub fn sources(&self) -> &[Vec<ToolsetPolicy>] {
        match self {
            Self::V1(sources) => sources,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolsetSource {
    Agent,
    Mcp { server_name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionPolicy {
    pub enabled: bool,
    pub permission: ToolPermissionRequirement,
}

impl Default for ToolExecutionPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            permission: ToolPermissionRequirement::AlwaysAllow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionRequirement {
    AlwaysAllow,
    AlwaysAsk,
    /// The platform evaluates each call and decides: it runs, it is refused, or
    /// it falls through to a person. An author picks this to delegate judgement
    /// for a tool rather than state one fixed answer for every call.
    Auto,
}

/// How much authority to act *without a person in the loop* a policy delegates.
///
/// This is the one axis admission compares on, and naming it is what keeps that
/// comparison meaningful as the policy vocabulary grows: the question a session
/// override must answer is not "is this stricter?" but "does this let more run
/// unattended than the author allowed?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum GrantedAutonomy {
    /// Nothing runs until a person decides.
    None,
    /// Some calls run on their own; the platform decides which, and may refuse
    /// one outright or hand it to a person.
    Evaluated,
    /// Everything the author enabled runs on its own.
    Full,
}

impl GrantedAutonomy {
    /// This level's position on the autonomy axis, ascending.
    ///
    /// Stated rather than derived from declaration order. A derived `Ord` would
    /// make admission silently depend on how the variants happen to be written
    /// down, so reordering them would invert the safety comparison while every
    /// existing two-ended test still passed. Here the order is a value a
    /// reviewer reads and a proof pins.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Evaluated => 1,
            Self::Full => 2,
        }
    }

    /// Whether this level delegates no more unattended authority than `ceiling`.
    #[must_use]
    pub const fn within(self, ceiling: Self) -> bool {
        self.rank() <= ceiling.rank()
    }
}

impl ToolPermissionRequirement {
    /// The unattended authority this requirement delegates.
    #[must_use]
    pub const fn granted_autonomy(self) -> GrantedAutonomy {
        match self {
            Self::AlwaysAsk => GrantedAutonomy::None,
            Self::Auto => GrantedAutonomy::Evaluated,
            Self::AlwaysAllow => GrantedAutonomy::Full,
        }
    }
}

/// Admit an execution request against the published tool policy.
///
/// A session-local request may narrow the authored authority but never widen
/// it, compared on [`GrantedAutonomy`]: asking when automatic execution was
/// allowed is a safe narrowing, while claiming unattended authority the author
/// withheld is rejected. Comparing the named axis rather than matching specific
/// pairs is what lets a new requirement join without reopening this decision.
#[must_use]
pub const fn select_tool_policy(
    registered: bool,
    exact_tool_match: bool,
    authored: ToolExecutionPolicy,
    requested: ToolPermissionRequirement,
) -> Option<ToolExecutionPolicy> {
    if !registered || !exact_tool_match || !authored.enabled {
        return None;
    }
    if !requested
        .granted_autonomy()
        .within(authored.permission.granted_autonomy())
    {
        return None;
    }
    Some(ToolExecutionPolicy {
        enabled: true,
        permission: requested,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicyOverride {
    pub name: String,
    pub policy: ToolExecutionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<serde_json::Value>,
}

impl ToolPolicyOverride {
    /// Construct the canonical policy-only override used by tools without
    /// execution-specific settings.
    #[must_use]
    pub fn new(name: impl Into<String>, policy: ToolExecutionPolicy) -> Self {
        Self::with_optional_configuration(name, policy, None)
    }

    /// Construct from a projection that has already resolved whether a
    /// tool-specific configuration exists.
    #[must_use]
    pub fn with_optional_configuration(
        name: impl Into<String>,
        policy: ToolExecutionPolicy,
        configuration: Option<serde_json::Value>,
    ) -> Self {
        Self {
            name: name.into(),
            policy,
            configuration,
        }
    }
}

impl ToolsetPolicy {
    #[must_use]
    pub fn policy_for(&self, name: &str) -> ToolExecutionPolicy {
        self.overrides
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.policy)
            .unwrap_or(self.default)
    }

    #[must_use]
    pub fn configuration_for(&self, name: &str) -> Option<&serde_json::Value> {
        self.overrides
            .iter()
            .find(|entry| entry.name == name)
            .and_then(|entry| entry.configuration.as_ref())
    }

    /// Select a policy for a call only after the executable registry supplies
    /// its canonical tool name. Missing registration and alias/name mismatch
    /// fail closed before the default toolset policy can grant authority.
    #[must_use]
    pub fn policy_for_registered_call(
        &self,
        registered_name: Option<&str>,
        requested_name: &str,
        requested_permission: ToolPermissionRequirement,
    ) -> Option<ToolExecutionPolicy> {
        select_tool_policy(
            registered_name.is_some(),
            registered_name == Some(requested_name),
            self.policy_for(requested_name),
            requested_permission,
        )
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Range over the COMPLETE requirement vocabulary.
    ///
    /// A two-valued generator silently stops covering the domain the moment a
    /// variant is added — the proof keeps passing while a third of its input
    /// space goes unexplored. The exhaustiveness assertion below turns that
    /// failure mode into a build error instead.
    fn symbolic_permission() -> ToolPermissionRequirement {
        match kani::any::<u8>() % 3 {
            0 => ToolPermissionRequirement::AlwaysAllow,
            1 => ToolPermissionRequirement::AlwaysAsk,
            _ => ToolPermissionRequirement::Auto,
        }
    }

    /// Fail the build when a requirement is added without extending the
    /// generator above. The match is exhaustive, so a new variant cannot be
    /// introduced without deciding how the proofs explore it.
    const fn requirement_generator_is_exhaustive(value: ToolPermissionRequirement) -> u8 {
        match value {
            ToolPermissionRequirement::AlwaysAllow => 0,
            ToolPermissionRequirement::AlwaysAsk => 1,
            ToolPermissionRequirement::Auto => 2,
        }
    }

    fn symbolic_policy(enabled: bool) -> ToolExecutionPolicy {
        ToolExecutionPolicy {
            enabled,
            permission: symbolic_permission(),
        }
    }

    /// @obligation tool_policy.managed_agent_and_mcp_defaults_are_source_exact_and_configurable
    #[kani::proof]
    fn toolset_policy_exact_override_replaces_only_the_matching_default() {
        // Cause/effect decision table: C1 lookup is the exact "search" name or
        // the distinct "fetch" name; C2 default and override independently
        // range over enabled x allow/ask. E1 exact returns the override; E2 the
        // foreign name returns the default. The fixed non-empty names prove the
        // production lookup's selection priority; arbitrary names and override
        // counts remain outside this scalar harness.
        let default = symbolic_policy(kani::any());
        let exact = symbolic_policy(kani::any());
        let policy = ToolsetPolicy {
            source: ToolsetSource::Agent,
            default,
            overrides: vec![ToolPolicyOverride::new("search", exact)],
        };

        assert_eq!(policy.policy_for("search"), exact);
        assert_eq!(policy.policy_for("fetch"), default);
    }

    /// @obligation tool_policy.selector_fails_closed_for_unregistered_widened_or_mismatched_calls
    /// @proof-input-debt free_symbol select_tool_policy exact_tool_match
    /// @proof-input-debt free_symbol select_tool_policy registered
    #[kani::proof]
    fn tool_policy_selector_fails_closed_for_unregistered_widened_or_mismatched_calls() {
        // Cause/effect graph over the COMPLETE requirement vocabulary.
        // C1 the tool is registered; C2 the lookup matched exactly; C3 the
        // authored policy is enabled; C4 the request delegates more unattended
        // authority than the author did. E1 admit and carry the request
        // verbatim; E2 refuse.
        //
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // |---|---|---|---|---|---|
        // | S1 | T | T | T | F | E1 |
        // | S2 | F | * | * | * | E2 |
        // | S3 | * | F | * | * | E2 |
        // | S4 | * | * | F | * | E2 |
        // | S5 | T | T | T | T | E2 |
        let registered: bool = kani::any();
        let exact_tool_match: bool = kani::any();
        let enabled: bool = kani::any();
        let authored_permission = symbolic_permission();
        let requested = symbolic_permission();
        let _ = requirement_generator_is_exhaustive(authored_permission);
        let selected = select_tool_policy(
            registered,
            exact_tool_match,
            ToolExecutionPolicy {
                enabled,
                permission: authored_permission,
            },
            requested,
        );
        // Widening is stated once, on the named axis, instead of enumerating
        // the pairs that happen to widen today.
        let widened =
            requested.granted_autonomy().rank() > authored_permission.granted_autonomy().rank();
        let expected = registered && exact_tool_match && enabled && !widened;

        assert_eq!(selected.is_some(), expected);
        if let Some(policy) = selected {
            assert!(policy.enabled);
            assert_eq!(policy.permission, requested);
            assert!(!widened);
        }
    }

    /// @obligation tool_policy.selector_fails_closed_for_unregistered_widened_or_mismatched_calls
    #[kani::proof]
    fn autonomy_is_a_total_order_whose_ceiling_test_is_exactly_rank_comparison() {
        // The admission decision rests entirely on this order, so the order
        // itself is proved rather than assumed: `within` must be exactly `<=`
        // on `rank`, and `rank` must be a total order (reflexive, antisymmetric,
        // transitive, and comparable in both directions).
        let a: GrantedAutonomy = kani::any();
        let b: GrantedAutonomy = kani::any();
        let c: GrantedAutonomy = kani::any();

        assert_eq!(a.within(b), a.rank() <= b.rank());
        assert!(a.within(a), "reflexive");
        if a.within(b) && b.within(a) {
            assert_eq!(a.rank(), b.rank(), "antisymmetric");
        }
        if a.within(b) && b.within(c) {
            assert!(a.within(c), "transitive");
        }
        assert!(a.within(b) || b.within(a), "total");
    }

    /// @obligation tool_policy.selector_fails_closed_for_unregistered_widened_or_mismatched_calls
    #[kani::proof]
    fn every_requirement_sits_at_exactly_one_autonomy_level() {
        // The projection from a policy value to the axis must be total and
        // injective-by-intent: `auto` sits strictly between the two fixed
        // answers, which is the whole reason a single comparison can admit it.
        let requirement = symbolic_permission();
        let level = requirement.granted_autonomy();
        let expected = match requirement {
            ToolPermissionRequirement::AlwaysAsk => GrantedAutonomy::None,
            ToolPermissionRequirement::Auto => GrantedAutonomy::Evaluated,
            ToolPermissionRequirement::AlwaysAllow => GrantedAutonomy::Full,
        };
        assert_eq!(level, expected);
        assert!(
            GrantedAutonomy::None.rank() < GrantedAutonomy::Evaluated.rank()
                && GrantedAutonomy::Evaluated.rank() < GrantedAutonomy::Full.rank(),
            "ask < auto < allow on the unattended-authority axis"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_tool_descriptor_round_trips_without_execution_ownership() {
        // Cause/effect decision table: R1 name+description+open JSON Schema are
        // preserved exactly; R2 no executor, permission, or backend field can be
        // authored because those concerns are absent from the canonical value.
        let descriptor = ClientToolDescriptor {
            name: "client_search".into(),
            description: "Search in the client".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let encoded = serde_json::to_vec(&descriptor).unwrap();
        assert_eq!(
            serde_json::from_slice::<ClientToolDescriptor>(&encoded).unwrap(),
            descriptor
        );
        let object = serde_json::to_value(descriptor).unwrap();
        assert_eq!(object.as_object().unwrap().len(), 3);
    }

    #[test]
    fn registered_tool_policy_selector_is_exact_and_non_widening() {
        let policy = ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAsk,
            },
            overrides: Vec::new(),
        };
        assert!(
            policy
                .policy_for_registered_call(
                    Some("client_lookup"),
                    "client_lookup",
                    ToolPermissionRequirement::AlwaysAsk,
                )
                .is_some()
        );
        assert!(
            policy
                .policy_for_registered_call(
                    None,
                    "client_lookup",
                    ToolPermissionRequirement::AlwaysAsk,
                )
                .is_none(),
            "unregistered"
        );
        assert!(
            policy
                .policy_for_registered_call(
                    Some("client_update"),
                    "client_lookup",
                    ToolPermissionRequirement::AlwaysAsk,
                )
                .is_none(),
            "mismatched identity"
        );
        assert!(
            policy
                .policy_for_registered_call(
                    Some("client_lookup"),
                    "client_lookup",
                    ToolPermissionRequirement::AlwaysAllow,
                )
                .is_none(),
            "authority widening"
        );
    }
}
