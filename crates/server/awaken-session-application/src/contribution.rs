//! Canonical Session application-contribution command.

use awaken_session_contract::{
    ApplicationContributionError, ApplicationSessionContribution,
    ApplicationSessionContributionApi, ApplicationSessionContributionFailure,
    ApplicationSessionContributionReceipt, CompiledSessionCreation, McpAttachmentOrigin,
    PersistedSession, SessionBaselineState, SessionMcpAttachmentSet,
};

use super::{
    McpAttachmentCandidate, McpAttachmentCandidateTarget, SessionApplication, SessionMutationError,
    mutation::repository_failure,
};

fn contribution_mutation(error: SessionMutationError) -> ApplicationSessionContributionFailure {
    match error {
        SessionMutationError::NotFound => ApplicationSessionContributionFailure::NotFound,
        SessionMutationError::Conflict | SessionMutationError::IdempotencyMismatch => {
            ApplicationSessionContributionFailure::Conflict
        }
        SessionMutationError::Unavailable(message) => {
            ApplicationSessionContributionFailure::Unavailable(message)
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplicationMcpInput {
    name: String,
    url: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    credential_source_id: Option<String>,
    #[serde(default)]
    credential_revision: Option<u64>,
    #[serde(default)]
    prompts_as_skills: bool,
}

impl ApplicationMcpInput {
    fn into_candidate(
        self,
    ) -> Result<McpAttachmentCandidate, ApplicationSessionContributionFailure> {
        if self.kind.as_deref().is_some_and(|kind| kind != "url") {
            return Err(ApplicationSessionContributionFailure::Invalid(
                "application MCP input type must be `url`".into(),
            ));
        }
        let published_credential = match (self.credential_source_id, self.credential_revision) {
            (None, None) => None,
            (Some(id), Some(revision)) if !id.trim().is_empty() && revision > 0 => {
                Some((id, revision))
            }
            _ => {
                return Err(ApplicationSessionContributionFailure::Invalid(
                    "application MCP credential requires a non-empty source id and positive exact revision"
                        .into(),
                ));
            }
        };
        Ok(McpAttachmentCandidate {
            name: self.name,
            target: McpAttachmentCandidateTarget::HttpUrl(self.url),
            prompts_as_skills: self.prompts_as_skills,
            published_credential,
            origin: McpAttachmentOrigin::Application,
        })
    }
}

fn map_contribution_error(
    error: ApplicationContributionError,
) -> ApplicationSessionContributionFailure {
    match error {
        ApplicationContributionError::NotRequired => {
            ApplicationSessionContributionFailure::NotRequired
        }
        ApplicationContributionError::Conflict => ApplicationSessionContributionFailure::Conflict,
        ApplicationContributionError::EmptyFingerprint => {
            ApplicationSessionContributionFailure::Invalid(error.to_string())
        }
    }
}

fn application_mcp_candidates(
    input: &awaken_session_contract::ApplicationSessionInput,
) -> Result<Vec<McpAttachmentCandidate>, ApplicationSessionContributionFailure> {
    input
        .mcp_inputs
        .iter()
        .cloned()
        .map(|value| {
            serde_json::from_value::<ApplicationMcpInput>(value)
                .map_err(|error| {
                    ApplicationSessionContributionFailure::Invalid(format!(
                        "application MCP input is malformed: {error}"
                    ))
                })?
                .into_candidate()
        })
        .collect()
}

impl SessionApplication {
    pub async fn commit_compiled_session_creation(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        mut compiled: CompiledSessionCreation,
    ) -> Result<PersistedSession, ApplicationSessionContributionFailure> {
        match &session.baseline {
            SessionBaselineState::Preparing(_) => {}
            SessionBaselineState::Frozen(_) => {
                return Err(ApplicationSessionContributionFailure::Unavailable(
                    "Session creation intent was already consumed".into(),
                ));
            }
        }
        let holder = compiled
            .baseline
            .environment
            .credential_realization
            .mcp_holder
            .clone();
        for input in session.resources.desired().inputs.iter().cloned() {
            if compiled
                .initial_resources
                .inputs
                .iter()
                .any(|candidate| candidate == &input)
            {
                continue;
            }
            compiled.initial_resources =
                compiled.initial_resources.attach(input).map_err(|error| {
                    ApplicationSessionContributionFailure::Unavailable(error.to_string())
                })?;
        }
        if compiled.initial_resources.skills.is_none() {
            compiled.initial_resources.skills = session.resources.desired().skills.clone();
        }
        let mut resources = session.resources.clone();
        let result = if resources.pending.is_some() {
            resources.revise_unattempted_pending(&session.session_id, compiled.initial_resources)
        } else {
            resources.prepare(&session.session_id, compiled.initial_resources)
        };
        result.map_err(|error| {
            ApplicationSessionContributionFailure::Unavailable(error.to_string())
        })?;
        let mcp = SessionMcpAttachmentSet::from_initial(compiled.initial_mcp, Some(holder))
            .map_err(|error| {
                ApplicationSessionContributionFailure::Unavailable(error.to_string())
            })?;
        session.baseline = SessionBaselineState::Frozen(compiled.baseline);
        session.resources = resources;
        session.mcp = mcp;
        self.commit_resource_snapshot(owner_scope, session, "finalize-creation", Vec::new())
            .await
            .map_err(|error| match error {
                SessionMutationError::Conflict => ApplicationSessionContributionFailure::Conflict,
                error => ApplicationSessionContributionFailure::Unavailable(error.to_string()),
            })
    }
}

#[async_trait::async_trait]
impl ApplicationSessionContributionApi for SessionApplication {
    async fn contribute_application(
        &self,
        contribution: ApplicationSessionContribution,
    ) -> Result<ApplicationSessionContributionReceipt, ApplicationSessionContributionFailure> {
        if contribution.session_id.trim().is_empty() {
            return Err(ApplicationSessionContributionFailure::Invalid(
                "Session id is empty".into(),
            ));
        }
        let owner_scope = self
            .owner(&contribution.session_id)
            .await
            .map_err(contribution_mutation)?;

        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let mut session = self
                .session_repository()
                .get(&contribution.session_id)
                .await
                .map_err(repository_failure)
                .map_err(contribution_mutation)?;
            let intent = match &mut session.baseline {
                SessionBaselineState::Frozen(baseline) => {
                    let receipt = baseline
                        .application
                        .as_ref()
                        .ok_or(ApplicationSessionContributionFailure::NotRequired)?;
                    let outcome = receipt
                        .verify_replay(&contribution.application_fingerprint, &contribution.input)
                        .map_err(map_contribution_error)?;
                    return Ok(ApplicationSessionContributionReceipt {
                        outcome,
                        projection: Self::frozen_session_projection(owner_scope.clone(), &session)?,
                    });
                }
                SessionBaselineState::Preparing(intent) => intent,
            };
            let outcome = intent
                .application
                .accept(
                    contribution.application_fingerprint.clone(),
                    contribution.input.clone(),
                )
                .map_err(map_contribution_error)?;
            let ordered_vault_ids = intent.control.mcp_authoring.ordered_vault_ids.clone();
            let candidates = application_mcp_candidates(&contribution.input)?;
            let application_mcp = self
                .normalize_mcp_drafts(candidates, &ordered_vault_ids)
                .await
                .map_err(|error| {
                    ApplicationSessionContributionFailure::Invalid(error.to_string())
                })?;
            let compiled = intent.clone().finalize(application_mcp).map_err(|error| {
                ApplicationSessionContributionFailure::Invalid(error.to_string())
            })?;
            match self
                .commit_compiled_session_creation(&owner_scope, session, compiled)
                .await
            {
                Ok(session) => {
                    return Ok(ApplicationSessionContributionReceipt {
                        outcome,
                        projection: Self::frozen_session_projection(owner_scope.clone(), &session)?,
                    });
                }
                Err(ApplicationSessionContributionFailure::Conflict)
                    if attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(ApplicationSessionContributionFailure::Conflict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn application_mcp_credential_pair_follows_the_causal_decision_table() {
        // Cause graph: source-id present XOR revision present -> malformed;
        // neither -> anonymous; both valid -> the existing exact credential pin.
        //
        // | Rule | source id | revision | Effect |
        // |---|---|---|---|
        // | A1 | absent | absent | anonymous Application candidate |
        // | A2 | present | present positive | exact credential candidate |
        // | A3 | present | absent | reject |
        // | A4 | absent | present | reject |
        // | A5 | blank | present | reject |
        // | A6 | present | zero | reject |
        // | A7 | valid pair | non-URL type | reject |
        // | A8 | valid pair | unknown field | reject |
        let rows = [
            (
                "A1",
                serde_json::json!({"name":"m","type":"url","url":"https://m.example"}),
                true,
                None,
            ),
            (
                "A2",
                serde_json::json!({"name":"m","type":"url","url":"https://m.example","credential_source_id":"cred-1","credential_revision":7}),
                true,
                Some(("cred-1", 7)),
            ),
            (
                "A3",
                serde_json::json!({"name":"m","url":"https://m.example","credential_source_id":"cred-1"}),
                false,
                None,
            ),
            (
                "A4",
                serde_json::json!({"name":"m","url":"https://m.example","credential_revision":7}),
                false,
                None,
            ),
            (
                "A5",
                serde_json::json!({"name":"m","url":"https://m.example","credential_source_id":" ","credential_revision":7}),
                false,
                None,
            ),
            (
                "A6",
                serde_json::json!({"name":"m","url":"https://m.example","credential_source_id":"cred-1","credential_revision":0}),
                false,
                None,
            ),
            (
                "A7",
                serde_json::json!({"name":"m","type":"stdio","url":"https://m.example","credential_source_id":"cred-1","credential_revision":7}),
                false,
                None,
            ),
            (
                "A8",
                serde_json::json!({"name":"m","url":"https://m.example","credential_source_id":"cred-1","credential_revision":7,"bearer":"must-not-pass"}),
                false,
                None,
            ),
        ];
        for (rule, input, expected_ok, expected_pin) in rows {
            let result =
                application_mcp_candidates(&awaken_session_contract::ApplicationSessionInput {
                    mcp_inputs: vec![input],
                    ..Default::default()
                });
            assert_eq!(result.is_ok(), expected_ok, "{rule}");
            if let (Ok(candidates), Some((id, revision))) = (result, expected_pin) {
                assert_eq!(
                    candidates[0].published_credential,
                    Some((id.to_string(), revision)),
                    "{rule}"
                );
            }
        }
    }
}
