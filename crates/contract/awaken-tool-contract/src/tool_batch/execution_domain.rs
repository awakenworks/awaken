// @formal surface requirement=runtime.run_tool_lifecycle.lifecycle
// @formal reason: Pre-entry domains and immutable history extend the one ToolBatch aggregate; adjacent codec/history tables cover parsing and scope drift, without a second receipt store or direct proof of external containment.
//! Pre-entry effect domains and their immutable canonical history.
//! This extends ToolBatch; it owns no persistence or execution path.

use super::*;

/// Effect containment published by the concrete implementation, never by the
/// model or an authored recovery policy. A process reference is not a receipt;
/// its independent provider must observe exact closure before release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolExecutionDomain {
    Opaque {},
    Worker {},
    ControlledProcess {
        provider_ref: String,
        execution_ref: String,
    },
}

impl Default for ToolExecutionDomain {
    fn default() -> Self {
        Self::Opaque {}
    }
}

impl ToolExecutionDomain {
    pub fn validate(&self) -> Result<(), ToolBatchError> {
        if let Self::ControlledProcess {
            provider_ref,
            execution_ref,
        } = self
            && (provider_ref.trim().is_empty() || execution_ref.trim().is_empty())
        {
            return Err(ToolBatchError::InvalidPersistedState(
                "process containment requires an exact provider binding".into(),
            ));
        }
        Ok(())
    }
}

impl ToolBatch {
    pub fn mark_executing(&mut self, call_id: &str) -> Result<u16, ToolBatchError> {
        self.mark_executing_in_domain(call_id, None)
    }

    pub fn bind_materialization_domain(
        &mut self,
        call_id: &str,
        domain: ToolExecutionDomain,
    ) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        domain.validate()?;
        let call = self.call_mut(call_id)?;
        if call
            .materialization_domain
            .as_ref()
            .is_some_and(|old| old != &domain)
        {
            return Err(ToolBatchError::InvalidPersistedState(
                "result materialization changed its containment".into(),
            ));
        }
        call.materialization_domain = Some(domain);
        Ok(())
    }

    pub fn mark_executing_in_domain(
        &mut self,
        call_id: &str,
        domain: Option<ToolExecutionDomain>,
    ) -> Result<u16, ToolBatchError> {
        if let Some(domain) = &domain {
            domain.validate()?;
        }
        self.ensure_open()?;
        let call = self.call_mut(call_id)?;
        let phase = phase_view(&call.phase);
        let attempt = next_execution_attempt(
            phase,
            call.recovery_policy.max_attempts(),
            ExecutionRequest::StartOrRetry,
        )?;
        call.phase = ToolCallPhase::Executing { attempt };
        call.execution_domain = domain;
        Ok(attempt)
    }
}

