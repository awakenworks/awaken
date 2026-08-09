//! Control-owned Session realization phase protocol (ADR-0066 D5).
//!
//! The Session aggregate owns durable desired and lifecycle state. A Runtime
//! projection owns only staged/live effects. These commands let local and remote
//! topology adapters drive the same root-CAS transitions without giving a Worker
//! repository access or creating a second MCP desired-state registry.

use async_trait::async_trait;

use crate::{
    FrozenSessionProjection, McpAttachmentRealizer, McpGenerationRef, McpRealizationReceipt,
    RunError, SessionRealizationLease, StageMcpAttachment,
};

/// Canonical Session-realization lease boundary. A lease is half-open: it is
/// live strictly before its expiry and stale at the exact expiry millisecond.
#[must_use]
pub const fn realization_lease_is_live_at(expires_at_unix_ms: u64, now_unix_ms: u64) -> bool {
    expires_at_unix_ms > now_unix_ms
}

/// Whether an asserted realization remains authorized by the aggregate's
/// current lease. A same-epoch renewal is a monotonic extension of one owner
/// incarnation, so work admitted under the shorter lease may finish while the
/// renewed current lease is live. Owner, incarnation, or epoch replacement
/// still fences the assertion.
#[must_use]
pub fn realization_lease_authorizes(
    current: &SessionRealizationLease,
    asserted: &SessionRealizationLease,
    now_unix_ms: u64,
) -> bool {
    current.owner == asserted.owner
        && current.runtime_incarnation == asserted.runtime_incarnation
        && current.epoch == asserted.epoch
        && current.expires_at_unix_ms >= asserted.expires_at_unix_ms
        && realization_lease_is_live_at(current.expires_at_unix_ms, now_unix_ms)
}

/// Whether the current exact-generation fence is the asserted fence or a
/// monotonic lease extension of it. Logical generation, owner incarnation, and
/// epoch remain immutable; only expiry may advance. Control uses this relation
/// to ask an in-flight phase driver to catch up instead of treating its already
/// authorized predecessor receipt as a conflicting generation.
#[must_use]
pub fn realization_generation_authorizes(
    current: &McpGenerationRef,
    asserted: &McpGenerationRef,
) -> bool {
    current.session_id == asserted.session_id
        && current.attachment_id == asserted.attachment_id
        && current.generation == asserted.generation
        && current.runtime_incarnation == asserted.runtime_incarnation
        && current.lease_epoch == asserted.lease_epoch
        && current.lease_expires_at_unix_ms >= asserted.lease_expires_at_unix_ms
}

/// Opaque Runtime assignment selected outside the Session domain. Worker and
/// local-process identities are mapped to these strings at the authenticated
/// application edge; the aggregate never imports their protocol vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationTarget {
    pub owner: String,
    pub runtime_incarnation: String,
    pub lease_expires_at_unix_ms: u64,
    /// Explicitly extend an existing lease for the same owner/incarnation.
    /// Ordinary create/hot-update commands leave this false, so a later wall
    /// clock alone cannot turn unrelated realization work into a renewal.
    #[serde(default)]
    pub renew_existing_lease: bool,
    /// Explicitly replace a live lease owned by another logical Runtime.
    /// Only a topology edge holding the current execution claim may set this;
    /// ordinary application callers leave it false and therefore cannot steal
    /// a live Session projection. Renewal and reassignment are mutually
    /// exclusive operations.
    #[serde(default)]
    pub reassign_existing_lease: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SessionRealizationAction {
    /// Realize the frozen Environment/Resources and stage every exact MCP
    /// request. `prepare_session` is the exact Environment/Resource work bit;
    /// MCP-only hot updates must not recreate an already-live environment.
    Stage {
        prepare_session: bool,
        #[serde(default)]
        mcp_stages: Vec<StageMcpAttachment>,
    },
    /// Durable activation has committed. Publish and drain only these exact
    /// generations before acknowledging the effect back to Control.
    Publish {
        #[serde(default)]
        publish: Vec<McpGenerationRef>,
        #[serde(default)]
        drain: Vec<McpGenerationRef>,
    },
    /// Every required projection acknowledgement is durable.
    Complete,
}

