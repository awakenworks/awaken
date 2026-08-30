//! Session Environment quiescence over the canonical durable MCP aggregate.

use super::*;

impl SessionMcpAttachmentSet {
    /// Project the exact durable MCP generations whose published Runtime
    /// effects belong to a Resident Session Environment. This aggregate-owned
    /// set is the only desired-owner input to quiescence; process-local slots
    /// may prove and drain effects but may not infer durable ownership.
    pub fn active_generation_refs(
        &self,
        session_id: &str,
    ) -> Result<Vec<McpGenerationRef>, McpAttachmentError> {
        let mut generations = self
            .attachments
            .iter()
            .filter(|attachment| attachment.state == McpAttachmentState::Active)
            .map(|attachment| {
                let claim = attachment
                    .realization
                    .as_ref()
                    .ok_or(McpAttachmentError::StaleRealizationClaim)?;
                Ok(McpGenerationRef {
                    session_id: session_id.to_owned(),
                    attachment_id: attachment.attachment_id.clone(),
                    generation: attachment.generation,
                    runtime_incarnation: claim.runtime_incarnation.clone(),
                    lease_epoch: claim.lease_epoch,
                    lease_expires_at_unix_ms: claim.lease_expires_at_unix_ms,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        generations.sort_by_key(crate::stable_fingerprint);
        if generations.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(McpAttachmentError::QuiescenceSetMismatch);
        }
        Ok(generations)
    }

    /// Invalidate publication acknowledgement for exactly the active durable
    /// generations proven quiescent by Runtime. Validation and revision
    /// capacity are completed before mutation, so a foreign, partial,
    /// duplicate, or otherwise stale receipt leaves the aggregate unchanged.
    /// State, generation, and realization claims remain authoritative and are
    /// reused by the existing realization phase protocol after restore.
    pub fn require_reprojection_after_quiescence(
        &mut self,
        session_id: &str,
        quiesced: &[McpGenerationRef],
    ) -> Result<usize, McpAttachmentError> {
        let expected = self.active_generation_refs(session_id)?;
        let mut asserted = quiesced.to_vec();
        asserted.sort_by_key(crate::stable_fingerprint);
        if asserted.windows(2).any(|pair| pair[0] == pair[1]) || asserted != expected {
            return Err(McpAttachmentError::QuiescenceSetMismatch);
        }
        let changed = self
            .attachments
            .iter()
            .filter(|attachment| {
                attachment.state == McpAttachmentState::Active
                    && attachment.publication_acknowledged
            })
            .count();
        if changed == 0 {
            return Ok(0);
        }
        let revision = self
            .revision
            .0
            .checked_add(1)
            .ok_or(McpAttachmentError::CounterExhausted)?;
        for attachment in &mut self.attachments {
            if attachment.state == McpAttachmentState::Active {
                attachment.publication_acknowledged = false;
            }
        }
        self.revision = McpSetRevision(revision);
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::draft;
    use super::*;

    #[test]
    fn reprojection_follows_the_exact_active_set_decision_table() {
        // Cause graph: C1 Runtime asserts the complete canonical Active set;
        // C2 the assertion is foreign, partial, or duplicate; C3 every Active
        // publication is already unacknowledged; C4 revision capacity remains.
        // Effects: E1 clear only publication acknowledgement and bump once;
        // E2 reject with the complete aggregate unchanged; E3 exact replay is
        // a no-op. Attachment state, generation, and claim never change.
        //
        // | Rule | Exact set | Duplicate | Already unacked | Capacity | Effect |
        // |---|---|---|---|---|---|
        // | Q1 | yes | no | no | yes | E1 |
        // | Q2 | partial/foreign | no | any | any | E2 |
        // | Q3 | no | yes | any | any | E2 |
        // | Q4 | yes | no | yes | any | E3 |
        // | Q5 | yes | no | no | no | E2 |
        let acknowledged = || {
            let mut set = SessionMcpAttachmentSet::from_initial(
                vec![
                    draft("alpha", "https://alpha.test/mcp", None),
                    draft("beta", "https://beta.test/mcp", None),
                ],
                None,
            )
            .unwrap();
            let generations = set
                .attachments
                .iter()
                .map(|attachment| (attachment.attachment_id.clone(), attachment.generation))
                .collect::<Vec<_>>();
            for (index, (attachment_id, generation)) in generations.iter().enumerate() {
                let realization_id = format!("realization-{index}");
                set.claim_realization(
                    attachment_id,
                    *generation,
                    McpRealizationClaim {
                        realization_id: realization_id.clone(),
                        runtime_incarnation: "worker-a/boot-1".into(),
                        lease_epoch: 7,
                        lease_expires_at_unix_ms: u64::MAX,
                        stage_idempotency_key: format!("stage-{index}"),
                    },
                )
                .unwrap();
                set.activate(attachment_id, *generation, &realization_id)
                    .unwrap();
                set.acknowledge_publication(attachment_id, *generation, &realization_id)
                    .unwrap();
            }
            set
        };
        let exact = acknowledged()
            .active_generation_refs("session-a")
            .expect("Q1 exact Active set");

        let mut accepted = acknowledged();
        let state_generation_claims = accepted
            .attachments
            .iter()
            .map(|attachment| {
                (
                    attachment.state,
                    attachment.generation,
                    attachment.realization.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            accepted
                .require_reprojection_after_quiescence("session-a", &exact)
                .unwrap(),
            2,
            "Q1/E1"
        );
        assert!(
            accepted
                .attachments
                .iter()
                .all(|attachment| !attachment.publication_acknowledged),
            "Q1/E1"
        );
        assert_eq!(
            accepted
                .attachments
                .iter()
                .map(|attachment| {
                    (
                        attachment.state,
                        attachment.generation,
                        attachment.realization.clone(),
                    )
                })
                .collect::<Vec<_>>(),
            state_generation_claims,
            "Q1 preserves durable identity"
        );
        let replay_revision = accepted.revision;
        assert_eq!(
            accepted
                .require_reprojection_after_quiescence("session-a", &exact)
                .unwrap(),
            0,
            "Q4/E3"
        );
        assert_eq!(accepted.revision, replay_revision, "Q4/E3");

        let mut foreign = exact.clone();
        foreign[0].session_id = "session-b".into();
        let duplicate = vec![exact[0].clone(), exact[0].clone()];
        for (rule, asserted) in [
            ("Q2-partial", exact[..1].to_vec()),
            ("Q2-foreign", foreign),
            ("Q3-duplicate", duplicate),
        ] {
            let mut set = acknowledged();
            let before = set.clone();
            assert_eq!(
                set.require_reprojection_after_quiescence("session-a", &asserted),
                Err(McpAttachmentError::QuiescenceSetMismatch),
                "{rule}/E2"
            );
            assert_eq!(set, before, "{rule}/E2 no mutation");
        }

        let mut exhausted = acknowledged();
        exhausted.revision = McpSetRevision(u64::MAX);
        let before = exhausted.clone();
        assert_eq!(
            exhausted.require_reprojection_after_quiescence("session-a", &exact),
            Err(McpAttachmentError::CounterExhausted),
            "Q5/E2"
        );
        assert_eq!(exhausted, before, "Q5/E2 no mutation");
    }
}
