//! Execution of one publication-pinned remote A2A attempt.

use super::*;

pub(super) fn ensure_supported_narrowing(
    activation: &RunActivation,
    context: &RuntimeRunContext,
) -> Result<()> {
    if activation.tool_capability_narrowing == ToolCapabilityNarrowing::DenyAll
        || context.tool_permission_policy.is_some()
    {
        return Err(Error::Execution(
            "A2A cannot prove enforcement of this Run's deny-all tool capability".to_string(),
        ));
    }
    Ok(())
}

#[async_trait]
impl RunExecutor for A2aRunExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities {
            cancellation: Cancellation::RemoteAbort,
            wait: Wait::Both,
            live_input: awaken_runtime_contract::execution::LiveInput::None,
        }
    }

    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        ensure_supported_narrowing(&activation, &context)?;
        let Ok(candidate) = remote_candidate_of(&activation) else {
            // Reached without a remote backend — a wiring fault; fail closed.
            let mut messages = activation.input.clone();
            messages.push(assistant_message(
                &context,
                &activation,
                "backend is not an A2A endpoint",
            ));
            return finish_terminal(
                &context,
                &activation,
                messages,
                EndCause::Error(Failure::Inference {
                    code: "a2a_config".to_string(),
                    message: "backend is not a2a".to_string(),
                }),
            )
            .await;
        };
        let endpoint = endpoint_of(candidate)?;

        let route = match restored_task_route(&context, &activation, &endpoint) {
            Ok(route) => route,
            Err(error) => {
                return finish_invalid_task_reference(&context, &activation, error).await;
            }
        };
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let transport = self
            .transport_resolver
            .resolve(candidate, &context)
            .await
            .map_err(Error::Execution)?;
        let task = match route {
            TaskReferenceRoute::Reattach(reference) => {
                verify_attempt_ownership(context.ownership.as_deref()).await?;
                let task = get_task(transport.as_ref(), &reference.task_id)
                    .await
                    .map_err(|error| Error::Execution(error.to_string()))?;
                validate_reattached_task(&reference, &task)?;
                task
            }
            TaskReferenceRoute::Fresh => {
                let normalized = awaken_runtime_contract::NormalizedModelInput::new(
                    &activation.snapshot.resolved_spec.instructions,
                    &context.request_context,
                    &activation.input,
                );
                let trace = normalized.trace();
                tracing::debug!(
                    run_id = %activation.run_id.0,
                    thread_id = %activation.thread_id.0,
                    publication_id = %activation.snapshot.fingerprint.0,
                    backend = "a2a",
                    has_instructions = trace.has_instructions,
                    request_context_supplied = trace.request_context_supplied,
                    request_context_visible = trace.request_context_visible,
                    durable_input_supplied = trace.durable_input_supplied,
                    durable_input_visible = trace.durable_input_visible,
                    legacy_derived_filtered = trace.legacy_derived_filtered,
                    "projected privacy-safe model input"
                );
                let prompt = normalized.text_envelope();
                let message_id = stable_message_id(&activation.run_id, None)?;
                verify_attempt_ownership(context.ownership.as_deref()).await?;
                let task = match send_message(
                    transport.as_ref(),
                    None,
                    &activation.thread_id.0,
                    &message_id,
                    &prompt,
                )
                .await
                {
                    Ok(task) => task,
                    Err(err) => {
                        let mut messages = activation.input.clone();
                        messages.push(assistant_message(
                            &context,
                            &activation,
                            format!("remote agent error: {err}"),
                        ));
                        return finish_terminal(
                            &context,
                            &activation,
                            messages,
                            EndCause::Error(Failure::Inference {
                                code: "a2a_error".to_string(),
                                message: err.to_string(),
                            }),
                        )
                        .await;
                    }
                };
                commit_boundary(
                    &context,
                    &activation,
                    RunDisposition::running(activation.run_id.clone()),
                    activation.input.clone(),
                    vec![task_reference_state(&TaskReference::from_task(
                        &endpoint, &task,
                    ))?],
                )
                .await?;
                task
            }
        };
        task_driver::drive_task(transport, &endpoint, &activation, &context, task).await
    }
}

