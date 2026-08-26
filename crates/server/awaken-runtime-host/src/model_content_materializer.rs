//! Attempt-bound projection from logical Files ids to immutable model content.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use awaken_agent_contract::agent::content::{ContentBlock, DocumentSource, ImageSource};
use awaken_resource_contract::{
    FileContentSource, FileReadPurpose, ResolvedFileContent, content_id,
};
use awaken_run_ingress::RunClaim;
use awaken_runtime_contract::llm::{ChatRequest, Error, ModelContentMaterializer};
use base64::Engine as _;

pub(crate) struct ResourceModelContentMaterializer {
    source: Arc<dyn FileContentSource<RunClaim>>,
    workspace_id: String,
    thread_id: String,
    claim: Option<RunClaim>,
}

impl ResourceModelContentMaterializer {
    pub(crate) fn new(
        source: Arc<dyn FileContentSource<RunClaim>>,
        workspace_id: impl Into<String>,
        thread_id: impl Into<String>,
        claim: Option<RunClaim>,
    ) -> Self {
        Self {
            source,
            workspace_id: workspace_id.into(),
            thread_id: thread_id.into(),
            claim,
        }
    }
}

#[async_trait::async_trait]
impl ModelContentMaterializer for ResourceModelContentMaterializer {
    async fn materialize(&self, mut request: ChatRequest) -> Result<ChatRequest, Error> {
        let mut ids = BTreeSet::new();
        for message in &request.messages {
            collect_file_ids(&message.content, &mut ids);
        }
        if ids.is_empty() {
            return Ok(request);
        }

        let purpose = FileReadPurpose::ModelContent {
            thread_id: self.thread_id.clone(),
        };
        let mut resolved = BTreeMap::new();
        for file_id in ids {
            let content = self
                .source
                .read(&self.workspace_id, &file_id, &purpose, self.claim.as_ref())
                .await
                .map_err(|error| {
                    Error::Provider(format!("model File {file_id} is unavailable: {error}"))
                })?
                .ok_or_else(|| {
                    Error::InvalidRequest(format!("model File {file_id} was not found"))
                })?;
            validate_resolved(&file_id, &content)?;
            resolved.insert(file_id, content);
        }

        for message in &mut request.messages {
            replace_file_ids(&mut message.content, &resolved)?;
        }
        Ok(request)
    }
}

