// @formal surface exclude=read_model_contract
// @formal reason: Public immutable inspection values over the original model journal; no engine SPI handles, commands, persistence or retry authority.
//! Read-only model-attempt inspection language over an already committed prefix.
use serde::{Deserialize, Serialize};
/// Recorded invocation coordinates, not a success claim or retry permission.
/// SDK: none — original model journal projected by the Awaken evidence read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOperationInspection {
    pub ordinal: u64,
    pub provider: String,
    pub model: String,
    pub backend: String,
    pub phase: String,
}

/// Inspect original operations without exposing the engine SPI or acquiring authority.
/// SDK: none — pure committed-prefix inspection used by server read adapters.
pub fn recorded_model_operations(
    commands: &[awaken_agent_contract::agent::state::Command],
    run: &awaken_agent_contract::agent::run::Id,
) -> Result<Vec<ModelOperationInspection>, String> {
    use crate::model_operation::{ActiveModelOperation, ModelOperationPhase};
    ActiveModelOperation::recorded(commands, run)
        .map_err(|_| "Recorded model operations are invalid".to_owned())
        .map(|operations| {
            operations
                .into_iter()
                .map(|operation| {
                    let (provider, model, backend) = operation.binding.identity();
                    ModelOperationInspection {
                        ordinal: operation.ordinal.get(),
                        provider: provider.into(),
                        model: model.into(),
                        backend: backend.into(),
                        phase: match operation.phase {
                            ModelOperationPhase::Entered => "entered",
                            ModelOperationPhase::Returned => "returned",
                            ModelOperationPhase::TimedOut => "timed_out",
                        }
                        .into(),
                    }
                })
                .collect()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model_operation::{ActiveModelOperation, ModelOperation, ModelOperationPhase},
        resolved::ModelBinding,
    };
    use awaken_agent_contract::agent::{run::Id, state::StateKey};

    #[test]
    fn inspection_reads_each_original_route_without_upgrading_entry_to_success() {
        // DT-I1: C1 original Run has two model routes, C2 first invocation only
        // entered, C3 second returned, C4 another Run has no journal. E1 retain
        // exact ordinal/provider/model/backend and phase; E2 historical absence
        // stays empty. Constraints: an entry may precede dispatch; no success or
        // retry permission can be inferred from this inspection.
        let run = Id("run-original".into());
        let first = ModelOperation::next(
            None,
            ModelBinding::parse("provider-a", "model-a", "native").unwrap(),
        )
        .unwrap();
        let mut second = ModelOperation::next(
            Some(&first),
            ModelBinding::parse("provider-b", "model-b", "acp:codex").unwrap(),
        )
        .unwrap();
        second.phase = ModelOperationPhase::Returned;
        let commands = [first, second]
            .into_iter()
            .map(|op| {
                let mut command = ActiveModelOperation::write(&Some(op));
                command.run_id = Some(run.clone());
                command
            })
            .collect::<Vec<_>>();
        let result = recorded_model_operations(&commands, &run).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(
            (&*result[0].model, &*result[0].backend, &*result[0].phase),
            ("model-a", "native", "entered")
        );
        assert_eq!(
            (
                result[1].ordinal,
                &*result[1].provider,
                &*result[1].model,
                &*result[1].backend,
                &*result[1].phase
            ),
            (2, "provider-b", "model-b", "acp:codex", "returned")
        );
        assert!(
            recorded_model_operations(&commands, &Id("historical".into()))
                .unwrap()
                .is_empty()
        );
        // DT-I2 malformed matching journal -> read failure, never empty success.
        let mut malformed = commands;
        malformed[0].action =
            awaken_agent_contract::agent::state::Action::Set(serde_json::json!({"bad":"journal"}));
        assert!(recorded_model_operations(&malformed, &run).is_err());
    }
}
