// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Caller-role declarations reuse the sole Create ledger vocabulary.
// @formal reason: They add no state, permissions or cleanup authority; the existing
// @formal reason: physical adapter and real Broker regressions retain those laws.
//! Privileged effects called by the Coordinator's existing Create broker.
//! Registry and JSON decisions stay with that caller. The implementation owns
//! the original physical ledger and client; this role adds no authority/state.

use crate::{
    CreateReplyProfile, CreateTicket, CreateTicketRequest, K8sLedgerBinding, RequestJoin,
    RuntimeError, TicketAdmission, TicketResult,
};

/// Original raw request stage, not effect classification, retry permission,
/// no-effect evidence or a serialized protocol. Admission may already be durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawCreateFailurePhase {
    RequestBuild,
    Send,
    Capture,
}

/// One caller role on the same original ledger/client cohort. This capability
/// belongs only to trusted Coordinator composition, never the signed Worker.
#[async_trait::async_trait]
pub trait K8sCreateBrokerEffects: Send + Sync {
    fn control_namespace(&self) -> &str;
    fn configuration_fingerprint(&self) -> &str;
    async fn admit_request(
        &self,
        binding: K8sLedgerBinding<'_>,
        request: &CreateTicketRequest,
    ) -> Result<(TicketAdmission, CreateReplyProfile), RuntimeError>;
    async fn ticket_for(
        &self,
        binding: K8sLedgerBinding<'_>,
        request: &CreateTicketRequest,
    ) -> Result<Option<CreateTicket>, RuntimeError>;
    async fn finish_request(
        &self,
        binding: K8sLedgerBinding<'_>,
        request: &CreateTicketRequest,
        result: &TicketResult,
    ) -> Result<(), RuntimeError>;
    async fn seal_group_and_join(
        &self,
        binding: K8sLedgerBinding<'_>,
        group: &str,
    ) -> Result<RequestJoin, RuntimeError>;
    async fn reserve_terminal_claim_pod_and_join(
        &self,
        binding: K8sLedgerBinding<'_>,
        group: &str,
        scope: &str,
        expected_pod_name: &str,
    ) -> Result<RequestJoin, RuntimeError>;
    async fn seal_worker(&self, binding: K8sLedgerBinding<'_>) -> Result<(), RuntimeError>;

    /// Only exact200 captures the whole body. None is a non200 unobservable
    /// readback with zero body polls, never absence/join/cleanup evidence.
    async fn read_original(
        &self,
        path: &str,
        profile: CreateReplyProfile,
    ) -> Result<Option<Vec<u8>>, RawCreateFailurePhase>;
    /// Every status captures completely under the explicit original profile.
    /// JSON, identity, definitive result and finish decisions stay with Broker.
    async fn post_create(
        &self,
        path: &str,
        body: Vec<u8>,
        profile: CreateReplyProfile,
    ) -> Result<(u16, Vec<u8>), RawCreateFailurePhase>;
}
