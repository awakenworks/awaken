// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: Original nonce/CAS/UID bodies move onto the same ledger/client.
// @formal reason: Raw capture phases and replay/status regressions preserve the
// @formal reason: existing admission law, not universal physical cleanup closure.
//! The original Broker effects on its sole physical ledger/client.
use super::capture::collect_create_body;
use super::{K8sWorkerCreateLedger, backend};
use awaken_container_contract::{
    CreateReplyProfile, CreateTicket, CreateTicketRequest, K8sCreateBrokerEffects,
    K8sLedgerBinding, RawCreateFailurePhase, RequestJoin, RuntimeError, TicketAdmission,
    TicketResult, TicketStatus,
};
use http::{Method, Request, StatusCode, header};
use kube::client::Body;

#[async_trait::async_trait]
impl K8sCreateBrokerEffects for K8sWorkerCreateLedger {
    fn control_namespace(&self) -> &str {
        &self.control_namespace
    }

    fn configuration_fingerprint(&self) -> &str {
        &self.configuration_fingerprint
    }

    /// Only `Forward` means this exact request won durable admission; the
    /// caller must then own an independent API request lifetime.
    async fn admit_request(
        &self,
        binding: K8sLedgerBinding<'_>,
        request: &CreateTicketRequest,
    ) -> Result<(TicketAdmission, CreateReplyProfile), RuntimeError> {
        // One candidate per caller, reused across resourceVersion CAS retries.
        // A losing caller reads the winner's retained identity and cannot
        // replace it with its own nonce.
        let isolation_id = uuid::Uuid::new_v4().simple().to_string();
        self.mutate(binding, |state| {
            let admission = state.admit(request, &isolation_id)?;
            if state.reply_profile() == CreateReplyProfile::LegacyRetainedCreate
                && admission == TicketAdmission::Forward
            {
                return Err(backend(
                    "historical Create ledger cannot admit a new unreserved request",
                ));
            }
            let changed = admission == TicketAdmission::Forward;
            Ok(((admission, state.reply_profile()), changed))
        })
        .await
    }

    async fn ticket_for(
        &self,
        binding: K8sLedgerBinding<'_>,
        request: &CreateTicketRequest,
    ) -> Result<Option<CreateTicket>, RuntimeError> {
        let (_, state) = self.load_exact(binding).await?;
        Ok(state.ticket(request).cloned())
    }

    async fn finish_request(
        &self,
        binding: K8sLedgerBinding<'_>,
        request: &CreateTicketRequest,
        result: &TicketResult,
    ) -> Result<(), RuntimeError> {
        self.mutate(binding, |state| {
            let changed = state.finish(request, result.clone())?;
            Ok(((), changed))
        })
        .await
    }

    async fn seal_group_and_join(
        &self,
        binding: K8sLedgerBinding<'_>,
        group: &str,
    ) -> Result<RequestJoin, RuntimeError> {
        self.mutate(binding, |state| {
            let changed = state.seal_group(group)?;
            Ok(((), changed))
        })
        .await?;
        let (_, state) = self.load_exact(binding).await?;
        Ok(state.join_group(group))
    }

    /// Retain the original claim's Pod name in this same ledger when an exact
    /// closed Worker created its canonical PVC but no original Pod survives.
    /// The reservation is a physical participant, never a cleanup receipt.
    async fn reserve_terminal_claim_pod_and_join(
        &self,
        binding: K8sLedgerBinding<'_>,
        group: &str,
        scope: &str,
        expected_pod_name: &str,
    ) -> Result<RequestJoin, RuntimeError> {
        let joined = self.isolate_sealed_group_and_join(binding, group).await?;
        let RequestJoin::Complete(tickets) = &joined else {
            return Ok(RequestJoin::Unknown);
        };
        // Runtime owns the realization-namespace projection. Re-encoding raw
        // scope here would target another Pod/PVC name after process loss.
        let claim_name = crate::names::continuation_claim_for_pod(expected_pod_name)?;
        let mut claim_uid = None;
        for ticket in tickets {
            match &ticket.status {
                TicketStatus::Finished(TicketResult::Created { uid })
                    if ticket.request.resource == "persistentvolumeclaims"
                        && ticket.request.namespace == self.sandbox_namespace
                        && ticket.request.name == claim_name
                        && claim_uid.is_none() =>
                {
                    claim_uid = Some(uid.clone());
                }
                TicketStatus::Finished(
                    TicketResult::Created { .. } | TicketResult::Isolated { .. },
                ) => {
                    // The ordinary Pod/PVC owner or an unclassified participant
                    // must decide its own cleanup. Never fabricate a guard for it.
                    return Ok(joined.clone());
                }
                TicketStatus::Finished(TicketResult::RejectedNoEffect { .. }) => {}
                TicketStatus::InFlight => return Ok(RequestJoin::Unknown),
            }
        }
        let Some(claim_uid) = claim_uid else {
            return Ok(joined);
        };
        let digest = blake3::hash(
            &serde_json::to_vec(&(
                "k8s-terminal-claim-pod-v1",
                group,
                scope,
                expected_pod_name,
                &claim_uid,
            ))
            .map_err(backend)?,
        )
        .to_hex()
        .to_string();
        let request = CreateTicketRequest::new(
            group,
            "pods",
            &self.sandbox_namespace,
            expected_pod_name,
            digest,
        )?;
        let isolation_id = uuid::Uuid::new_v4().simple().to_string();
        self.mutate(binding, |state| {
            let changed = state.admit_terminal_pod_guard(&request, &isolation_id)?;
            Ok(((), changed))
        })
        .await?;
        self.isolate_sealed_group_and_join(binding, group).await
    }

    async fn seal_worker(&self, binding: K8sLedgerBinding<'_>) -> Result<(), RuntimeError> {
        self.mutate(binding, |state| Ok(((), state.seal_worker())))
            .await
    }

    async fn read_original(
        &self,
        path: &str,
        profile: CreateReplyProfile,
    ) -> Result<Option<Vec<u8>>, RawCreateFailurePhase> {
        let request = Request::builder()
            .method(Method::GET)
            .uri(path)
            .body(Body::empty())
            .map_err(|_| RawCreateFailurePhase::RequestBuild)?;
        let response = self
            .client
            .send(request)
            .await
            .map_err(|_| RawCreateFailurePhase::Send)?;
        if response.status() != StatusCode::OK {
            return Ok(None);
        }
        let bytes = collect_create_body(response.into_body(), profile)
            .await
            .map_err(|_| RawCreateFailurePhase::Capture)?;
        Ok(Some(bytes))
    }

    async fn post_create(
        &self,
        path: &str,
        body: Vec<u8>,
        profile: CreateReplyProfile,
    ) -> Result<(u16, Vec<u8>), RawCreateFailurePhase> {
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .map_err(|_| RawCreateFailurePhase::RequestBuild)?;
        let response = self
            .client
            .send(request)
            .await
            .map_err(|_| RawCreateFailurePhase::Send)?;
        let status = response.status().as_u16();
        let bytes = collect_create_body(response.into_body(), profile)
            .await
            .map_err(|_| RawCreateFailurePhase::Capture)?;
        Ok((status, bytes))
    }
}