impl ActiveToolBatch {
    /// Transient history fold over the canonical cell. Each (batch, call,
    /// attempt) keeps its original containment even when a later batch replaces
    /// it. A later Set cannot hide malformed records or upgrade legacy scope.
    pub fn recorded_execution_domains(
        state: &[StateCommand],
        run_id: &RunId,
    ) -> Result<Vec<ToolExecutionDomain>, ToolBatchError> {
        let mut domains = std::collections::BTreeMap::new();
        let mut latest_attempts = std::collections::BTreeMap::new();
        for recorded in Self::recorded_batches(state, run_id) {
            let (_, batch) = recorded?;
            for call in batch.calls() {
                if let Some(domain) = &call.materialization_domain {
                    domain.validate()?;
                    let key = (batch.id().as_str().to_owned(), call.call.call_id.clone(), 0);
                    if domains.get(&key).is_some_and(|old| old != domain) {
                        return Err(ToolBatchError::InvalidPersistedState(
                            "materialization history changed its containment".into(),
                        ));
                    }
                    domains.insert(key, domain.clone());
                }
                let call_key = (batch.id().as_str().to_owned(), call.call.call_id.clone());
                if let ToolCallPhase::Executing { attempt } = call.phase {
                    latest_attempts.insert(call_key.clone(), attempt);
                    let domain = call.execution_domain.clone().unwrap_or_default();
                    domain.validate()?;
                    let key = (
                        batch.id().as_str().to_owned(),
                        call.call.call_id.clone(),
                        attempt,
                    );
                    if domains.get(&key).is_some_and(|old| old != &domain) {
                        return Err(ToolBatchError::InvalidPersistedState(
                            "tool entry changed its containment".into(),
                        ));
                    }
                    domains.insert(key, domain);
                } else if let Some(attempt) = latest_attempts.get(&call_key) {
                    let key = (call_key.0, call_key.1, *attempt);
                    if domains.get(&key) != Some(&call.execution_domain.clone().unwrap_or_default())
                    {
                        return Err(ToolBatchError::InvalidPersistedState(
                            "returned tool changed its entered containment".into(),
                        ));
                    }
                }
            }
        }
        Ok(domains.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::batch;

    #[test]
    fn every_domain_rejects_unknown_fields_without_changing_legacy_wire_shape() {
        // W1 exact Worker/opaque/process encoding => roundtrip; W2 add a
        // sibling field to ANY variant => reject. Empty struct variants keep
        // deny_unknown_fields effective for local/opaque domains too, so a
        // discarded process field cannot silently assert Worker containment.
        for domain in [
            ToolExecutionDomain::Opaque {},
            ToolExecutionDomain::Worker {},
            ToolExecutionDomain::ControlledProcess {
                provider_ref: "p".into(),
                execution_ref: "uid".into(),
            },
        ] {
            let mut value = serde_json::to_value(&domain).unwrap();
            assert_eq!(
                serde_json::from_value::<ToolExecutionDomain>(value.clone()).unwrap(),
                domain
            );
            value["unrecognized_participant"] = serde_json::json!("not silently discarded");
            assert!(serde_json::from_value::<ToolExecutionDomain>(value).is_err());
        }
        assert_eq!(
            serde_json::to_value(ToolExecutionDomain::Worker {}).unwrap(),
            serde_json::json!({"kind":"worker"})
        );
        assert_eq!(
            ToolExecutionDomain::default(),
            ToolExecutionDomain::Opaque {}
        );
    }
    use super::*;

    #[test]
    fn execution_and_materialization_domains_survive_batch_replacement_without_scope_upgrade() {
        // C1 entered tool in Worker/process/legacy-opaque scope; C2 spill owns
        // another process; C3 later batch replaces the active cell; C4 same
        // entry's scope changes on return. E1 history retains both domains;
        // E2 C3 cannot erase them; E3 C4 rejects; legacy absence stays opaque.
        // Input cells are production aggregates/decoders, not a receipt fixture.
        let run = RunId("r".into());
        let bind = |value: &ToolBatch| {
            let mut command = ActiveToolBatch::write(&Some(value.clone()));
            command.run_id = Some(run.clone());
            command
        };
        for scope in [
            ToolExecutionDomain::Worker {},
            ToolExecutionDomain::Opaque {},
        ] {
            let mut first = batch();
            first
                .mark_executing_in_domain("c", Some(scope.clone()))
                .unwrap();
            let entered = bind(&first);
            let spill = ToolExecutionDomain::ControlledProcess {
                provider_ref: "provider".into(),
                execution_ref: "spill-process".into(),
            };
            first
                .bind_materialization_domain("c", spill.clone())
                .unwrap();
            let materializing = bind(&first);
            first.complete(ToolOutput::ok("c", "done")).unwrap();
            first.finalize().unwrap();
            let returned = bind(&first);
            let next = ToolBatch::for_step(
                run.clone(),
                1,
                [(
                    ToolCall {
                        call_id: "next".into(),
                        tool_id: "t".into(),
                        arguments: serde_json::json!({}),
                    },
                    ToolRecoveryPolicy::default(),
                )],
            )
            .unwrap();
            let records = [
                entered.clone(),
                materializing,
                returned.clone(),
                bind(&next),
            ];
            let retained = ActiveToolBatch::recorded_execution_domains(&records, &run).unwrap();
            assert!(retained.contains(&scope));
            assert!(retained.contains(&spill));
            let mut changed = serde_json::to_value(first).unwrap();
            changed["calls"][0]["execution_domain"] =
                serde_json::to_value(if scope == (ToolExecutionDomain::Worker {}) {
                    ToolExecutionDomain::Opaque {}
                } else {
                    ToolExecutionDomain::Worker {}
                })
                .unwrap();
            let changed: ToolBatch = serde_json::from_value(changed).unwrap();
            assert!(
                ActiveToolBatch::recorded_execution_domains(
                    &[entered, returned, bind(&changed)],
                    &run
                )
                .is_err()
            );
        }
    }
}
