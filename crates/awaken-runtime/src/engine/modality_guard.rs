//! Runtime request validation against resolved model input modalities.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::content::{ContentBlock, DocumentSource};
use awaken_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, InferenceStream, LlmExecutor,
};
use awaken_contract::contract::inference::StreamResult;
use awaken_contract::registry_spec::{Modality, ModelSpec};

use crate::registry::model_capabilities::CapabilitySource;

/// Executor wrapper that rejects request content unsupported by a `ModelSpec`.
pub(crate) struct ModalityGuardExecutor {
    inner: Arc<dyn LlmExecutor>,
    model_id: String,
    input: Vec<Modality>,
}

impl ModalityGuardExecutor {
    pub(crate) fn wrap_trusted(
        inner: Arc<dyn LlmExecutor>,
        model: &ModelSpec,
        source: Option<CapabilitySource>,
    ) -> Arc<dyn LlmExecutor> {
        if model.modalities.input.is_empty() {
            return inner;
        }
        if !source.is_some_and(CapabilitySource::is_runtime_trusted) {
            return inner;
        }
        Arc::new(Self {
            inner,
            model_id: model.id.clone(),
            input: model.modalities.input.clone(),
        })
    }

    fn validate(&self, request: &InferenceRequest) -> Result<(), InferenceExecutionError> {
        validate_system(&request.system, &self.input, &self.model_id)?;
        for (message_idx, message) in request.messages.iter().enumerate() {
            validate_blocks(
                &message.content,
                &self.input,
                &self.model_id,
                &format!("messages[{message_idx}].content"),
            )?;
        }
        Ok(())
    }
}

#[async_trait]
impl LlmExecutor for ModalityGuardExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        self.validate(&request)?;
        self.inner.execute(request).await
    }

    fn execute_stream(
        &self,
        request: InferenceRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<InferenceStream, InferenceExecutionError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            self.validate(&request)?;
            self.inner.execute_stream(request).await
        })
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn supports_upstream_model_override(&self) -> bool {
        self.inner.supports_upstream_model_override()
    }

    fn record_stream_success(&self, request: &InferenceRequest) {
        self.inner.record_stream_success(request);
    }

    fn record_stream_failure(&self, request: &InferenceRequest, err: &InferenceExecutionError) {
        self.inner.record_stream_failure(request, err);
    }
}

fn validate_system(
    blocks: &[ContentBlock],
    allowed: &[Modality],
    model_id: &str,
) -> Result<(), InferenceExecutionError> {
    validate_blocks(blocks, allowed, model_id, "system")
}

fn validate_blocks(
    blocks: &[ContentBlock],
    allowed: &[Modality],
    model_id: &str,
    path: &str,
) -> Result<(), InferenceExecutionError> {
    for (idx, block) in blocks.iter().enumerate() {
        let block_path = format!("{path}[{idx}]");
        match block {
            ContentBlock::Text { .. } => {
                validate_modality(Modality::Text, allowed, model_id, &block_path)?
            }
            ContentBlock::Image { .. } => {
                validate_modality(Modality::Image, allowed, model_id, &block_path)?
            }
            ContentBlock::Document { source, .. } => {
                if let Some(modality) = document_modality(source) {
                    validate_modality(modality, allowed, model_id, &block_path)?;
                }
            }
            ContentBlock::Audio { .. } => {
                validate_modality(Modality::Audio, allowed, model_id, &block_path)?
            }
            ContentBlock::Video { .. } => {
                validate_modality(Modality::Video, allowed, model_id, &block_path)?
            }
            ContentBlock::ToolResult { content, .. } => {
                validate_blocks(content, allowed, model_id, &format!("{block_path}.content"))?;
            }
            ContentBlock::Thinking { .. } | ContentBlock::ToolUse { .. } => {}
        }
    }
    Ok(())
}

fn document_modality(source: &DocumentSource) -> Option<Modality> {
    match source {
        DocumentSource::Base64 { media_type, .. } => media_type
            .eq_ignore_ascii_case("application/pdf")
            .then_some(Modality::Pdf),
        DocumentSource::Url { url } => url
            .split(['?', '#'])
            .next()
            .is_some_and(|path| path.to_ascii_lowercase().ends_with(".pdf"))
            .then_some(Modality::Pdf),
    }
}

