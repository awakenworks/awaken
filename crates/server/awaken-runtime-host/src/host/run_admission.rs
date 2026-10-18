// @formal entrypoint model_route.fallback_preserves_the_complete_publication_pin
//! Frozen dispatch-envelope resolution and exact root tool-format binding.

use super::*;

impl crate::RunAssembly {
    /// Resolve the one exact inference plaintext holder used by both direct and
    /// dispatched attempts from the immutable candidate publication.
    pub(crate) fn inference_plaintext_holder(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<awaken_runtime_contract::PlaintextHolder>, HostError> {
        awaken_dispatch_execution::session_dispatch::inference_plaintext_holder(activation, || {
            Ok(self.thread_credential_realization(&activation.thread_id.0))
        })
        .map_err(HostError::from)
    }

    /// Bind the same fresh-root format before direct execution can commit an
    /// Awaiting ticket. Its later durable resume reads that frozen ticket.
    pub(crate) fn bind_fresh_root_tool_configuration(
        &self,
        activation: RunActivation,
    ) -> RunActivation {
        let is_session_dispatch = self
            .installed_sessions
            .read_run(&activation.thread_id.0, |run| run.session_dispatch)
            .unwrap_or(false);
        awaken_dispatch_execution::placement::bind_root_tool_configuration_format(
            activation,
            is_session_dispatch,
        )
    }

    pub(crate) fn prepare_dispatch(
        &self,
        activation: RunActivation,
    ) -> awaken_dispatch_execution::session_dispatch::DispatchPreparation<'_> {
        let configured = activation.effective_model_ref() != crate::UNCONFIGURED_MODEL_REF;
        awaken_dispatch_execution::session_dispatch::DispatchPreparation::new(
            activation,
            Box::new(super::dispatch_execution::InstalledSessionReads(
                self.installed_sessions.realizations(),
            )),
            self.local_workspace(),
            self.agent.agent_publications.as_deref(),
            crate::agent_catalog::RUN_PUBLICATIONS,
            self.runs_local_dispatch_pool(),
            configured,
        )
    }
}
#[cfg(test)]
mod tests {
    use crate::NoModelConfiguredExecutor;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime_contract::resolved::ModelBinding;
    use std::sync::Arc;

