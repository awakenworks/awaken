//! Live MiniMax smoke tests. Ignored by default; they need network and the
//! MiniMax key in the environment. MiniMax exposes an Anthropic-compatible
//! endpoint, so genai's Anthropic adapter is pointed at it via a
//! `ServiceTargetResolver`. Run with:
//!
//! ```sh
//! MINIMAX_API_KEY=... cargo test -p awaken-provider-genai --test minimax_live -- --ignored --nocapture
//! ```

use std::sync::Mutex;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, ChatRole, DeltaSink, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;
use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};

fn model() -> String {
    std::env::var("MINIMAX_MODEL").unwrap_or_else(|_| "MiniMax-M3".to_string())
}

fn executor() -> GenaiExecutor {
    let base = std::env::var("MINIMAX_BASE_URL")
        .unwrap_or_else(|_| "https://api.minimaxi.com/anthropic/".to_string());
    let key = std::env::var("MINIMAX_API_KEY").expect("MINIMAX_API_KEY must be set");
    let resolver = ServiceTargetResolver::from_resolver_fn(
        move |target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            let ServiceTarget { model, .. } = target;
            let endpoint = Endpoint::from_owned(base.clone());
            let auth = AuthData::from_single(key.clone());
            // MiniMax speaks the Anthropic wire format.
            let model = ModelIden::new(AdapterKind::Anthropic, model.model_name);
            Ok(ServiceTarget {
                endpoint,
                auth,
                model,
            })
        },
    );
    let client = Client::builder()
        .with_service_target_resolver(resolver)
        .build();
    GenaiExecutor::with_client(client)
}

fn binding() -> ModelBinding {
    ModelBinding {
        provider_instance_ref: "minimax".to_string(),
        model_ref: model(),
        backend_ref: "genai".to_string(),
    }
}

fn user(blocks: Vec<ContentBlock>) -> ChatRequest {
    ChatRequest {
        model_binding: binding(),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: blocks,
        }],
        tools: Vec::new(),
    }
}

#[derive(Default)]
struct Recorder {
    chunks: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl DeltaSink for Recorder {
    async fn on_text(&self, chunk: &str) {
        self.chunks.lock().unwrap().push(chunk.to_string());
    }
}

#[tokio::test]
#[ignore = "requires network and MINIMAX_API_KEY"]
async fn minimax_text_completion() {
    let request = user(vec![ContentBlock::text("Reply with the single word: pong")]);
    let response = executor().infer(request).await.expect("infer");
    let text = response.output.text_content();
    println!("[minimax text] -> {text:?}");
    assert!(!text.is_empty(), "model returned text");
}

#[tokio::test]
#[ignore = "requires network and MINIMAX_API_KEY"]
async fn minimax_streaming_arrives_in_chunks() {
    let request = user(vec![ContentBlock::text(
        "Count slowly from one to ten, words only.",
    )]);
    let recorder = Recorder::default();
    let response = executor()
        .infer_streaming(request, &recorder)
        .await
        .expect("stream");

    let chunks = recorder.chunks.lock().unwrap().clone();
    let assembled = response.output.text_content();
    println!(
        "[minimax stream] {} chunks, assembled {} chars",
        chunks.len(),
        assembled.chars().count()
    );
    assert!(
        !assembled.is_empty(),
        "assembled streamed text is non-empty"
    );
    assert!(!chunks.is_empty(), "at least one live chunk arrived");
    assert_eq!(
        chunks.concat(),
        assembled,
        "live chunks concatenate to the committed text"
    );
}

/// A 16x16 solid-red PNG, inline so the test needs no network for the image
/// itself. The Anthropic wire format takes images as base64, not URLs.
const RED_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAIAAACQkWg2AAAAFklEQVR42mP4z8BAEmIY1TCqYfhqAACQ+f8B8u7oVwAAAABJRU5ErkJggg==";

#[tokio::test]
#[ignore = "requires network and MINIMAX_API_KEY (vision model)"]
async fn minimax_multimodal_image() {
    let request = user(vec![
        ContentBlock::text("What single color fills this image? Answer with one word."),
        ContentBlock::image_base64("image/png", RED_PNG_B64),
    ]);
    let response = executor().infer(request).await.expect("infer image");
    let text = response.output.text_content();
    println!("[minimax image] -> {text:?}");
    assert!(!text.is_empty(), "model responded to the image");
}
