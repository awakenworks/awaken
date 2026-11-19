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
        // Join the whole Worker before selecting this Session. Other finished
        // canonical Pod/PVC identities confer no membership or disposal right.
        let scoped = scoped_terminal_tickets(tickets, &self.sandbox_namespace, expected_pod_name)?;
        let claim_name = crate::managed_names::continuation_claim_for_pod(expected_pod_name)?;
        let claim_uid = scoped.iter().find_map(|ticket| match &ticket.status {
            TicketStatus::Finished(TicketResult::Created { uid })
                if ticket.request.resource == "persistentvolumeclaims"
                    && ticket.request.namespace == self.sandbox_namespace
                    && ticket.request.name == claim_name =>
            {
                Some(uid.clone())
            }
            _ => None,
        });
        let Some(claim_uid) = claim_uid else {
            return Ok(RequestJoin::Complete(scoped));
        };
        // An unclassified projected participant must reach its ordinary owner;
        // a claim reservation cannot silently discard its writeback obligation.
        if scoped.iter().any(|ticket| {
            matches!(
                ticket.status,
                TicketStatus::Finished(
                    TicketResult::Created { .. }
                        | TicketResult::Isolated { .. }
                        | TicketResult::CreatedThenIsolated { .. }
                )
            ) && !matches!(
                ticket.request.resource.as_str(),
                "pods" | "persistentvolumeclaims"
            )
        }) {
            return Ok(RequestJoin::Complete(scoped));
        }
        let original_pod = scoped.iter().find(|ticket| {
            ticket.request.resource == "pods"
                && ticket.request.namespace == self.sandbox_namespace
                && ticket.request.name == expected_pod_name
        });
        let mut absent_created_uid = None;
        let request = match original_pod {
            Some(ticket) => match &ticket.status {
                TicketStatus::Finished(TicketResult::Created { uid }) => {
                    // The ticket remembers creation, not continued existence.
                    // Keep a live/foreign original with its ordinary owner. Only
                    // a successful strong 404 admits same-name reisolation.
                    let api: kube::Api<k8s_openapi::api::core::v1::Pod> =
                        kube::Api::namespaced(self.client.clone(), &self.sandbox_namespace);
                    if crate::get_optional(&api, expected_pod_name)
                        .await?
                        .is_some()
                    {
                        return Ok(RequestJoin::Complete(scoped));
                    }
                    if uid.len() > 256 || ticket.isolation_id.is_empty() {
                        return Ok(RequestJoin::Unknown);
                    }
                    absent_created_uid = Some(uid.clone());
                    ticket.request.clone()
                }
                TicketStatus::Finished(
                    TicketResult::Isolated { .. } | TicketResult::CreatedThenIsolated { .. },
                ) => return Ok(RequestJoin::Complete(scoped)),
                TicketStatus::Finished(TicketResult::RejectedNoEffect { .. }) => {
                    ticket.request.clone()
                }
                TicketStatus::InFlight => return Ok(RequestJoin::Unknown),
            },
            None => {
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
                CreateTicketRequest::new(
                    group,
                    "pods",
                    &self.sandbox_namespace,
                    expected_pod_name,
                    digest,
                )?
            }
        };
        let isolation_id = uuid::Uuid::new_v4().simple().to_string();
        self.mutate(binding, |state| {
            let changed = state.admit_terminal_pod_guard(
                &request,
                &isolation_id,
                absent_created_uid.as_deref(),
            )?;
            Ok(((), changed))
        })
        .await?;
        match self.isolate_sealed_group_and_join(binding, group).await? {
            RequestJoin::Complete(tickets) => Ok(RequestJoin::Complete(scoped_terminal_tickets(
                &tickets,
                &self.sandbox_namespace,
                expected_pod_name,
            )?)),
            RequestJoin::Unknown => Ok(RequestJoin::Unknown),
        }
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

/// A projection of one already-complete group, never a second admission store.
fn scoped_terminal_tickets(
    tickets: &[CreateTicket],
    namespace: &str,
    pod: &str,
) -> Result<Vec<CreateTicket>, RuntimeError> {
    let claim = crate::managed_names::continuation_claim_for_pod(pod)?;
    Ok(tickets
        .iter()
        .filter(|ticket| {
            if ticket.request.namespace != namespace {
                return true;
            }
            match ticket.request.resource.as_str() {
                "pods" => ticket.request.name == pod || !ticket.request.name.starts_with("awaken-"),
                "persistentvolumeclaims" => {
                    ticket.request.name == claim
                        || crate::managed_names::pod_for_continuation_claim(&ticket.request.name)
                            .is_none()
                }
                _ => true,
            }
        })
        .cloned()
        .collect())
}