fn validate_modality(
    modality: Modality,
    allowed: &[Modality],
    model_id: &str,
    path: &str,
) -> Result<(), InferenceExecutionError> {
    if allowed.contains(&modality) {
        return Ok(());
    }
    Err(InferenceExecutionError::InvalidRequest(format!(
        "model '{model_id}' does not support {modality:?} input at {path}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::message::Message;
    use awaken_contract::registry_spec::Modalities;

    #[derive(Default)]
    struct CountingExecutor {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LlmExecutor for CountingExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(StreamResult {
                content: Vec::new(),
                tool_calls: Vec::new(),
                usage: None,
                stop_reason: None,
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "counting"
        }
    }

    fn request_with(content: Vec<ContentBlock>) -> InferenceRequest {
        InferenceRequest {
            upstream_model: "upstream".into(),
            routing_key: None,
            messages: vec![Message::user_with_content(content)],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: false,
        }
    }

    fn model_with_input(input: Vec<Modality>) -> ModelSpec {
        ModelSpec {
            modalities: Modalities {
                input,
                output: vec![Modality::Text],
            },
            ..ModelSpec::new("m", "p", "u")
        }
    }

    #[tokio::test]
    async fn rejects_unsupported_image_before_provider_call() {
        let inner = Arc::new(CountingExecutor::default());
        let executor = ModalityGuardExecutor::wrap_trusted(
            inner.clone(),
            &model_with_input(vec![Modality::Text]),
            Some(CapabilitySource::ExplicitSpec),
        );

        let err = executor
            .execute(request_with(vec![ContentBlock::image_url(
                "https://example.com/image.png",
            )]))
            .await
            .expect_err("text-only model should reject image input");

        assert!(matches!(err, InferenceExecutionError::InvalidRequest(_)));
        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn allows_declared_image_input() {
        let inner = Arc::new(CountingExecutor::default());
        let executor = ModalityGuardExecutor::wrap_trusted(
            inner.clone(),
            &model_with_input(vec![Modality::Text, Modality::Image]),
            Some(CapabilitySource::ExplicitSpec),
        );

        executor
            .execute(request_with(vec![
                ContentBlock::text("describe"),
                ContentBlock::image_url("https://example.com/image.png"),
            ]))
            .await
            .expect("vision model should allow image input");

        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn protocol_blocks_do_not_count_as_input_modalities() {
        let inner = Arc::new(CountingExecutor::default());
        let executor = ModalityGuardExecutor::wrap_trusted(
            inner.clone(),
            &model_with_input(vec![Modality::Text]),
            Some(CapabilitySource::ExplicitSpec),
        );

        executor
            .execute(request_with(vec![
                ContentBlock::Thinking {
                    thinking: "internal reasoning".into(),
                },
                ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "search".into(),
                    input: serde_json::json!({"q": "awaken"}),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: vec![ContentBlock::text("result")],
                },
            ]))
            .await
            .expect("protocol blocks are not user input modalities");

        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn tool_result_media_content_is_still_checked() {
        let inner = Arc::new(CountingExecutor::default());
        let executor = ModalityGuardExecutor::wrap_trusted(
            inner.clone(),
            &model_with_input(vec![Modality::Text]),
            Some(CapabilitySource::ExplicitSpec),
        );

        let err = executor
            .execute(request_with(vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".into(),
                content: vec![ContentBlock::image_url("https://example.com/image.png")],
            }]))
            .await
            .expect_err("media inside tool result should still be checked");

        assert!(matches!(err, InferenceExecutionError::InvalidRequest(_)));
        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn only_identifiable_pdf_documents_require_pdf_modality() {
        let inner = Arc::new(CountingExecutor::default());
        let executor = ModalityGuardExecutor::wrap_trusted(
            inner.clone(),
            &model_with_input(vec![Modality::Text]),
            Some(CapabilitySource::ExplicitSpec),
        );

        let err = executor
            .execute(request_with(vec![ContentBlock::document_base64(
                "application/pdf",
                "base64",
                Some("paper".into()),
            )]))
            .await
            .expect_err("pdf input should require Pdf modality");
        assert!(matches!(err, InferenceExecutionError::InvalidRequest(_)));

        executor
            .execute(request_with(vec![ContentBlock::document_base64(
                "text/csv",
                "a,b",
                Some("table".into()),
            )]))
            .await
            .expect("non-pdf document kinds are not represented by Modality::Pdf");

        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unspecified_modalities_do_not_wrap_executor() {
        let inner = Arc::new(CountingExecutor::default());
        let executor = ModalityGuardExecutor::wrap_trusted(
            inner.clone(),
            &ModelSpec::new("m", "p", "u"),
            Some(CapabilitySource::ExplicitSpec),
        );

        executor
            .execute(request_with(vec![ContentBlock::audio_url(
                "https://example.com/audio.mp3",
            )]))
            .await
            .expect("unknown model modalities should remain permissive");

        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
