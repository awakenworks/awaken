pub mod travel {
    pub mod state;
    pub mod tools;
}

pub mod research {
    pub mod state;
    pub mod tools;
}

pub mod starter_backend;

use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tirea_agentos::composition::{
    tool_map_from_arc, AgentDefinition, AgentOsBuilder, ModelDefinition,
};
use tirea_agentos::contracts::runtime::behavior::AgentBehavior;
use tirea_agentos::contracts::runtime::tool_call::Tool;
use tirea_agentos::contracts::storage::{ThreadReader, ThreadStore};
use tirea_agentos_server::http::{self, AppState};
use tirea_agentos_server::protocol;
use tirea_store_adapters::FileStore;

#[derive(Debug, Parser)]
pub struct Args {
    #[arg(long, env = "AGENTOS_HTTP_ADDR", default_value = "127.0.0.1:8080")]
    pub http_addr: String,

    #[arg(long, env = "AGENTOS_STORAGE_DIR", default_value = "./sessions")]
    pub storage_dir: PathBuf,

    /// TensorZero gateway URL (e.g. http://localhost:4000/openai/v1/).
    /// When set, all model calls are routed through TensorZero for observability.
    #[arg(long, env = "TENSORZERO_URL")]
    pub tensorzero_url: Option<String>,
}

/// Build an AgentOs and start the HTTP server.
pub async fn serve(
    args: Args,
    agent_def: AgentDefinition,
    tools: Vec<Arc<dyn Tool>>,
    plugins: Vec<(String, Arc<dyn AgentBehavior>)>,
) {
    let agent_id = agent_def.id.clone();
    let model_id = agent_def.model.clone();
    let tool_map: HashMap<String, Arc<dyn Tool>> = tool_map_from_arc(tools);

    let mut builder = AgentOsBuilder::new()
        .with_agent(&agent_id, agent_def)
        .with_tools(tool_map);

    // When TensorZero URL is provided, route model calls through TensorZero gateway.
    if let Some(tz_url) = &args.tensorzero_url {
        let endpoint = tz_url.clone();
        let tz_client = genai::Client::builder()
            .with_service_target_resolver_fn(move |mut t: genai::ServiceTarget| {
                t.endpoint = genai::resolver::Endpoint::from_owned(&*endpoint);
                t.auth = genai::resolver::AuthData::from_single("tensorzero");
                Ok(t)
            })
            .build();

        builder = builder.with_provider("tz", tz_client).with_model(
            &model_id,
            ModelDefinition::new("tz", "openai::tensorzero::function_name::agent_chat"),
        );
        eprintln!("routing model '{model_id}' through TensorZero at {tz_url}");
    }

    for (id, plugin) in plugins {
        builder = builder.with_registered_behavior(id, plugin);
    }

    let file_store = Arc::new(FileStore::new(args.storage_dir));
    builder = builder.with_agent_state_store(file_store.clone() as Arc<dyn ThreadStore>);

    let os = builder.build().expect("failed to build AgentOs");
    let read_store: Arc<dyn ThreadReader> = file_store;

    let state = AppState {
        os: Arc::new(os),
        read_store,
    };
    let app = axum::Router::new()
        .merge(http::health_routes())
        .merge(http::thread_routes())
        .nest("/v1/ag-ui", protocol::ag_ui::http::routes())
        .nest("/v1/ai-sdk", protocol::ai_sdk_v6::http::routes())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.http_addr)
        .await
        .expect("failed to bind");
    eprintln!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server crashed");
}