/// Secret-free next action returned after every successful Control phase.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationDirective {
    pub projection: FrozenSessionProjection,
    pub lease: SessionRealizationLease,
    pub action: SessionRealizationAction,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BeginSessionRealization {
    pub session_id: String,
    pub target: SessionRealizationTarget,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ActivateSessionRealization {
    pub session_id: String,
    pub lease: SessionRealizationLease,
    /// Exact Resource generation prepared by the Stage effect. `None` means
    /// this was an MCP-only phase and must not settle an independently pending
    /// Resource transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_resource_revision: Option<u64>,
    #[serde(default)]
    pub mcp_receipts: Vec<McpRealizationReceipt>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AcknowledgeSessionRealization {
    pub session_id: String,
    pub lease: SessionRealizationLease,
    #[serde(default)]
    pub published: Vec<McpGenerationRef>,
    #[serde(default)]
    pub drained: Vec<McpGenerationRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailSessionRealization {
    pub session_id: String,
    pub lease: SessionRealizationLease,
    /// Exact Resource generation whose preparation failed, when this failure
    /// crossed the Resource effect boundary. MCP-only failures leave pending
    /// Resource work untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_resource_revision: Option<u64>,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionRealizationControlFailure {
    #[error("Session was not found")]
    NotFound,
    #[error("Session is not ready for this realization phase")]
    NotReady,
    #[error("Session realization ownership is stale")]
    StaleOwnership,
    #[error("Session changed concurrently")]
    Conflict,
    #[error("Session realization command is invalid: {0}")]
    Invalid(String),
    #[error("Session realization service is unavailable: {0}")]
    Unavailable(String),
}

/// Driving port for the Control-owned realization state machine. It performs no
/// Runtime I/O; topology adapters execute the returned action and submit exact
/// receipts/acknowledgements to the next phase.
#[async_trait]
pub trait SessionRealizationControl: Send + Sync {
    async fn begin_session_realization(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    async fn activate_session_realization(
        &self,
        command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    async fn acknowledge_session_realization(
        &self,
        command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    async fn fail_session_realization(
        &self,
        command: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure>;
}

/// Topology-specific projection installation used by the one realization
/// driver. Local Managed execution lowers the frozen projection to
/// `SessionRuntime::prepare_session`; a remote Worker installs the same frozen
/// facts into its Host. It owns no lifecycle transition or desired state.
#[async_trait]
pub trait SessionProjectionSynchronizer: Send + Sync {
    async fn synchronize_session_projection(
        &self,
        session_id: &str,
        projection: &FrozenSessionProjection,
        lease: &SessionRealizationLease,
        prepare_session: bool,
    ) -> Result<(), RunError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SessionRealizationDriveError {
    #[error(transparent)]
    Effect(#[from] RunError),
    #[error(transparent)]
    Control(#[from] SessionRealizationControlFailure),
    #[error("Session realization phase protocol did not converge")]
    DidNotConverge,
}

/// Canonical Stage → Activate → Publish/Drain → Acknowledge driver.
///
/// Local and remote topology adapters share this algorithm. They may differ in
/// how a frozen projection is installed and how MCP effects are transported,
/// but cannot acquire a second ordering, cleanup, receipt, or failure path.
pub async fn drive_session_realization(
    session_id: &str,
    control: &dyn SessionRealizationControl,
    synchronizer: &dyn SessionProjectionSynchronizer,
    mcp: &dyn McpAttachmentRealizer,
    mut directive: SessionRealizationDirective,
) -> Result<(), SessionRealizationDriveError> {
    for _ in 0..4 {
        let prepare_session = match &directive.action {
            SessionRealizationAction::Stage {
                prepare_session, ..
            } => *prepare_session,
            SessionRealizationAction::Publish { .. } | SessionRealizationAction::Complete => false,
        };
        if let Err(error) = synchronizer
            .synchronize_session_projection(
                session_id,
                &directive.projection,
                &directive.lease,
                prepare_session,
            )
            .await
        {
            let _ = control
                .fail_session_realization(FailSessionRealization {
                    session_id: session_id.to_string(),
                    lease: directive.lease,
                    prepared_resource_revision: prepare_session
                        .then_some(directive.projection.resource_revision),
                    reason: error.to_string(),
                })
                .await;
            return Err(error.into());
        }
        match directive.action.clone() {
            SessionRealizationAction::Stage {
                prepare_session,
                mcp_stages,
            } => {
                let mut receipts = Vec::with_capacity(mcp_stages.len());
                let effect = async {
                    for request in mcp_stages {
                        let receipt = mcp.stage_mcp_attachment(request.clone()).await?;
                        receipt.verify(&request).map_err(|_| {
                            RunError::classified(
                                "mcp_receipt_mismatch",
                                "Runtime returned a receipt for another MCP realization",
                            )
                        })?;
                        receipts.push(receipt);
                    }
                    Ok::<(), RunError>(())
                }
                .await;
                if let Err(error) = effect {
                    for receipt in &receipts {
                        let _ = mcp.drain_mcp_generation(receipt.generation.clone()).await;
                    }
                    let _ = control
                        .fail_session_realization(FailSessionRealization {
                            session_id: session_id.to_string(),
                            lease: directive.lease,
                            prepared_resource_revision: prepare_session
                                .then_some(directive.projection.resource_revision),
                            reason: error.to_string(),
                        })
                        .await;
                    return Err(error.into());
                }
                directive = match control
                    .activate_session_realization(ActivateSessionRealization {
                        session_id: session_id.to_string(),
                        lease: directive.lease,
                        prepared_resource_revision: prepare_session
                            .then_some(directive.projection.resource_revision),
                        mcp_receipts: receipts.clone(),
                    })
                    .await
                {
                    Ok(next) => next,
                    Err(error) => {
                        for receipt in receipts {
                            let _ = mcp.drain_mcp_generation(receipt.generation).await;
                        }
                        return Err(error.into());
                    }
                };
            }
            SessionRealizationAction::Publish { publish, drain } => {
                let effect = async {
                    for generation in &publish {
                        mcp.publish_mcp_generation(generation.clone()).await?;
                    }
                    for generation in &drain {
                        mcp.drain_mcp_generation(generation.clone()).await?;
                    }
                    Ok::<(), RunError>(())
                }
                .await;
                if let Err(error) = effect {
                    let _ = control
                        .fail_session_realization(FailSessionRealization {
                            session_id: session_id.to_string(),
                            lease: directive.lease,
                            prepared_resource_revision: None,
                            reason: error.to_string(),
                        })
                        .await;
                    return Err(error.into());
                }
                directive = control
                    .acknowledge_session_realization(AcknowledgeSessionRealization {
                        session_id: session_id.to_string(),
                        lease: directive.lease,
                        published: publish,
                        drained: drain,
                    })
                    .await?;
            }
            SessionRealizationAction::Complete => return Ok(()),
        }
        // A Control transition may complete on the final permitted action. Do
        // not require a redundant fifth loop iteration merely to observe the
        // terminal directive after a same-epoch renewal catch-up.
        if matches!(directive.action, SessionRealizationAction::Complete) {
            return Ok(());
        }
    }
    Err(SessionRealizationDriveError::DidNotConverge)
}

/// One application service is installed behind Worker transport. This
/// supertrait prevents contribution and realization from being accidentally
/// wired to different Session authorities.
pub trait ApplicationSessionControl:
    crate::ApplicationSessionContributionApi + SessionRealizationControl
{
}

impl<T> ApplicationSessionControl for T where
    T: crate::ApplicationSessionContributionApi + SessionRealizationControl
{
}

#[cfg(test)]
mod tests {
    use super::{
        realization_generation_authorizes, realization_lease_authorizes,
        realization_lease_is_live_at,
    };
    use crate::{McpAttachmentId, McpGeneration, McpGenerationRef, SessionRealizationLease};

    /// Cause C1: expiry is strictly after observation time. Only C1 authorizes
    /// another effect; equality is already outside the half-open lease.
    ///
    /// | Rule | expiry vs now | live |
    /// |---|---|---|
    /// | L1 | before | false |
    /// | L2 | equal | false |
    /// | L3 | after | true |
    #[test]
    fn realization_lease_boundary_decision_table() {
        for (expiry, now, expected, rule) in [
            (9, 10, false, "L1"),
            (10, 10, false, "L2"),
            (11, 10, true, "L3"),
        ] {
            assert_eq!(
                realization_lease_is_live_at(expiry, now),
                expected,
                "{rule}"
            );
        }
    }

    /// Realization-authorization cause/effect graph: C1 owner/incarnation/epoch
    /// identity is unchanged; C2 current expiry is equal to or later than the
    /// asserted expiry; C3 current lease is live. E1 authorizes the in-flight
    /// phase; otherwise E2 fences it. An asserted lease may itself have elapsed
    /// after admission because a live same-epoch extension owns continuation.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// |---|---|---|---|---|
    /// | A1 | yes | yes | yes | E1 |
    /// | A2 | no | any | yes | E2 |
    /// | A3 | yes | no | yes | E2 |
    /// | A4 | yes | yes | no | E2 |
    #[test]
    fn realization_lease_authorization_decision_table() {
        let asserted = SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 3,
            expires_at_unix_ms: 10,
        };
        for (rule, current, now, expected) in [
            ("A1 exact", asserted.clone(), 9, true),
            (
                "A1 renewed after asserted expiry",
                SessionRealizationLease {
                    expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                11,
                true,
            ),
            (
                "A2 replaced owner",
                SessionRealizationLease {
                    owner: "worker-b".into(),
                    expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                11,
                false,
            ),
            (
                "A3 regressed expiry",
                SessionRealizationLease {
                    expires_at_unix_ms: 9,
                    ..asserted.clone()
                },
                8,
                false,
            ),
            (
                "A4 current expired",
                SessionRealizationLease {
                    expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                20,
                false,
            ),
        ] {
            assert_eq!(
                realization_lease_authorizes(&current, &asserted, now),
                expected,
                "{rule}"
            );
        }
    }

    /// Generation-fence cause/effect graph: C1 logical generation identity,
    /// owner incarnation, and epoch match; C2 current expiry is equal/later.
    /// E1 permits a phase catch-up without committing the predecessor effect;
    /// E2 rejects replacement, regression, or cross-generation input.
    ///
    /// | Rule | C1 | C2 | Effect |
    /// |---|---|---|---|
    /// | G1 exact | yes | equal | E1 |
    /// | G2 renewed | yes | later | E1 |
    /// | G3 regression | yes | earlier | E2 |
    /// | G4 replacement | no | any | E2 |
    #[test]
    fn realization_generation_authorization_decision_table() {
        let asserted = McpGenerationRef {
            session_id: "session-a".into(),
            attachment_id: McpAttachmentId("browser".into()),
            generation: McpGeneration(1),
            runtime_incarnation: "worker-a/boot-1".into(),
            lease_epoch: 3,
            lease_expires_at_unix_ms: 10,
        };
        for (rule, current, expected) in [
            ("G1", asserted.clone(), true),
            (
                "G2",
                McpGenerationRef {
                    lease_expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                true,
            ),
            (
                "G3",
                McpGenerationRef {
                    lease_expires_at_unix_ms: 9,
                    ..asserted.clone()
                },
                false,
            ),
            (
                "G4",
                McpGenerationRef {
                    lease_epoch: 4,
                    lease_expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                false,
            ),
        ] {
            assert_eq!(
                realization_generation_authorizes(&current, &asserted),
                expected,
                "{rule}"
            );
        }
    }
}
