//! Durable MCP attachment generations owned by the Session aggregate (ADR-0066).

use std::collections::{BTreeMap, BTreeSet};

use awaken_credential_contract::{CredentialAccess, CredentialRealizationKind, PlaintextHolder};
use serde::{Deserialize, Serialize};

pub use awaken_agent_contract::{McpTarget, McpTargetError, McpTargetIdentity};

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpSetRevision(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpGeneration(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpAttachmentId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpDesiredSetFingerprint(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAttachmentOrigin {
    Session,
    Application,
    Agent,
}

/// Exact, normalized, secret-free desired attachment. Raw authoring DTOs and
/// material never enter the aggregate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAttachmentDraft {
    pub name: String,
    pub target: McpTarget,
    #[serde(default, skip_serializing_if = "is_false")]
    pub prompts_as_skills: bool,
    pub credential: Option<CredentialAccess>,
    pub origin: McpAttachmentOrigin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRealizationClaim {
    pub realization_id: String,
    pub runtime_incarnation: String,
    pub lease_epoch: u64,
    pub lease_expires_at_unix_ms: u64,
    pub stage_idempotency_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAttachmentState {
    Requested,
    Realizing,
    Active,
    Draining,
    Removed,
    Failed,
}

impl McpAttachmentState {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Removed | Self::Failed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMcpAttachment {
    pub attachment_id: McpAttachmentId,
    pub name: String,
    pub generation: McpGeneration,
    pub target: McpTarget,
    #[serde(default, skip_serializing_if = "is_false")]
    pub prompts_as_skills: bool,
    pub origin: McpAttachmentOrigin,
    pub credential: Option<CredentialAccess>,
    pub selected_plaintext_holder: Option<PlaintextHolder>,
    pub state: McpAttachmentState,
    /// True only after Runtime acknowledged publication for this exact durable
    /// realization claim. Legacy rows default to false and are reconciled.
    #[serde(default)]
    pub publication_acknowledged: bool,
    pub realization: Option<McpRealizationClaim>,
    pub attempts: u32,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMcpAttachmentSet {
    pub revision: McpSetRevision,
    pub desired_fingerprint: McpDesiredSetFingerprint,
    /// Logical names in the canonical desired set. `None` is reserved for
    /// legacy rows written before this field existed; commands always replace
    /// it with `Some`, including `Some(empty)` for an explicit removal-all.
    #[serde(default)]
    pub desired_names: Option<BTreeSet<String>>,
    pub attachments: Vec<SessionMcpAttachment>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpReplacementPlan {
    pub requested: Vec<(McpAttachmentId, McpGeneration)>,
    pub draining: Vec<(McpAttachmentId, McpGeneration)>,
    pub changed: bool,
}

/// Exact identity and continuing ownership fence for one Runtime projection.
/// Runtime adapters must compare the complete value; a logical name is never a
/// sufficient route or cleanup identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpGenerationRef {
    pub session_id: String,
    pub attachment_id: McpAttachmentId,
    pub generation: McpGeneration,
    pub runtime_incarnation: String,
    pub lease_epoch: u64,
    pub lease_expires_at_unix_ms: u64,
}

/// Secret-free command for realizing one already-authorized attachment
/// generation. Credential selection and holder admission have already happened
/// in the Session application layer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageMcpAttachment {
    pub workspace_id: String,
    pub generation: McpGenerationRef,
    pub realization_id: String,
    pub stage_idempotency_key: String,
    pub name: String,
    pub target: McpTarget,
    #[serde(default, skip_serializing_if = "is_false")]
    pub prompts_as_skills: bool,
    pub credential: Option<CredentialAccess>,
    pub selected_plaintext_holder: Option<PlaintextHolder>,
}

impl StageMcpAttachment {
    /// Stable identity of the complete secret-free realization command. Runtime
    /// receipts echo this value so Control can reject a receipt produced for a
    /// different target, credential pin, holder, lease, or generation.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        crate::stable_fingerprint(self)
    }

    /// Stable desired/effect identity across lease renewal. Only the lease
    /// expiry and its per-attempt idempotency key are excluded; target,
    /// credential pin, holder, owner incarnation, epoch, and logical generation
    /// remain fenced. Runtime adapters use this to distinguish an authorized
    /// extension from a conflicting restage of the same generation.
    #[must_use]
    pub fn renewal_binding_fingerprint(&self) -> String {
        if !self.prompts_as_skills {
            return crate::stable_fingerprint(&(
                &self.workspace_id,
                &self.generation.session_id,
                &self.generation.attachment_id,
                self.generation.generation,
                &self.generation.runtime_incarnation,
                self.generation.lease_epoch,
                &self.realization_id,
                &self.name,
                &self.target,
                &self.credential,
                &self.selected_plaintext_holder,
            ));
        }
        crate::stable_fingerprint(&(
            &self.workspace_id,
            &self.generation.session_id,
            &self.generation.attachment_id,
            self.generation.generation,
            &self.generation.runtime_incarnation,
            self.generation.lease_epoch,
            &self.realization_id,
            &self.name,
            &self.target,
            self.prompts_as_skills,
            &self.credential,
            &self.selected_plaintext_holder,
        ))
    }
}

/// Secret-free evidence returned by the Runtime projection boundary. It records
/// the exact effect that was staged, never credential material or a live handle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRealizationReceipt {
    pub generation: McpGenerationRef,
    pub realization_id: String,
    pub selected_plaintext_holder: Option<PlaintextHolder>,
    pub actual_realization_kind: Option<CredentialRealizationKind>,
    pub receipt_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpRealizationReceiptError {
    #[error("MCP realization receipt does not match its exact stage request")]
    Mismatch,
}

impl McpRealizationReceipt {
    /// Verify all command/receipt fences together. Callers must not select a
    /// subset: doing so would let a receipt for another target or credential
    /// authorize this generation merely because its route id happened to match.
    pub fn verify(&self, request: &StageMcpAttachment) -> Result<(), McpRealizationReceiptError> {
        if self.generation == request.generation
            && self.realization_id == request.realization_id
            && self.selected_plaintext_holder == request.selected_plaintext_holder
            && self.receipt_fingerprint == request.fingerprint()
        {
            Ok(())
        } else {
            Err(McpRealizationReceiptError::Mismatch)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpProjectionEffectKind {
    Publish,
    Drain,
}

/// Exact evidence for the externally visible publish/drain phases. Stage uses
/// [`McpRealizationReceipt`]; all phases therefore expose a verifiable receipt
/// before Control advances durable state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpProjectionReceipt {
    pub generation: McpGenerationRef,
    pub effect: McpProjectionEffectKind,
    pub effect_id: String,
}

impl McpProjectionReceipt {
    #[must_use]
    pub fn new(generation: McpGenerationRef, effect: McpProjectionEffectKind) -> Self {
        let effect_id =
            crate::stable_fingerprint(&("mcp-projection-effect-v1", &generation, effect));
        Self {
            generation,
            effect,
            effect_id,
        }
    }

    pub fn verify(
        &self,
        generation: &McpGenerationRef,
        effect: McpProjectionEffectKind,
    ) -> Result<(), McpRealizationReceiptError> {
        if self == &Self::new(generation.clone(), effect) {
            Ok(())
        } else {
            Err(McpRealizationReceiptError::Mismatch)
        }
    }
}

impl Default for SessionMcpAttachmentSet {
    fn default() -> Self {
        Self::from_initial(Vec::new(), None).expect("empty MCP attachment set is valid")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpAttachmentError {
    #[error("MCP attachment name is empty")]
    EmptyName,
    #[error("MCP attachment target is empty")]
    EmptyTarget,
    #[error("duplicate MCP attachment name `{0}`")]
    DuplicateName(String),
    #[error("duplicate MCP attachment target `{0}`")]
    DuplicateTarget(String),
    #[error("credential holder is missing for protected MCP attachment `{0}`")]
    MissingCredentialHolder(String),
    #[error("credential holder is not allowed for MCP attachment `{0}`")]
    CredentialHolderNotAllowed(String),
    #[error("MCP attachment `{0}` was not found")]
    UnknownAttachment(String),
    #[error("MCP generation does not match")]
    StaleGeneration,
    #[error("MCP realization claim does not match")]
    StaleRealizationClaim,
    #[error("invalid MCP attachment transition")]
    InvalidTransition,
    #[error("MCP attachment counter is exhausted")]
    CounterExhausted,
}

/// Resolve all create-time MCP sources through the one documented precedence
/// rule. The returned order is canonical by logical name.
pub(crate) fn resolve_mcp_draft_precedence(
    drafts: Vec<McpAttachmentDraft>,
) -> Result<Vec<McpAttachmentDraft>, McpAttachmentError> {
    fn rank(origin: McpAttachmentOrigin) -> u8 {
        match origin {
            McpAttachmentOrigin::Session => 3,
            McpAttachmentOrigin::Application => 2,
            McpAttachmentOrigin::Agent => 1,
        }
    }

    let mut selected = BTreeMap::<String, McpAttachmentDraft>::new();
    for draft in drafts {
        if draft.name.trim().is_empty() {
            return Err(McpAttachmentError::EmptyName);
        }
        if draft.target.display_target().trim().is_empty() {
            return Err(McpAttachmentError::EmptyTarget);
        }
        match selected.get(&draft.name) {
            None => {
                selected.insert(draft.name.clone(), draft);
            }
            Some(current) if rank(current.origin) == rank(draft.origin) => {
                return Err(McpAttachmentError::DuplicateName(draft.name));
            }
            Some(current) if rank(current.origin) > rank(draft.origin) => {}
            Some(_) => {
                selected.insert(draft.name.clone(), draft);
            }
        }
    }
    let mut targets = BTreeSet::new();
    for draft in selected.values() {
        if !targets.insert(draft.target.fingerprint().to_string()) {
            return Err(McpAttachmentError::DuplicateTarget(
                draft.target.display_target(),
            ));
        }
    }
    Ok(selected.into_values().collect())
}

impl SessionMcpAttachmentSet {
    pub fn from_initial(
        drafts: Vec<McpAttachmentDraft>,
        selected_holder: Option<PlaintextHolder>,
    ) -> Result<Self, McpAttachmentError> {
        validate_drafts(&drafts, selected_holder.as_ref())?;
        let desired_fingerprint = desired_fingerprint(&drafts);
        let attachments: Vec<SessionMcpAttachment> = drafts
            .into_iter()
            .map(|draft| SessionMcpAttachment {
                attachment_id: attachment_id(&draft.name),
                name: draft.name,
                generation: McpGeneration(1),
                target: draft.target,
                prompts_as_skills: draft.prompts_as_skills,
                origin: draft.origin,
                selected_plaintext_holder: draft
                    .credential
                    .as_ref()
                    .map(|_| selected_holder.clone().expect("validated holder")),
                credential: draft.credential,
                state: McpAttachmentState::Requested,
                publication_acknowledged: false,
                realization: None,
                attempts: 0,
                last_error: None,
            })
            .collect();
        Ok(Self {
            revision: McpSetRevision(1),
            desired_fingerprint,
            desired_names: Some(
                attachments
                    .iter()
                    .map(|attachment| attachment.name.clone())
                    .collect(),
            ),
            attachments,
        })
    }

    #[must_use]
    pub fn visible(&self) -> Vec<&SessionMcpAttachment> {
        self.attachments
            .iter()
            .filter(|attachment| attachment.state == McpAttachmentState::Active)
            .collect()
    }

    /// Any nonterminal generation is durable reconciliation work. `Active` is
    /// included because Runtime projections are process-local and must be
    /// restaged/published after ownership replacement.
    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        self.attachments
            .iter()
            .any(|attachment| !attachment.state.is_terminal())
    }

    pub fn claim_realization(
        &mut self,
        attachment_id: &McpAttachmentId,
        generation: McpGeneration,
        claim: McpRealizationClaim,
    ) -> Result<(), McpAttachmentError> {
        let attachment = exact_mut(&mut self.attachments, attachment_id, generation)?;
        if attachment.state != McpAttachmentState::Requested {
            return Err(McpAttachmentError::InvalidTransition);
        }
        attachment.attempts = attachment
            .attempts
            .checked_add(1)
            .ok_or(McpAttachmentError::CounterExhausted)?;
        attachment.state = McpAttachmentState::Realizing;
        attachment.publication_acknowledged = false;
        attachment.realization = Some(claim);
        self.bump_revision()
    }

    /// Fence crash recovery to a new Session lease before any Runtime effect.
    /// Requested generations enter Realizing; already Realizing or Active
    /// generations retain their durable visibility while replacing only the
    /// expired process-local realization claim.
    pub fn claim_recovery(
        &mut self,
        attachment_id: &McpAttachmentId,
        generation: McpGeneration,
        claim: McpRealizationClaim,
    ) -> Result<(), McpAttachmentError> {
        let attachment = exact_mut(&mut self.attachments, attachment_id, generation)?;
        match attachment.state {
            McpAttachmentState::Requested => {
                attachment.state = McpAttachmentState::Realizing;
            }
            McpAttachmentState::Realizing | McpAttachmentState::Active => {}
            McpAttachmentState::Draining
            | McpAttachmentState::Removed
            | McpAttachmentState::Failed => return Err(McpAttachmentError::InvalidTransition),
        }
        attachment.attempts = attachment
            .attempts
            .checked_add(1)
            .ok_or(McpAttachmentError::CounterExhausted)?;
        attachment.realization = Some(claim);
        attachment.publication_acknowledged = false;
        self.bump_revision()
    }

    /// Diff one canonical desired set into durable generation intent. This is a
    /// pure aggregate command: no transport, credential material, or Runtime
    /// effect occurs here.
    pub fn request_full_replacement(
        &mut self,
        drafts: Vec<McpAttachmentDraft>,
        selected_holder: Option<PlaintextHolder>,
    ) -> Result<McpReplacementPlan, McpAttachmentError> {
        validate_drafts(&drafts, selected_holder.as_ref())?;
        let fingerprint = desired_fingerprint(&drafts);
        // A matching fingerprint is idempotent only while every desired exact
        // generation is still recoverable. A Failed generation is terminal;
        // replaying the same desired set must allocate N+1 so an operator can
        // retry without first manufacturing a different target.
        if fingerprint == self.desired_fingerprint
            && drafts.iter().all(|draft| {
                self.attachments.iter().any(|attachment| {
                    attachment.name == draft.name
                        && matches!(
                            attachment.state,
                            McpAttachmentState::Requested
                                | McpAttachmentState::Realizing
                                | McpAttachmentState::Active
                        )
                        && attachment.target == draft.target
                        && attachment.prompts_as_skills == draft.prompts_as_skills
                        && attachment.credential == draft.credential
                        && attachment.origin == draft.origin
                })
            })
        {
            return Ok(McpReplacementPlan::default());
        }

        let desired_names = drafts
            .iter()
            .map(|draft| draft.name.clone())
            .collect::<BTreeSet<_>>();
        let mut plan = McpReplacementPlan {
            changed: true,
            ..Default::default()
        };
        for attachment in &mut self.attachments {
            if !desired_names.contains(&attachment.name) {
                match attachment.state {
                    McpAttachmentState::Active | McpAttachmentState::Realizing => {
                        attachment.state = McpAttachmentState::Draining;
                        plan.draining
                            .push((attachment.attachment_id.clone(), attachment.generation));
                    }
                    McpAttachmentState::Requested => {
                        attachment.state = McpAttachmentState::Removed;
                    }
                    McpAttachmentState::Draining
                    | McpAttachmentState::Removed
                    | McpAttachmentState::Failed => {}
                }
            }
        }

        for draft in drafts {
            let matching_nonterminal = self.attachments.iter().any(|attachment| {
                attachment.name == draft.name
                    && !attachment.state.is_terminal()
                    && attachment.state != McpAttachmentState::Draining
                    && attachment.target == draft.target
                    && attachment.prompts_as_skills == draft.prompts_as_skills
                    && attachment.credential == draft.credential
                    && attachment.origin == draft.origin
            });
            if matching_nonterminal {
                continue;
            }
            let attachment_id = self
                .attachments
                .iter()
                .find(|attachment| attachment.name == draft.name)
                .map_or_else(
                    || attachment_id(&draft.name),
                    |item| item.attachment_id.clone(),
                );
            let generation = self
                .attachments
                .iter()
                .filter(|attachment| attachment.attachment_id == attachment_id)
                .map(|attachment| attachment.generation.0)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .map(McpGeneration)
                .ok_or(McpAttachmentError::CounterExhausted)?;
            let selected_plaintext_holder = draft
                .credential
                .as_ref()
                .map(|_| selected_holder.clone().expect("validated holder"));
            self.attachments.push(SessionMcpAttachment {
                attachment_id: attachment_id.clone(),
                name: draft.name,
                generation,
                target: draft.target,
                prompts_as_skills: draft.prompts_as_skills,
                origin: draft.origin,
                credential: draft.credential,
                selected_plaintext_holder,
                state: McpAttachmentState::Requested,
                publication_acknowledged: false,
                realization: None,
                attempts: 0,
                last_error: None,
            });
            plan.requested.push((attachment_id, generation));
        }
        // A set-wide replacement does not hide removals while another desired
        // generation is still staging. Persist the future drain intent through
        // `desired_names`; the activation switch applies it atomically after all
        // requested generations have receipts. A pure removal can drain now.
        if !plan.requested.is_empty() {
            for (attachment_id, generation) in &plan.draining {
                let attachment = exact_mut(&mut self.attachments, attachment_id, *generation)?;
                attachment.state = McpAttachmentState::Active;
            }
        }
        self.desired_fingerprint = fingerprint;
        self.desired_names = Some(desired_names);
        self.bump_revision()?;
        Ok(plan)
    }

    /// Apply the removal half of a staged full-replacement at the same durable
    /// switch that activates its requested generations.
    pub fn begin_obsolete_drains(&mut self) -> Result<(), McpAttachmentError> {
        let Some(desired_names) = &self.desired_names else {
            return Ok(());
        };
        let mut changed = false;
        for attachment in &mut self.attachments {
            if attachment.state == McpAttachmentState::Active
                && !desired_names.contains(&attachment.name)
            {
                attachment.state = McpAttachmentState::Draining;
                changed = true;
            }
        }
        if changed {
            self.bump_revision()?;
        }
        Ok(())
    }

    pub fn activate(
        &mut self,
        attachment_id: &McpAttachmentId,
        generation: McpGeneration,
        realization_id: &str,
    ) -> Result<(), McpAttachmentError> {
        let index = exact_index(&self.attachments, attachment_id, generation)?;
        let attachment = &self.attachments[index];
        if attachment.state != McpAttachmentState::Realizing {
            return Err(McpAttachmentError::InvalidTransition);
        }
        if attachment
            .realization
            .as_ref()
            .is_none_or(|claim| claim.realization_id != realization_id)
        {
            return Err(McpAttachmentError::StaleRealizationClaim);
        }
        for current in &mut self.attachments {
            if current.attachment_id == *attachment_id
                && current.state == McpAttachmentState::Active
            {
                current.state = McpAttachmentState::Draining;
            }
        }
        self.attachments[index].state = McpAttachmentState::Active;
        self.attachments[index].publication_acknowledged = false;
        self.bump_revision()
    }

    pub fn acknowledge_publication(
        &mut self,
        attachment_id: &McpAttachmentId,
        generation: McpGeneration,
        realization_id: &str,
    ) -> Result<(), McpAttachmentError> {
        let attachment = exact_mut(&mut self.attachments, attachment_id, generation)?;
        if attachment.state != McpAttachmentState::Active {
            return Err(McpAttachmentError::InvalidTransition);
        }
        if attachment
            .realization
            .as_ref()
            .is_none_or(|claim| claim.realization_id != realization_id)
        {
            return Err(McpAttachmentError::StaleRealizationClaim);
        }
        if attachment.publication_acknowledged {
            return Ok(());
        }
        attachment.publication_acknowledged = true;
        self.bump_revision()
    }

    /// Extend every active projection owned by the same realization lease.
    ///
    /// Renewal deliberately reuses the existing generation and realization id:
    /// it changes neither desired MCP state nor credential selection. Clearing
    /// `publication_acknowledged` makes the sole realization phase protocol
    /// restage and republish the extended exact fence before it reports
    /// completion. Replaying the same expiry is an idempotent no-op, which lets
    /// activation converge after a concurrent aggregate lease renewal without
    /// inventing a second renewal registry.
    pub fn renew_active_realizations(
        &mut self,
        runtime_incarnation: &str,
        lease_epoch: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<usize, McpAttachmentError> {
        let active = self
            .attachments
            .iter()
            .filter(|attachment| attachment.state == McpAttachmentState::Active)
            .collect::<Vec<_>>();
        for attachment in &active {
            let claim = attachment
                .realization
                .as_ref()
                .ok_or(McpAttachmentError::StaleRealizationClaim)?;
            if claim.runtime_incarnation != runtime_incarnation
                || claim.lease_epoch != lease_epoch
                || lease_expires_at_unix_ms < claim.lease_expires_at_unix_ms
            {
                return Err(McpAttachmentError::StaleRealizationClaim);
            }
        }
        let mut renewed = 0;
        for attachment in &mut self.attachments {
            if attachment.state != McpAttachmentState::Active {
                continue;
            }
            let claim = attachment
                .realization
                .as_mut()
                .expect("all active claims were validated");
            if claim.lease_expires_at_unix_ms == lease_expires_at_unix_ms {
                continue;
            }
            claim.lease_expires_at_unix_ms = lease_expires_at_unix_ms;
            claim.stage_idempotency_key = format!(
                "renew:{}:{}:{}:{}:{}",
                attachment.attachment_id.0,
                attachment.generation.0,
                runtime_incarnation,
                lease_epoch,
                lease_expires_at_unix_ms
            );
            attachment.publication_acknowledged = false;
            renewed += 1;
        }
        if renewed > 0 {
            self.bump_revision()?;
        }
        Ok(renewed)
    }

    pub fn fail_realization(
        &mut self,
        attachment_id: &McpAttachmentId,
        generation: McpGeneration,
        realization_id: &str,
        error: impl Into<String>,
    ) -> Result<(), McpAttachmentError> {
        let attachment = exact_mut(&mut self.attachments, attachment_id, generation)?;
        if attachment.state != McpAttachmentState::Realizing {
            return Err(McpAttachmentError::InvalidTransition);
        }
        if attachment
            .realization
            .as_ref()
            .is_none_or(|claim| claim.realization_id != realization_id)
        {
            return Err(McpAttachmentError::StaleRealizationClaim);
        }
        attachment.state = McpAttachmentState::Failed;
        attachment.last_error = Some(error.into());
        self.bump_revision()
    }

    pub fn begin_drain(
        &mut self,
        attachment_id: &McpAttachmentId,
        generation: McpGeneration,
    ) -> Result<(), McpAttachmentError> {
        let attachment = exact_mut(&mut self.attachments, attachment_id, generation)?;
        if attachment.state != McpAttachmentState::Active {
            return Err(McpAttachmentError::InvalidTransition);
        }
        attachment.state = McpAttachmentState::Draining;
        self.bump_revision()
    }

    pub fn finish_drain(
        &mut self,
        attachment_id: &McpAttachmentId,
        generation: McpGeneration,
    ) -> Result<(), McpAttachmentError> {
        let attachment = exact_mut(&mut self.attachments, attachment_id, generation)?;
        if attachment.state != McpAttachmentState::Draining {
            return Err(McpAttachmentError::InvalidTransition);
        }
        attachment.state = McpAttachmentState::Removed;
        // Keep the secret-free claim as terminal acknowledgement evidence. It
        // cannot restore visibility or a live handle, but it lets an at-least-once
        // drain acknowledgement be verified after response loss.
        self.bump_revision()
    }

    fn bump_revision(&mut self) -> Result<(), McpAttachmentError> {
        self.revision.0 = self
            .revision
            .0
            .checked_add(1)
            .ok_or(McpAttachmentError::CounterExhausted)?;
        Ok(())
    }
}

fn validate_drafts(
    drafts: &[McpAttachmentDraft],
    selected_holder: Option<&PlaintextHolder>,
) -> Result<(), McpAttachmentError> {
    let mut names = BTreeSet::new();
    let mut targets = BTreeSet::new();
    for draft in drafts {
        if draft.name.trim().is_empty() {
            return Err(McpAttachmentError::EmptyName);
        }
        if draft.target.display_target().trim().is_empty() {
            return Err(McpAttachmentError::EmptyTarget);
        }
        if !names.insert(draft.name.clone()) {
            return Err(McpAttachmentError::DuplicateName(draft.name.clone()));
        }
        if !targets.insert(draft.target.fingerprint().to_string()) {
            return Err(McpAttachmentError::DuplicateTarget(
                draft.target.display_target(),
            ));
        }
        if let Some(access) = &draft.credential {
            let holder = selected_holder
                .ok_or_else(|| McpAttachmentError::MissingCredentialHolder(draft.name.clone()))?;
            if !access.policy.allowed_plaintext_holders.contains(holder) {
                return Err(McpAttachmentError::CredentialHolderNotAllowed(
                    draft.name.clone(),
                ));
            }
        }
    }
    Ok(())
}

fn desired_fingerprint(drafts: &[McpAttachmentDraft]) -> McpDesiredSetFingerprint {
    if drafts.iter().all(|draft| !draft.prompts_as_skills) {
        let legacy = drafts
            .iter()
            .map(|draft| {
                (
                    draft.name.as_str(),
                    (&draft.target, &draft.credential, draft.origin),
                )
            })
            .collect::<BTreeMap<_, _>>();
        return McpDesiredSetFingerprint(crate::stable_fingerprint(&legacy));
    }
    let ordered = drafts
        .iter()
        .map(|draft| {
            (
                draft.name.as_str(),
                (
                    &draft.target,
                    draft.prompts_as_skills,
                    &draft.credential,
                    draft.origin,
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    McpDesiredSetFingerprint(crate::stable_fingerprint(&ordered))
}

fn attachment_id(name: &str) -> McpAttachmentId {
    McpAttachmentId(format!("mcp_{}", crate::stable_fingerprint(&name)))
}

fn exact_index(
    attachments: &[SessionMcpAttachment],
    attachment_id: &McpAttachmentId,
    generation: McpGeneration,
) -> Result<usize, McpAttachmentError> {
    if let Some(index) = attachments
        .iter()
        .position(|item| item.attachment_id == *attachment_id && item.generation == generation)
    {
        return Ok(index);
    }
    if attachments
        .iter()
        .any(|item| item.attachment_id == *attachment_id)
    {
        Err(McpAttachmentError::StaleGeneration)
    } else {
        Err(McpAttachmentError::UnknownAttachment(
            attachment_id.0.clone(),
        ))
    }
}

fn exact_mut<'a>(
    attachments: &'a mut [SessionMcpAttachment],
    attachment_id: &McpAttachmentId,
    generation: McpGeneration,
) -> Result<&'a mut SessionMcpAttachment, McpAttachmentError> {
    let index = exact_index(attachments, attachment_id, generation)?;
    Ok(&mut attachments[index])
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_contract::{
        CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef, CredentialUsage,
        ModelExposurePolicy, PlaintextBoundary,
    };

    #[test]
    fn target_identity_cases_follow_the_decision_table() {
        // Cause graph: absolute HTTP(S) + no userinfo/fragment -> canonical
        // identity; scheme/host case, default port and trailing slash disappear;
        // path/query/non-default-port changes remain; every invalid cause rejects.
        //
        // | Rule | HTTP(S) | userinfo/fragment | cosmetic-only delta | Effect |
        // |---|---|---|---|---|
        // | U1 | T | F | T | equal identity/fingerprint |
        // | U2 | T | F | F | different identity/fingerprint |
        // | U3 | F | - | - | reject |
        // | U4 | T | T | - | reject |
        let canonical = McpTarget::parse_http("https://mcp.example.test/sse").unwrap();
        let cosmetic = McpTarget::parse_http("HTTPS://MCP.EXAMPLE.TEST:443/sse/").unwrap();
        assert_eq!(
            McpTarget::identity(canonical.http_url().unwrap()).unwrap(),
            McpTarget::identity(cosmetic.http_url().unwrap()).unwrap(),
            "U1"
        );
        assert_eq!(canonical.fingerprint(), cosmetic.fingerprint(), "U1");
        let different = McpTarget::parse_http("https://mcp.example.test:8443/sse").unwrap();
        assert_ne!(canonical.fingerprint(), different.fingerprint(), "U2");
        assert!(McpTarget::parse_http("file:///tmp/mcp").is_err(), "U3");
        for invalid in [
            "https://user:secret@mcp.example.test/sse",
            "https://mcp.example.test/sse#fragment",
        ] {
            assert!(McpTarget::parse_http(invalid).is_err(), "U4: {invalid}");
        }
    }

    #[test]
    fn realization_receipt_cases_are_generated_from_the_decision_table() {
        // Cause graph: exact generation AND realization id AND selected holder
        // AND complete stage-request fingerprint -> accept. A false value on any
        // edge rejects the receipt; no partial identity subset is authoritative.
        //
        // | Rule | generation | realization | holder | request fingerprint | Effect |
        // |---|---|---|---|---|---|
        // | P1 | exact | exact | exact | exact | accept |
        // | P2 | other | exact | exact | exact | reject |
        // | P3 | exact | other | exact | exact | reject |
        // | P4 | exact | exact | other | exact | reject |
        // | P5 | exact | exact | exact | other | reject |
        let selected = holder("worker-a");
        let request = StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: McpGenerationRef {
                session_id: "session-a".into(),
                attachment_id: McpAttachmentId("mcp-docs".into()),
                generation: McpGeneration(3),
                runtime_incarnation: "runtime-a".into(),
                lease_epoch: 7,
                lease_expires_at_unix_ms: u64::MAX,
            },
            realization_id: "realization-a".into(),
            stage_idempotency_key: "stage-a".into(),
            name: "docs".into(),
            target: McpTarget::parse_http("https://mcp.example.test").unwrap(),
            credential: None,
            prompts_as_skills: false,
            selected_plaintext_holder: Some(selected.clone()),
        };
        let exact = McpRealizationReceipt {
            generation: request.generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: request.selected_plaintext_holder.clone(),
            actual_realization_kind: None,
            receipt_fingerprint: request.fingerprint(),
        };
        for (rule, mutation, accepted) in [
            ("P1", 0_u8, true),
            ("P2", 1, false),
            ("P3", 2, false),
            ("P4", 3, false),
            ("P5", 4, false),
        ] {
            let mut receipt = exact.clone();
            match mutation {
                0 => {}
                1 => receipt.generation.generation = McpGeneration(4),
                2 => receipt.realization_id = "realization-b".into(),
                3 => receipt.selected_plaintext_holder = Some(holder("worker-b")),
                4 => receipt.receipt_fingerprint = "another-request".into(),
                _ => unreachable!(),
            }
            assert_eq!(receipt.verify(&request).is_ok(), accepted, "{rule}");
        }
    }

    fn holder(domain: &str) -> PlaintextHolder {
        PlaintextHolder::new(PlaintextBoundary::Worker, domain)
    }

    fn credential(allowed: PlaintextHolder) -> CredentialAccess {
        CredentialAccess::new(
            CredentialRef {
                id: "credential-1".into(),
                revision: 3,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            CredentialExecutionPolicy::exact(allowed, ModelExposurePolicy::VirtualOnly),
        )
    }

    fn draft(name: &str, url: &str, credential: Option<CredentialAccess>) -> McpAttachmentDraft {
        McpAttachmentDraft {
            name: name.into(),
            target: McpTarget::parse_http(url).unwrap(),
            prompts_as_skills: false,
            credential,
            origin: McpAttachmentOrigin::Session,
        }
    }

    fn origin_draft(name: &str, url: &str, origin: McpAttachmentOrigin) -> McpAttachmentDraft {
        McpAttachmentDraft {
            name: name.into(),
            target: McpTarget::parse_http(url).unwrap(),
            prompts_as_skills: false,
            credential: None,
            origin,
        }
    }

    #[test]
    fn create_time_mcp_precedence_cases_follow_the_decision_table() {
        // Cause graph: candidates first compete by exact logical name using
        // Session > Application > Agent; the selected set then requires unique
        // canonical targets. Equal-rank duplicate names never use order as a
        // hidden tie-breaker.
        //
        // | Rule | Same name sources | Selected | Target collision | Effect |
        // |---|---|---|---|---|
        // | P1 | Session+Application+Agent | Session | no | success |
        // | P2 | Application+Agent | Application | no | success |
        // | P3 | same rank twice | - | no | DuplicateName |
        // | P4 | different names | both | yes | DuplicateTarget |
        // | P5 | distinct names/targets | both | no | canonical name order |
        let p1 = resolve_mcp_draft_precedence(vec![
            origin_draft("calc", "https://agent", McpAttachmentOrigin::Agent),
            origin_draft(
                "calc",
                "https://application",
                McpAttachmentOrigin::Application,
            ),
            origin_draft("calc", "https://session", McpAttachmentOrigin::Session),
        ])
        .unwrap();
        assert_eq!(p1.len(), 1, "P1");
        assert_eq!(p1[0].target.http_url(), Some("https://session"), "P1");

        let p2 = resolve_mcp_draft_precedence(vec![
            origin_draft("calc", "https://agent", McpAttachmentOrigin::Agent),
            origin_draft(
                "calc",
                "https://application",
                McpAttachmentOrigin::Application,
            ),
        ])
        .unwrap();
        assert_eq!(p2[0].target.http_url(), Some("https://application"), "P2");

        assert!(
            matches!(
                resolve_mcp_draft_precedence(vec![
                    origin_draft("calc", "https://a", McpAttachmentOrigin::Agent),
                    origin_draft("calc", "https://b", McpAttachmentOrigin::Agent),
                ]),
                Err(McpAttachmentError::DuplicateName(name)) if name == "calc"
            ),
            "P3"
        );

        assert!(
            matches!(
                resolve_mcp_draft_precedence(vec![
                    origin_draft("a", "https://same", McpAttachmentOrigin::Session),
                    origin_draft("b", "https://same", McpAttachmentOrigin::Application),
                ]),
                Err(McpAttachmentError::DuplicateTarget(_))
            ),
            "P4"
        );

        let p5 = resolve_mcp_draft_precedence(vec![
            origin_draft("z", "https://z", McpAttachmentOrigin::Agent),
            origin_draft("a", "https://a", McpAttachmentOrigin::Application),
        ])
        .unwrap();
        assert_eq!(
            p5.iter()
                .map(|draft| draft.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "z"],
            "P5"
        );
    }

    #[test]
    fn initial_attachment_tests_are_generated_from_decision_table() {
        // Causal graph:
        // valid identity -> unique name -> unique normalized target
        // -> protected? exact selected holder required and allowed
        // -> E1 generation-1 Requested; the first failed cause yields E2.
        //
        // | Rule | identity | name unique | target unique | protected | holder present | holder allowed | Effect |
        // |---|---|---|---|---|---|---|---|
        // | I1 | T | T | T | F | - | - | Requested |
        // | I2 | T | T | T | T | T | T | Requested + holder |
        // | I3 | F | - | - | - | - | - | reject identity |
        // | I4 | T | F | - | - | - | - | reject name |
        // | I5 | T | T | F | - | - | - | reject target |
        // | I6 | T | T | T | T | F | - | reject missing holder |
        // | I7 | T | T | T | T | T | F | reject forbidden holder |
        let worker = holder("worker-a");
        let rules = [
            (
                "I1",
                vec![draft("a", "https://a.test/mcp", None)],
                None,
                Ok(false),
            ),
            (
                "I2",
                vec![draft(
                    "a",
                    "https://a.test/mcp",
                    Some(credential(worker.clone())),
                )],
                Some(worker.clone()),
                Ok(true),
            ),
            (
                "I3",
                vec![draft("", "https://a.test/mcp", None)],
                None,
                Err(McpAttachmentError::EmptyName),
            ),
            (
                "I4",
                vec![
                    draft("a", "https://a.test/one", None),
                    draft("a", "https://b.test/two", None),
                ],
                None,
                Err(McpAttachmentError::DuplicateName("a".into())),
            ),
            (
                "I5",
                vec![
                    draft("a", "https://a.test/mcp", None),
                    draft("b", "https://a.test/mcp", None),
                ],
                None,
                Err(McpAttachmentError::DuplicateTarget(
                    "https://a.test/mcp".into(),
                )),
            ),
            (
                "I6",
                vec![draft(
                    "a",
                    "https://a.test/mcp",
                    Some(credential(worker.clone())),
                )],
                None,
                Err(McpAttachmentError::MissingCredentialHolder("a".into())),
            ),
            (
                "I7",
                vec![draft(
                    "a",
                    "https://a.test/mcp",
                    Some(credential(worker.clone())),
                )],
                Some(holder("worker-b")),
                Err(McpAttachmentError::CredentialHolderNotAllowed("a".into())),
            ),
        ];
        for (id, drafts, selected, expected) in rules {
            let actual = SessionMcpAttachmentSet::from_initial(drafts, selected)
                .map(|set| set.attachments[0].selected_plaintext_holder.is_some());
            assert_eq!(actual, expected, "{id}");
        }
    }

    #[test]
    fn lifecycle_tests_are_generated_from_decision_table() {
        // Causal graph:
        // exact id+generation -> command is valid for current state
        // -> realization id matches where required -> one durable next state.
        // A failed cause leaves the attachment unchanged.
        //
        // | Rule | Current | Command | exact generation | exact claim | Effect |
        // |---|---|---|---|---|---|
        // | L1 | Requested | claim | T | - | Realizing |
        // | L2 | Realizing | activate | T | T | Active |
        // | L3 | Realizing | fail | T | T | Failed |
        // | L4 | Active | drain | T | - | Draining |
        // | L5 | Draining | finish | T | - | Removed |
        // | L6 | any | command | F | - | stale/no mutation |
        // | L7 | Realizing | activate/fail | T | F | stale/no mutation |
        // | L8 | terminal/other | command | T | - | invalid/no mutation |
        // | L9 | Active | publish ack | T | T | acknowledged |
        // | L10 | Active | publish ack replay | T | T | no-op |
        // | L11 | Active | publish ack | T | F | stale/no mutation |
        let mut set = SessionMcpAttachmentSet::from_initial(
            vec![draft("a", "https://a.test/mcp", None)],
            None,
        )
        .unwrap();
        let id = set.attachments[0].attachment_id.clone();
        let claim = McpRealizationClaim {
            realization_id: "realization-1".into(),
            runtime_incarnation: "runtime-1".into(),
            lease_epoch: 4,
            lease_expires_at_unix_ms: 100,
            stage_idempotency_key: "stage-1".into(),
        };
        set.claim_realization(&id, McpGeneration(1), claim)
            .expect("L1");
        assert_eq!(set.attachments[0].state, McpAttachmentState::Realizing);

        let before = set.clone();
        assert_eq!(
            set.activate(&id, McpGeneration(2), "realization-1"),
            Err(McpAttachmentError::StaleGeneration),
            "L6"
        );
        assert_eq!(set, before, "L6 no mutation");
        assert_eq!(
            set.activate(&id, McpGeneration(1), "foreign"),
            Err(McpAttachmentError::StaleRealizationClaim),
            "L7"
        );
        assert_eq!(set, before, "L7 no mutation");

        let mut failed = set.clone();
        failed
            .fail_realization(&id, McpGeneration(1), "realization-1", "dial failed")
            .expect("L3");
        assert_eq!(failed.attachments[0].state, McpAttachmentState::Failed);
        assert_eq!(
            failed.begin_drain(&id, McpGeneration(1)),
            Err(McpAttachmentError::InvalidTransition),
            "L8"
        );

        set.activate(&id, McpGeneration(1), "realization-1")
            .expect("L2");
        assert_eq!(set.attachments[0].state, McpAttachmentState::Active);
        let before_ack = set.clone();
        assert_eq!(
            set.acknowledge_publication(&id, McpGeneration(1), "foreign"),
            Err(McpAttachmentError::StaleRealizationClaim),
            "L11"
        );
        assert_eq!(set, before_ack, "L11 no mutation");
        set.acknowledge_publication(&id, McpGeneration(1), "realization-1")
            .expect("L9");
        assert!(set.attachments[0].publication_acknowledged, "L9");
        let revision = set.revision;
        set.acknowledge_publication(&id, McpGeneration(1), "realization-1")
            .expect("L10");
        assert_eq!(set.revision, revision, "L10");
        set.begin_drain(&id, McpGeneration(1)).expect("L4");
        assert_eq!(set.attachments[0].state, McpAttachmentState::Draining);
        set.finish_drain(&id, McpGeneration(1)).expect("L5");
        assert_eq!(set.attachments[0].state, McpAttachmentState::Removed);
    }

    #[test]
    fn active_realization_renewal_cases_follow_the_decision_table() {
        // Cause graph: Active AND exact incarnation AND exact epoch AND later
        // expiry -> extend the existing claim and require republication. Any
        // false fence fails without mutation; non-active generations are not a
        // renewal target and remain unchanged.
        //
        // | Rule | State | Incarnation | Epoch | Expiry | Effect |
        // |---|---|---|---|---|---|
        // | N1 | Active | exact | exact | later | extend + unacknowledged |
        // | N2 | Active | other | exact | later | reject/no mutation |
        // | N3 | Active | exact | other | later | reject/no mutation |
        // | N4 | Active | exact | exact | same | idempotent/no mutation |
        // | N5 | Active | exact | exact | earlier | reject/no mutation |
        // | N6 | Requested | exact | exact | later | unchanged |
        let active = || {
            let mut set = SessionMcpAttachmentSet::from_initial(
                vec![draft("a", "https://a.test/mcp", None)],
                None,
            )
            .unwrap();
            let id = set.attachments[0].attachment_id.clone();
            set.claim_realization(
                &id,
                McpGeneration(1),
                McpRealizationClaim {
                    realization_id: "realization-1".into(),
                    runtime_incarnation: "runtime-1".into(),
                    lease_epoch: 4,
                    lease_expires_at_unix_ms: 100,
                    stage_idempotency_key: "stage-1".into(),
                },
            )
            .unwrap();
            set.activate(&id, McpGeneration(1), "realization-1")
                .unwrap();
            set.acknowledge_publication(&id, McpGeneration(1), "realization-1")
                .unwrap();
            set
        };

        let mut renewed = active();
        assert_eq!(
            renewed
                .renew_active_realizations("runtime-1", 4, 200)
                .unwrap(),
            1,
            "N1"
        );
        let claim = renewed.attachments[0].realization.as_ref().unwrap();
        assert_eq!(claim.lease_expires_at_unix_ms, 200, "N1");
        assert!(claim.stage_idempotency_key.starts_with("renew:"), "N1");
        assert!(!renewed.attachments[0].publication_acknowledged, "N1");

        for (rule, incarnation, epoch, expiry) in [
            ("N2", "runtime-2", 4, 200),
            ("N3", "runtime-1", 5, 200),
            ("N5", "runtime-1", 4, 99),
        ] {
            let mut set = active();
            let before = set.clone();
            assert_eq!(
                set.renew_active_realizations(incarnation, epoch, expiry),
                Err(McpAttachmentError::StaleRealizationClaim),
                "{rule}"
            );
            assert_eq!(set, before, "{rule} no mutation");
        }

        let mut idempotent = active();
        let before = idempotent.clone();
        assert_eq!(
            idempotent
                .renew_active_realizations("runtime-1", 4, 100)
                .unwrap(),
            0,
            "N4"
        );
        assert_eq!(idempotent, before, "N4 no mutation");

        let mut requested = SessionMcpAttachmentSet::from_initial(
            vec![draft("a", "https://a.test/mcp", None)],
            None,
        )
        .unwrap();
        let before = requested.clone();
        assert_eq!(
            requested
                .renew_active_realizations("runtime-1", 4, 200)
                .unwrap(),
            0,
            "N6"
        );
        assert_eq!(requested, before, "N6");
    }

    #[test]
    fn recovery_claim_tests_are_generated_from_decision_table() {
        // Cause graph: exact generation -> recoverable nonterminal state
        // -> replace the expired Runtime claim under a new lease. Requested also
        // enters Realizing; visible Active remains visible until republished.
        //
        // | Rule | exact generation | current | Effect |
        // |---|---|---|---|
        // | R1 | T | Requested | Realizing + new claim |
        // | R2 | T | Realizing | Realizing + new claim |
        // | R3 | T | Active | Active + new claim |
        // | R4 | T | Draining | invalid / unchanged |
        // | R5 | T | Removed | invalid / unchanged |
        // | R6 | T | Failed | invalid / unchanged |
        // | R7 | F | any | stale / unchanged |
        let claim = |id: &str| McpRealizationClaim {
            realization_id: id.into(),
            runtime_incarnation: "runtime-2".into(),
            lease_epoch: 9,
            lease_expires_at_unix_ms: 200,
            stage_idempotency_key: format!("stage-{id}"),
        };
        for (rule, state, expected) in [
            (
                "R1",
                McpAttachmentState::Requested,
                Ok(McpAttachmentState::Realizing),
            ),
            (
                "R2",
                McpAttachmentState::Realizing,
                Ok(McpAttachmentState::Realizing),
            ),
            (
                "R3",
                McpAttachmentState::Active,
                Ok(McpAttachmentState::Active),
            ),
            (
                "R4",
                McpAttachmentState::Draining,
                Err(McpAttachmentError::InvalidTransition),
            ),
            (
                "R5",
                McpAttachmentState::Removed,
                Err(McpAttachmentError::InvalidTransition),
            ),
            (
                "R6",
                McpAttachmentState::Failed,
                Err(McpAttachmentError::InvalidTransition),
            ),
        ] {
            let mut set = SessionMcpAttachmentSet::from_initial(
                vec![draft("a", "https://a.test/mcp", None)],
                None,
            )
            .unwrap();
            set.attachments[0].state = state;
            let id = set.attachments[0].attachment_id.clone();
            let before = set.clone();
            let actual = set
                .claim_recovery(&id, McpGeneration(1), claim(rule))
                .map(|()| set.attachments[0].state);
            assert_eq!(actual, expected, "{rule}");
            if expected.is_err() {
                assert_eq!(set, before, "{rule} no mutation");
            } else {
                assert_eq!(
                    set.attachments[0]
                        .realization
                        .as_ref()
                        .map(|claim| claim.realization_id.as_str()),
                    Some(rule),
                    "{rule}"
                );
            }
        }

        let mut set = SessionMcpAttachmentSet::from_initial(
            vec![draft("a", "https://a.test/mcp", None)],
            None,
        )
        .unwrap();
        let id = set.attachments[0].attachment_id.clone();
        let before = set.clone();
        assert_eq!(
            set.claim_recovery(&id, McpGeneration(2), claim("R7")),
            Err(McpAttachmentError::StaleGeneration),
            "R7"
        );
        assert_eq!(set, before, "R7 no mutation");
    }

    #[test]
    fn reconciliation_selection_tests_are_generated_from_decision_table() {
        // Causal graph: generation exists -> state is nonterminal -> Runtime
        // projection may be missing/stale after restart -> include in the one
        // repository recovery index. Terminal facts require no Runtime effect.
        //
        // | Rule | State | Nonterminal | Indexed |
        // |---|---|---|---|
        // | Q1 | Requested | T | T |
        // | Q2 | Realizing | T | T |
        // | Q3 | Active | T | T |
        // | Q4 | Draining | T | T |
        // | Q5 | Removed | F | F |
        // | Q6 | Failed | F | F |
        for (rule, state, expected) in [
            ("Q1", McpAttachmentState::Requested, true),
            ("Q2", McpAttachmentState::Realizing, true),
            ("Q3", McpAttachmentState::Active, true),
            ("Q4", McpAttachmentState::Draining, true),
            ("Q5", McpAttachmentState::Removed, false),
            ("Q6", McpAttachmentState::Failed, false),
        ] {
            let mut set = SessionMcpAttachmentSet::from_initial(
                vec![draft("a", "https://a.test/mcp", None)],
                None,
            )
            .unwrap();
            set.attachments[0].state = state;
            assert_eq!(set.needs_reconciliation(), expected, "{rule}");
        }
    }

    #[test]
    fn full_replacement_tests_are_generated_from_decision_table() {
        // Cause graph: canonical desired fingerprint equal -> no-op; otherwise
        // diff by logical name. Missing name adds generation 1, changed exact
        // facts allocate generation N+1 while old Active remains visible, and a
        // removed name enters Draining. A matching nonterminal generation is
        // adopted instead of duplicated.
        //
        // | Rule | desired fingerprint | logical name | exact facts | current | Effect |
        // |---|---|---|---|---|---|
        // | D1 | changed | missing | - | - | add gen 1 Requested |
        // | D2 | equal | present | T | Active | no-op |
        // | D3 | changed | present | F | Active | add gen 2; old stays Active |
        // | D4 | changed | removed | - | Active | Draining |
        // | D5 | changed | present | T | Requested | adopt; no duplicate |
        // | D6 | equal | present | T | Failed | add retry generation N+1 |
        // | D7 | changed | remove+add | - | Active | keep old visible until switch |
        // | D8 | changed | present | prompts flag changed | Active | add gen 2 |
        let mut add = SessionMcpAttachmentSet::default();
        let plan = add
            .request_full_replacement(vec![draft("a", "https://a.test/mcp", None)], None)
            .expect("D1");
        assert_eq!(plan.requested.len(), 1, "D1");
        assert_eq!(add.attachments[0].generation, McpGeneration(1), "D1");

        add.attachments[0].state = McpAttachmentState::Active;
        let unchanged = add
            .request_full_replacement(vec![draft("a", "https://a.test/mcp", None)], None)
            .expect("D2");
        assert!(!unchanged.changed, "D2");

        let replaced = add
            .request_full_replacement(vec![draft("a", "https://b.test/mcp", None)], None)
            .expect("D3");
        assert_eq!(replaced.requested.len(), 1, "D3");
        assert_eq!(add.attachments[0].state, McpAttachmentState::Active, "D3");
        assert_eq!(add.attachments[1].generation, McpGeneration(2), "D3");

        let mut prompt_policy = SessionMcpAttachmentSet::from_initial(
            vec![draft("docs", "https://docs.test/mcp", None)],
            None,
        )
        .expect("D8 initial");
        prompt_policy.attachments[0].state = McpAttachmentState::Active;
        let mut prompt_enabled = draft("docs", "https://docs.test/mcp", None);
        prompt_enabled.prompts_as_skills = true;
        let prompt_replaced = prompt_policy
            .request_full_replacement(vec![prompt_enabled], None)
            .expect("D8");
        assert!(prompt_replaced.changed, "D8");
        assert_eq!(prompt_policy.attachments.len(), 2, "D8");
        assert_eq!(
            prompt_policy.attachments[1].generation,
            McpGeneration(2),
            "D8"
        );
        assert!(prompt_policy.attachments[1].prompts_as_skills, "D8");

        let count = add.attachments.len();
        let adopted = add
            .request_full_replacement(
                vec![
                    draft("a", "https://b.test/mcp", None),
                    draft("b", "https://c.test/mcp", None),
                ],
                None,
            )
            .expect("D5");
        assert!(adopted.changed, "D5 desired set changed by b");
        assert_eq!(
            add.attachments
                .iter()
                .filter(|attachment| attachment.name == "a")
                .count(),
            2,
            "D5 adopts a's pending generation"
        );
        assert_eq!(add.attachments.len(), count + 1, "D5 only b is added");

        let mut retry = SessionMcpAttachmentSet::from_initial(
            vec![draft("a", "https://a.test/mcp", None)],
            None,
        )
        .unwrap();
        retry.attachments[0].state = McpAttachmentState::Failed;
        let retried = retry
            .request_full_replacement(vec![draft("a", "https://a.test/mcp", None)], None)
            .expect("D6");
        assert_eq!(retried.requested.len(), 1, "D6");
        assert_eq!(retry.attachments[1].generation, McpGeneration(2), "D6");

        let mut atomic = SessionMcpAttachmentSet::from_initial(
            vec![draft("old", "https://old.test/mcp", None)],
            None,
        )
        .unwrap();
        atomic.attachments[0].state = McpAttachmentState::Active;
        let switched = atomic
            .request_full_replacement(vec![draft("new", "https://new.test/mcp", None)], None)
            .expect("D7");
        assert_eq!(switched.requested.len(), 1, "D7");
        assert_eq!(atomic.visible()[0].name, "old", "D7 before switch");
        atomic.begin_obsolete_drains().expect("D7 switch");
        assert!(atomic.visible().is_empty(), "D7 after switch");

        let removed = add.request_full_replacement(Vec::new(), None).expect("D4");
        assert_eq!(
            removed.draining.len(),
            1,
            "D4 only active generation drains"
        );
        assert_eq!(add.visible().len(), 0, "D4");
    }

    #[test]
    fn projection_receipt_is_bound_to_generation_and_effect_kind() {
        let generation = McpGenerationRef {
            session_id: "session".into(),
            attachment_id: McpAttachmentId("attachment".into()),
            generation: McpGeneration(3),
            runtime_incarnation: "runtime".into(),
            lease_epoch: 7,
            lease_expires_at_unix_ms: 99,
        };
        let receipt =
            McpProjectionReceipt::new(generation.clone(), McpProjectionEffectKind::Publish);
        assert!(
            receipt
                .verify(&generation, McpProjectionEffectKind::Publish)
                .is_ok()
        );
        assert!(
            receipt
                .verify(&generation, McpProjectionEffectKind::Drain)
                .is_err(),
            "publish evidence cannot acknowledge drain"
        );
        let mut stale = generation;
        stale.lease_epoch += 1;
        assert!(
            receipt
                .verify(&stale, McpProjectionEffectKind::Publish)
                .is_err(),
            "a stale owner cannot reuse projection evidence"
        );
    }
}