    #[tokio::test]
    async fn dispatch_model_admission_precedes_frozen_slot_reads() {
        // Cause/effect graph: C1 the immutable activation selects a configured
        // model; C2 the original installed-slot mutex is poisoned. E1 a missing
        // model returns the existing guidance before any slot read; E2 an
        // admitted model uses the original slot owner, whose poisoned lock denies
        // admission with the typed realization error instead of unwinding. No
        // alternate registry or fallback snapshot is allowed.
        // | Rule | C1 configured | C2 poisoned | Effect |
        // | R1 | no  | no  | E1 BadRequest |
        // | R2 | no  | yes | E1 BadRequest, no slot access |
        // | R3 | yes | no  | E2 compiled dispatch |
        // | R4 | yes | yes | E2 typed poison denial, no panic |
        for configured in [false, true] {
            for poisoned in [false, true] {
                let host = crate::HostDependencies::ephemeral(
                    Arc::new(NoModelConfiguredExecutor),
                    "model",
                )
                .await
                .build()
                .expect("validated Host dependencies");
                if poisoned {
                    let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        host.installed_sessions
                            .update("admission-order", |_| panic!("fixture poison"));
                    }));
                    assert!(poison.is_err(), "C2 uses the original registry lock");
                }
                let model = if configured {
                    "model"
                } else {
                    crate::UNCONFIGURED_MODEL_REF
                };
                let activation = awaken_runtime_contract::RunActivation::new(
                    awaken_agent_contract::agent::run::Id("admission-order-run".into()),
                    ThreadId("admission-order".into()),
                    awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
                        .model(ModelBinding::parse("test", model, "native").expect("fixture model"))
                        .build(),
                    Vec::new(),
                );
                let admitted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    host.run_assembly.prepare_dispatch(activation).fresh()
                }));
                if !configured {
                    let error = admitted
                        .expect("R1/R2 model admission precedes slot access")
                        .expect_err("R1/R2 reject an unconfigured model");
                    assert!(matches!(
                        error,
                        awaken_dispatch_execution::AdmissionError::Rejected(_)
                    ));
                    assert!(error.to_string().starts_with("No model is configured."));
                } else if poisoned {
                    let denied = admitted
                        .expect("R4 a poisoned slot owner denies without unwinding")
                        .expect_err("R4 retains the original fail-closed slot owner");
                    let poisoned_code = crate::realization_error::RealizationError::Poisoned(
                        crate::realization_error::RealizationLock::InstalledSessions,
                    )
                    .code();
                    assert!(
                        matches!(
                            &denied,
                            awaken_dispatch_execution::AdmissionError::Run(error)
                                if error.code == poisoned_code.as_ref()
                        ),
                        "R4 {denied}"
                    );
                } else {
                    assert!(admitted.expect("R3 healthy slot").is_ok());
                }
            }
        }
    }

    #[tokio::test]
    async fn fresh_and_resumed_root_dispatch_use_the_ticket_format() {
        use awaken_agent_contract::agent::run::Id as RunId;
        use awaken_runtime_contract::RunActivation;
        use awaken_runtime_contract::activation::{
            INHERITED_TOOLSETS_CAPABILITY, InheritedToolsets,
        };

        // Causes: C1 a fresh Session root can author an awaiting ticket; C2 a
        // resumed Run carries frozen V1 sources; C3 a legacy ticket has none.
        // Effects: E1 direct and queued fresh roots bind the same marker; E2
        // resume preserves C2 and its Worker capability; E3 C3 stays unmarked
        // instead of silently changing an already-committed Run. The actual
        // await/reply/continuation is covered by Coordinator Outcome HITL.
        // | Rule | fresh | ticket sources | effect |
        // | R1 | yes | absent at admission | E1 |
        // | R2 | no | V1 empty | E2 |
        // | R3 | no | none | E3 |
        let host =
            (crate::HostDependencies::ephemeral(Arc::new(NoModelConfiguredExecutor), "model")
                .await)
                .build()
                .expect("validated Host dependencies");
        host.installed_sessions
            .update_run("format-root", |run| run.session_dispatch = true);
        let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
            .model(ModelBinding::parse("test", "model", "native").expect("valid fixture"))
            .build();
        let activation = RunActivation::new(
            RunId("format-run".into()),
            ThreadId("format-root".into()),
            snapshot,
            Vec::new(),
        );
        let direct = host
            .run_assembly
            .bind_fresh_root_tool_configuration(activation.clone());
        let queued = host
            .run_assembly
            .prepare_dispatch(activation.clone())
            .fresh()
            .expect("R1 queue");
        assert_eq!(
            direct.inherited_toolsets(),
            queued.activation.inherited_toolsets(),
            "R1 direct and queued fresh paths freeze one format"
        );
        let frozen = InheritedToolsets::V1(Vec::new());
        assert_eq!(direct.inherited_toolsets(), Some(&frozen), "R1 marker");
        let resumed = host
            .run_assembly
            .prepare_dispatch(activation.clone().with_inherited_toolsets(Some(frozen)))
            .resume()
            .expect("R2 resume");
        assert_eq!(
            resumed.activation.inherited_toolsets(),
            direct.inherited_toolsets(),
            "R2 frozen sources"
        );
        assert!(
            resumed
                .placement
                .required_capabilities
                .contains(INHERITED_TOOLSETS_CAPABILITY),
            "R2 Worker must understand the marker"
        );
        let legacy = host
            .run_assembly
            .prepare_dispatch(activation)
            .resume()
            .expect("R3 legacy resume");
        assert_eq!(legacy.activation.inherited_toolsets(), None, "R3");
        assert!(
            !legacy
                .placement
                .required_capabilities
                .contains(INHERITED_TOOLSETS_CAPABILITY),
            "R3 must not invent a new ticket format"
        );
    }
}
