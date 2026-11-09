// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Each question is current native registered-resource evaluation;
// @formal reason: the Workspace result contributes only an opaque consistency fence.
use crate::RemoteManagementAuthz;
use awaken_agent_config::personal_selection::AgentSelectionOwner;
use awaken_iam_contract::{
    ActionKey, AuthorizationAccess, AuthorizationContext, AuthorizationDecision,
    AuthorizationRequest, NamespaceId, PrincipalRef, ProductId,
    RegisteredResourceAuthorizationRequest, ResourceAuthorizationRequest, ResourceId, ResourceType,
    ScopeRef, WorkspaceId,
};
use sha2::{Digest, Sha256};

pub(super) const RESOURCE_TYPE: &str = "awaken.agent";
pub(super) const NAMESPACE: &str = "awaken.workspace";
pub(super) const USE: &str = "agent.use";
pub(super) const SELECTION_READ: &str = "agent.selection.read";
pub(super) const SELECTION_WRITE: &str = "agent.selection.write";
pub(super) const DEFINITION_READ: &str = "agent.definition.read";

#[derive(Debug, Clone, Copy)]
pub(super) enum Failure {
    Unauthorized,
    Denied,
    Unavailable,
    NotConfigured,
    Invalid,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Person {
    pub owner: AgentSelectionOwner,
    pub principal: PrincipalRef,
}

pub(super) fn exact(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

/// Length-prefixed UTF-8 coordinates prevent separator ambiguity. This identity
/// binds verified native issuer, trusted Workspace and Config id, never a label.
pub(super) fn resource_id(
    issuer: &str,
    workspace: &str,
    agent: &str,
) -> Result<ResourceId, Failure> {
    if !exact(issuer, 2048) || !exact(workspace, 300) || !exact(agent, 300) {
        return Err(Failure::Invalid);
    }
    let mut hash = Sha256::new();
    for value in [issuer, workspace, agent] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    Ok(ResourceId(format!("awaken-agent-v1:{:x}", hash.finalize())))
}

fn action(value: &str) -> ActionKey {
    ActionKey::in_namespace(&NamespaceId(NAMESPACE.into()), value)
}

fn fence(value: Option<String>) -> Result<String, Failure> {
    value
        .filter(|value| {
            value.strip_prefix("iam-fence-v1:").is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        })
        .ok_or(Failure::Unavailable)
}

pub(super) fn person(iam: &RemoteManagementAuthz, token: &str) -> Result<Person, Failure> {
    let (issuer, account) = iam
        .current_agent_governance_identity(token)
        .map_err(|error| match error {
            awaken_iam_host::AuthReject::Unavailable => Failure::Unavailable,
            _ => Failure::Unauthorized,
        })?;
    Ok(Person {
        owner: AgentSelectionOwner::new(issuer, account.0.clone())
            .map_err(|_| Failure::Unavailable)?,
        principal: PrincipalRef::Account {
            account_id: account,
        },
    })
}

/// The decision is deliberately ignored: this observes the native version/epoch
/// around the page and confers no browse, use, definition or preference right.
pub(super) fn observe(
    iam: &RemoteManagementAuthz,
    person: &Person,
    workspace: &str,
) -> Result<String, Failure> {
    observe_principal(iam, &person.principal, workspace)
}
pub(super) fn observe_principal(
    iam: &RemoteManagementAuthz,
    principal: &PrincipalRef,
    workspace: &str,
) -> Result<String, Failure> {
    let result = iam
        .agent_governance_observation(ResourceAuthorizationRequest {
            request: AuthorizationRequest::direct(
                principal.clone(),
                action("workspace.read"),
                ScopeRef::Workspace {
                    workspace_id: WorkspaceId(workspace.into()),
                },
            ),
            context: AuthorizationContext::default(),
        })
        .map_err(|_| Failure::Unavailable)?;
    fence(result.freshness)
}

pub(super) fn allows(
    iam: &RemoteManagementAuthz,
    product: &ProductId,
    person: &Person,
    workspace: &str,
    agent: &str,
    demand: &str,
    expected_fence: &str,
) -> Result<bool, Failure> {
    allows_principal(
        iam,
        product,
        DefinitionSubject {
            issuer: person.owner.issuer(),
            principal: &person.principal,
        },
        workspace,
        agent,
        demand,
        expected_fence,
    )
}
pub(super) struct DefinitionSubject<'a> {
    pub(super) issuer: &'a str,
    pub(super) principal: &'a PrincipalRef,
}
pub(super) fn allows_principal(
    iam: &RemoteManagementAuthz,
    product: &ProductId,
    subject: DefinitionSubject<'_>,
    workspace: &str,
    agent: &str,
    demand: &str,
    expected_fence: &str,
) -> Result<bool, Failure> {
    let id = resource_id(subject.issuer, workspace, agent)?;
    let principal = subject.principal;
    let selection = matches!(demand, SELECTION_READ | SELECTION_WRITE);
    let result = iam
        .agent_governance_authorization(RegisteredResourceAuthorizationRequest {
            authorization: ResourceAuthorizationRequest {
                request: AuthorizationRequest::direct(
                    principal.clone(),
                    action(demand),
                    ScopeRef::Resource {
                        resource_type: ResourceType(RESOURCE_TYPE.into()),
                        resource_id: id.clone(),
                    },
                ),
                context: AuthorizationContext {
                    resource_id: Some(id.0),
                    resource_type: Some(RESOURCE_TYPE.into()),
                    owner: selection.then(|| principal.clone()),
                    access: if matches!(
                        demand,
                        SELECTION_WRITE
                            | "agent.definition.write"
                            | "agent.definition.publish"
                            | "agent.definition.archive"
                    ) {
                        AuthorizationAccess::Write
                    } else {
                        AuthorizationAccess::Read
                    },
                    ..Default::default()
                },
            },
            namespace: NamespaceId(NAMESPACE.into()),
            product_id: product.clone(),
            workspace_id: WorkspaceId(workspace.into()),
            resource_type: ResourceType(RESOURCE_TYPE.into()),
        })
        .map_err(|_| Failure::Unavailable)?;
    if fence(result.evaluation.freshness)? != expected_fence {
        return Err(Failure::Unavailable);
    }
    // The SDK already validates the complete matching proof on every Allow.
    Ok(result.evaluation.outcome.decision == AuthorizationDecision::Allow)
}