#[async_trait]
impl RunAttemptExecutor for A2aRunExecutor {
    fn physical_attempt_recovery(
        &self,
        activation: &RunActivation,
        context: &RuntimeRunContext,
    ) -> AttemptRecoveryMode {
        // Cause C1: the recovered Run has a complete committed A2A TaskReference.
        // Cause C2: that reference is bound to the publication-frozen endpoint.
        // Effect E1: C1+C2 permits replacing only the Dispatch physical slot;
        // `execute` then uses `tasks/get` and cannot repeat `message:send`.
        // Effect E2: every absent, malformed, non-remote, or mismatched reference
        // retains the generic predecessor-quiescence fence.
        //
        // | Rule | C1 | C2 | Recovery mode |
        // |---|---|---|---|
        // | R1 | yes | yes | continue durable remote operation |
        // | R2 | no | n/a | require predecessor quiescence |
        // | R3 | yes | no | require predecessor quiescence |
        let continuation_is_bound = (|| {
            let candidate = remote_candidate_of(activation).ok()?;
            let endpoint = endpoint_of(candidate).ok()?;
            matches!(
                restored_task_route(context, activation, &endpoint).ok()?,
                TaskReferenceRoute::Reattach(_)
            )
            .then_some(())
        })()
        .is_some();
        if continuation_is_bound {
            AttemptRecoveryMode::ContinueDurableRemoteOperation
        } else {
            AttemptRecoveryMode::RequirePredecessorQuiescence
        }
    }

    async fn resume(
        &self,
        activation: RunActivation,
        command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        ensure_supported_narrowing(&activation, &context)?;
        let reader = context
            .reader
            .as_ref()
            .ok_or_else(|| Error::Execution("A2A resume requires committed history".to_string()))?;
        let ticket = reader
            .resume_ticket(&activation.run_id)
            .ok_or_else(|| Error::Execution("A2A run is not awaiting a resume".to_string()))?;
        validate_resume(&ticket, &command)
            .map_err(|error| Error::Execution(format!("invalid A2A resume: {error}")))?;
        let candidate = remote_candidate_of(&activation)?;
        let endpoint = endpoint_of(candidate)?;
        let reference = match restored_task_route(&context, &activation, &endpoint) {
            Ok(TaskReferenceRoute::Reattach(reference)) => reference,
            Ok(TaskReferenceRoute::Fresh) => {
                return finish_invalid_task_reference(
                    &context,
                    &activation,
                    Error::Execution("A2A resume is missing its durable remote task".to_string()),
                )
                .await;
            }
            Err(error) => {
                return finish_invalid_task_reference(&context, &activation, error).await;
            }
        };
        let text = resume_text(&command.result)?;
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let transport = self
            .transport_resolver
            .resolve(candidate, &context)
            .await
            .map_err(Error::Execution)?;
        let message_id = stable_message_id(&activation.run_id, Some(&ticket.correlation_id))?;
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let task = send_message(
            transport.as_ref(),
            None,
            &reference.context_id,
            &message_id,
            &text,
        )
        .await
        .map_err(|error| Error::Execution(error.to_string()))?;
        let reference = continued_task_reference(&endpoint, &reference, &task)?;
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![task_reference_state(&reference)?],
        )
        .await?;
        task_driver::drive_task(transport, &endpoint, &activation, &context, task).await
    }

    async fn cancel(&self, activation: RunActivation, context: RuntimeRunContext) -> Result<()> {
        let candidate = remote_candidate_of(&activation)?;
        let endpoint = endpoint_of(candidate)?;
        let reference = match restored_task_route(&context, &activation, &endpoint)? {
            TaskReferenceRoute::Fresh => return Ok(()),
            TaskReferenceRoute::Reattach(reference) => reference,
        };
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let transport = self
            .transport_resolver
            .resolve(candidate, &context)
            .await
            .map_err(Error::Execution)?;
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let task = get_task(transport.as_ref(), &reference.task_id)
            .await
            .map_err(|error| Error::Execution(error.to_string()))?;
        validate_reattached_task(&reference, &task)?;
        if matches!(
            task.status.state,
            TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected
        ) {
            return Ok(());
        }
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        try_cancel_task(transport.as_ref(), &reference.task_id)
            .await
            .map_err(|error| Error::Execution(error.to_string()))
    }
}
