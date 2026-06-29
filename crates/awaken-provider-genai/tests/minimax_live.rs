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
use awaken_runtime_contract::llm::{
    ChatMessage, ChatRequest, ChatRole, DeltaSink, LlmExecutor, ToolSchema,
};
use awaken_runtime_contract::resolved::ModelBinding;
use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};

fn model() -> String {
    std::env::var("MINIMAX_MODEL").unwrap_or_else(|_| "MiniMax-M3".to_string())
}

fn executor() -> GenaiExecutor {
    // genai's Anthropic adapter appends `messages` to this base, so it must end
    // at the versioned path: `.../anthropic/v1/`. Without `v1/` the stream
    // endpoint returns 404.
    let base = std::env::var("MINIMAX_BASE_URL")
        .unwrap_or_else(|_| "https://api.minimaxi.com/anthropic/v1/".to_string());
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
    tool_calls: Mutex<Vec<(String, String, serde_json::Value)>>,
}

#[async_trait::async_trait]
impl DeltaSink for Recorder {
    async fn on_text(&self, chunk: &str) {
        self.chunks.lock().unwrap().push(chunk.to_string());
    }

    async fn on_tool_call(&self, call_id: &str, tool_id: &str, arguments: &serde_json::Value) {
        self.tool_calls.lock().unwrap().push((
            call_id.to_string(),
            tool_id.to_string(),
            arguments.clone(),
        ));
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

/// A weather tool the model is steered into calling. Its single required string
/// argument lets us assert the streamed arguments accumulated into valid JSON.
fn weather_tool() -> ToolSchema {
    ToolSchema {
        id: "get_weather".to_string(),
        description: "Get the current weather for a city.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "city": { "type": "string", "description": "City name" }
            },
            "required": ["city"]
        }),
    }
}

#[tokio::test]
#[ignore = "requires network and MINIMAX_API_KEY"]
async fn minimax_streaming_tool_call_accumulates_arguments() {
    let request = ChatRequest {
        model_binding: binding(),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ContentBlock::text(
                "Use the get_weather tool to look up the weather in Paris. \
                 Call the tool; do not answer in prose.",
            )],
        }],
        tools: vec![weather_tool()],
    };
    let recorder = Recorder::default();
    let response = executor()
        .infer_streaming(request, &recorder)
        .await
        .expect("stream tool call");

    // The committed turn is the source of truth (G13): it must carry the call.
    let committed = response.output.tool_calls();
    println!("[minimax tool stream] committed calls -> {committed:?}");
    let call = committed
        .iter()
        .find(|c| c.tool_id == "get_weather")
        .expect("model called get_weather");

    // The live sink saw the call too, and its final arguments accumulated into
    // the same valid JSON the committed call carries (genai upserts by call id).
    let live = recorder.tool_calls.lock().unwrap().clone();
    println!("[minimax tool stream] {} live tool-call deltas", live.len());
    assert!(
        !live.is_empty(),
        "at least one live tool-call delta arrived"
    );
    let last = live
        .iter()
        .rev()
        .find(|(_, tool_id, _)| tool_id == "get_weather")
        .expect("a live get_weather delta");
    assert_eq!(
        last.2, call.arguments,
        "the final live arguments equal the committed arguments"
    );
    // MiniMax delivers tool arguments as a JSON *string* (e.g. `"{\"city\":...}"`),
    // not a pre-parsed object; the adapter forwards the provider's shape verbatim.
    // Accept either: parse the string when needed, then assert the object holds a
    // string `city`. This proves the streamed deltas accumulated into valid JSON.
    let parsed = match &call.arguments {
        serde_json::Value::String(raw) => {
            serde_json::from_str::<serde_json::Value>(raw).expect("arguments are valid JSON text")
        }
        other => other.clone(),
    };
    assert!(
        parsed.get("city").and_then(|v| v.as_str()).is_some(),
        "accumulated arguments parse to an object with a string city: {parsed:?}",
    );
}
