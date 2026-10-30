// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Closed transport DTOs only; the existing handlers retain authorization and effects.
//! Closed HTTP request DTOs for Worker Session control. No realization policy or effects.
use awaken_dispatch::{RunClaim, WorkerIdentity};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionResumeReq {
    pub(super) claim: RunClaim,
    pub(super) identity: WorkerIdentity,
    pub(super) session_id: String,
    pub(super) session_work_authority:
        Option<awaken_session_application_contract::SessionWorkAuthorityReceipt>,
    pub(super) continuation:
        Option<awaken_session_configuration_contract::resource::SessionRealizationLease>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionCleanupPollReq {
    pub(super) identity: WorkerIdentity,
    pub(super) session_id: String,
    pub(super) lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionCleanupClaimNextReq {
    pub(super) identity: WorkerIdentity,
    pub(super) target: awaken_session_realization_contract::SessionRealizationTarget,
    pub(super) sweep_cursor_version: u8,
    #[serde(default)]
    pub(super) after_session_id: Option<awaken_session_types::SessionId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionPreparationClaimNextReq {
    pub(super) identity: WorkerIdentity,
    pub(super) target: awaken_session_realization_contract::SessionRealizationTarget,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionCleanupPreparationRecordReq {
    pub(super) identity: WorkerIdentity,
    pub(super) lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
    pub(super) preparation: awaken_session_contract::SessionCleanupPreparation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionCleanupPreparationAuthorizeReq {
    pub(super) identity: WorkerIdentity,
    pub(super) effect: awaken_session_contract::SessionTerminalCleanupEffect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionCleanupDisposalAuthorizeReq {
    pub(super) identity: WorkerIdentity,
    pub(super) effect: awaken_session_contract::SessionTerminalCleanupDisposalEffect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionCleanupDisposalRecordReq {
    pub(super) identity: WorkerIdentity,
    pub(super) lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
    pub(super) receipt: awaken_session_contract::SessionCleanupDisposalReceipt,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionRealizationReq<T> {
    pub(super) identity: WorkerIdentity,
    pub(super) command: T,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionEnvironmentIntentReq {
    pub(super) identity: WorkerIdentity,
    pub(super) intent: awaken_session_contract::SessionEnvironmentEffectIntent,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionEnvironmentReceiptReq {
    pub(super) identity: WorkerIdentity,
    pub(super) receipt: awaken_session_contract::SessionEnvironmentReceipt,
}
