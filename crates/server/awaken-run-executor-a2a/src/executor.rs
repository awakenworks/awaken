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

        let restored = match restored_task_reference(&context, &activation) {
            Ok(restored) => restored,
            Err(error) => {
                return finish_invalid_task_reference(&context, &activation, error).await;
            }
        };
        if let Some(reference) = &restored
            && let Err(error) = ensure_endpoint(reference, &endpoint)
        {
            return finish_invalid_task_reference(&context, &activation, error).await;
        }
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let transport = self
            .transport_resolver
            .resolve(candidate, &context)
            .await
            .map_err(Error::Execution)?;
        let task = match restored {
            Some(reference) => {
                verify_attempt_ownership(context.ownership.as_deref()).await?;
                get_task(transport.as_ref(), &reference.task_id)
                    .await
                    .map_err(|error| Error::Execution(error.to_string()))?
            }
            None => {
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
                let message_id = format!("a2a-msg-{}", activation.run_id.0);
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
        let reference = match restored_task_reference(&context, &activation) {
            Ok(Some(reference)) => reference,
            Ok(None) => {
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
        if let Err(error) = ensure_endpoint(&reference, &endpoint) {
            return finish_invalid_task_reference(&context, &activation, error).await;
        }
        let text = resume_text(&command.result)?;
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let transport = self
            .transport_resolver
            .resolve(candidate, &context)
            .await
            .map_err(Error::Execution)?;
        let message_id = format!(
            "a2a-resume-{}-{}",
            activation.run_id.0, ticket.correlation_id
        );
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
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![task_reference_state(&TaskReference::from_task(
                &endpoint, &task,
            ))?],
        )
        .await?;
        task_driver::drive_task(transport, &endpoint, &activation, &context, task).await
    }

    async fn cancel(&self, activation: RunActivation, context: RuntimeRunContext) -> Result<()> {
        let candidate = remote_candidate_of(&activation)?;
        let endpoint = endpoint_of(candidate)?;
        let Some(reference) = restored_task_reference(&context, &activation)? else {
            return Ok(());
        };
        ensure_endpoint(&reference, &endpoint)?;
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