fn collect_file_ids(blocks: &[ContentBlock], ids: &mut BTreeSet<String>) {
    for block in blocks {
        match block {
            ContentBlock::Image {
                source: ImageSource::File { file_id },
            }
            | ContentBlock::Document {
                source: DocumentSource::File { file_id },
                ..
            } => {
                ids.insert(file_id.clone());
            }
            ContentBlock::ToolResult { content, .. } => collect_file_ids(content, ids),
            ContentBlock::Text { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::Document { .. }
            | ContentBlock::SearchResult { .. }
            | ContentBlock::ToolReference { .. }
            | ContentBlock::Redacted
            | ContentBlock::ToolUse { .. }
            | ContentBlock::Thinking { .. } => {}
        }
    }
}

fn validate_resolved(file_id: &str, resolved: &ResolvedFileContent) -> Result<(), Error> {
    if resolved.file_id != file_id
        || resolved.content_id != content_id(&resolved.bytes)
        || resolved.media_type.trim().is_empty()
    {
        return Err(Error::Provider(format!(
            "model File {file_id} failed immutable metadata verification"
        )));
    }
    Ok(())
}

fn replace_file_ids(
    blocks: &mut [ContentBlock],
    resolved: &BTreeMap<String, ResolvedFileContent>,
) -> Result<(), Error> {
    for block in blocks {
        match block {
            ContentBlock::Image { source } => {
                let ImageSource::File { file_id } = source else {
                    continue;
                };
                let content = exact_content(resolved, file_id)?;
                if !content.media_type.starts_with("image/") {
                    return Err(Error::InvalidRequest(format!(
                        "model image File {file_id} has non-image media type {}",
                        content.media_type
                    )));
                }
                *source = ImageSource::Base64 {
                    media_type: content.media_type.clone(),
                    data: base64::engine::general_purpose::STANDARD.encode(&content.bytes),
                };
            }
            ContentBlock::Document { source, .. } => {
                let DocumentSource::File { file_id } = source else {
                    continue;
                };
                let content = exact_content(resolved, file_id)?;
                *source = if content.media_type == "text/plain" {
                    DocumentSource::Text {
                        media_type: content.media_type.clone(),
                        data: String::from_utf8(content.bytes.clone()).map_err(|_| {
                            Error::InvalidRequest(format!(
                                "model text File {file_id} is not valid UTF-8"
                            ))
                        })?,
                    }
                } else {
                    DocumentSource::Base64 {
                        media_type: content.media_type.clone(),
                        data: base64::engine::general_purpose::STANDARD.encode(&content.bytes),
                    }
                };
            }
            ContentBlock::ToolResult { content, .. } => replace_file_ids(content, resolved)?,
            ContentBlock::Text { .. }
            | ContentBlock::SearchResult { .. }
            | ContentBlock::ToolReference { .. }
            | ContentBlock::Redacted
            | ContentBlock::ToolUse { .. }
            | ContentBlock::Thinking { .. } => {}
        }
    }
    Ok(())
}

fn exact_content<'a>(
    resolved: &'a BTreeMap<String, ResolvedFileContent>,
    file_id: &str,
) -> Result<&'a ResolvedFileContent, Error> {
    resolved.get(file_id).ok_or_else(|| {
        Error::Provider(format!(
            "model File {file_id} disappeared during atomic materialization"
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_agent_contract::agent::message::Role;
    use awaken_resource_contract::FileContentSourceError;
    use awaken_runtime_contract::llm::ChatMessage;
    use awaken_runtime_contract::resolved::ModelBinding;

    use super::*;

    struct Source {
        content: Option<ResolvedFileContent>,
        fail: bool,
        reads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl FileContentSource<RunClaim> for Source {
        async fn read(
            &self,
            workspace_id: &str,
            file_id: &str,
            purpose: &FileReadPurpose,
            _claim: Option<&RunClaim>,
        ) -> Result<Option<ResolvedFileContent>, FileContentSourceError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            assert_eq!(workspace_id, "workspace-a");
            assert_eq!(file_id, "file-a");
            assert_eq!(
                purpose,
                &FileReadPurpose::ModelContent {
                    thread_id: "thread-a".into()
                }
            );
            if self.fail {
                Err(FileContentSourceError::new("source offline"))
            } else {
                Ok(self.content.clone())
            }
        }
    }

    fn request(blocks: Vec<ContentBlock>) -> ChatRequest {
        ChatRequest {
            model_binding: ModelBinding::new("provider", "model", "native"),
            inference: Default::default(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: blocks,
            }],
            tools: Vec::new(),
        }
    }

    fn resolved(media_type: &str, bytes: &[u8]) -> ResolvedFileContent {
        ResolvedFileContent {
            file_id: "file-a".into(),
            content_id: content_id(bytes),
            filename: "input.bin".into(),
            media_type: media_type.into(),
            bytes: bytes.to_vec(),
        }
    }

    fn materializer(source: Arc<Source>) -> ResourceModelContentMaterializer {
        ResourceModelContentMaterializer::new(source, "workspace-a", "thread-a", None)
    }

    /// Cause-effect graph and FMECA (model File materialization):
    /// C1=request contains no File -> E1=zero reads, unchanged request.
    /// C2=one authorized immutable File referenced N times -> E2=one read and all
    /// references replaced; constraint C2 excludes C3-C6.
    /// C3=logical File absent -> E3=invalid request, zero provider dispatch.
    /// C4=source unavailable -> E4=provider/infrastructure error, zero provider dispatch.
    /// C5=id/digest/MIME metadata invalid -> E5=fail closed before replacement.
    /// C6=image has non-image MIME -> E6=invalid request before replacement.
    /// C7=text/plain document is UTF-8 -> E7=typed text source; invalid UTF-8 ->
    /// E8=invalid request before replacement.
    /// FMECA: stale/substituted bytes (severity critical, detect digest/id check),
    /// cross-kind image confusion (high, detect MIME gate), repeated mutable reads
    /// (high, mitigate request-local dedup), and partial replacement (high,
    /// mitigate resolve-all then mutate). Decision rules M1=C1; M2=C2; M3=C3;
    /// M4=C4; M5=C5; M6=C6; M7=C7 valid; M8=C7 invalid. This test owns
    /// M1/M2/M7 and nested ToolResult coverage.
    #[tokio::test]
    async fn materializes_nested_repeated_references_once_and_keeps_no_file_ids() {
        let source = Arc::new(Source {
            content: Some(resolved("image/png", b"png")),
            fail: false,
            reads: AtomicUsize::new(0),
        });
        let sut = materializer(source.clone());
        let output = sut
            .materialize(request(vec![
                ContentBlock::image_file("file-a"),
                ContentBlock::tool_result("call", vec![ContentBlock::document_file("file-a")]),
            ]))
            .await
            .expect("M2 materializes every reference");
        assert_eq!(source.reads.load(Ordering::SeqCst), 1, "M2 dedup");
        let encoded = serde_json::to_string(&output).unwrap();
        assert!(!encoded.contains("\"type\":\"file\""), "M2 no logical refs");

        let empty_source = Arc::new(Source {
            content: None,
            fail: false,
            reads: AtomicUsize::new(0),
        });
        let input = request(vec![ContentBlock::text("plain")]);
        assert_eq!(
            materializer(empty_source.clone())
                .materialize(input.clone())
                .await
                .unwrap(),
            input,
            "M1"
        );
        assert_eq!(empty_source.reads.load(Ordering::SeqCst), 0, "M1");

        let text = materializer(Arc::new(Source {
            content: Some(resolved("text/plain", b"plain facts")),
            fail: false,
            reads: AtomicUsize::new(0),
        }))
        .materialize(request(vec![ContentBlock::document_file("file-a")]))
        .await
        .expect("M7 text/plain becomes a text document source");
        assert!(matches!(
            &text.messages[0].content[0],
            ContentBlock::Document {
                source: DocumentSource::Text { media_type, data }, ..
            } if media_type == "text/plain" && data == "plain facts"
        ));
    }

    /// Decision rules M3-M6/M8 from the cause graph above. Each failure is detected
    /// before a request can reach an LLM; the Runtime integration test separately
    /// proves the provider call count remains zero.
    #[tokio::test]
    async fn missing_unavailable_substituted_and_wrong_kind_files_fail_closed() {
        let cases = [
            (None, false, "was not found", "M3"),
            (None, true, "source offline", "M4"),
            (
                Some(ResolvedFileContent {
                    content_id: "wrong".into(),
                    ..resolved("image/png", b"png")
                }),
                false,
                "metadata verification",
                "M5",
            ),
            (
                Some(resolved("text/plain", b"text")),
                false,
                "non-image media type",
                "M6",
            ),
        ];
        for (content, fail, expected, rule) in cases {
            let error = materializer(Arc::new(Source {
                content,
                fail,
                reads: AtomicUsize::new(0),
            }))
            .materialize(request(vec![ContentBlock::image_file("file-a")]))
            .await
            .expect_err(rule);
            assert!(error.to_string().contains(expected), "{rule}: {error}");
        }

        let invalid_utf8 = materializer(Arc::new(Source {
            content: Some(resolved("text/plain", &[0xff, 0xfe])),
            fail: false,
            reads: AtomicUsize::new(0),
        }))
        .materialize(request(vec![ContentBlock::document_file("file-a")]))
        .await
        .expect_err("M8 invalid UTF-8");
        assert!(invalid_utf8.to_string().contains("not valid UTF-8"), "M8");
    }
}
