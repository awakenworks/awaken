#![allow(missing_docs)]

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tirea_agentos::composition::AgentDefinition;
use tirea_agentos::runtime::AgentOs;
use tirea_contract::runtime::tool_call::{Tool, ToolDescriptor, ToolError, ToolResult};
use tirea_contract::thread::Message;
use tirea_contract::ToolCallContext;
use tirea_contract::{AgentEvent, RunOrigin, RunRequest};

struct OpenMeteoWeatherTool {
    http: reqwest::Client,
}

impl OpenMeteoWeatherTool {
    fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client build");
        Self { http }
    }
}

#[derive(Debug, Deserialize)]
struct GeoResponse {
    #[serde(default)]
    results: Vec<GeoResult>,
}

#[derive(Debug, Deserialize)]
struct GeoResult {
    latitude: f64,
    longitude: f64,
    name: String,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    admin1: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
}

#[async_trait]
impl Tool for OpenMeteoWeatherTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_weather",
            "Get Weather",
            "Get current weather for a city using Open-Meteo (real network call)",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "City name (e.g., 'San Francisco')"
                }
            },
            "required": ["city"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let city = args
            .get("city")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'city'".to_string()))?
            .trim();
        if city.is_empty() {
            return Err(ToolError::InvalidArguments("Empty 'city'".to_string()));
        }

        let geo_url = reqwest::Url::parse_with_params(
            "https://geocoding-api.open-meteo.com/v1/search",
            [
                ("name", city),
                ("count", "1"),
                ("language", "en"),
                ("format", "json"),
            ],
        )
        .map_err(|e| ToolError::ExecutionFailed(format!("bad geocoding url: {}", e)))?;

        let geo: GeoResponse = self
            .http
            .get(geo_url)
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("geocoding request failed: {}", e)))?
            .error_for_status()
            .map_err(|e| ToolError::ExecutionFailed(format!("geocoding http error: {}", e)))?
            .json()
            .await
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("geocoding json parse failed: {}", e))
            })?;

        let r = geo
            .results
            .into_iter()
            .next()
            .ok_or_else(|| ToolError::NotFound(format!("city not found: {}", city)))?;

        let current = "temperature_2m,relative_humidity_2m,weather_code,wind_speed_10m";
        let timezone = "auto";
        let latitude = r.latitude.to_string();
        let longitude = r.longitude.to_string();
        let forecast_url = reqwest::Url::parse_with_params(
            "https://api.open-meteo.com/v1/forecast",
            [
                ("latitude", latitude.as_str()),
                ("longitude", longitude.as_str()),
                ("current", current),
                ("timezone", timezone),
            ],
        )
        .map_err(|e| ToolError::ExecutionFailed(format!("bad forecast url: {}", e)))?;

        let forecast: Value = self
            .http
            .get(forecast_url)
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("forecast request failed: {}", e)))?
            .error_for_status()
            .map_err(|e| ToolError::ExecutionFailed(format!("forecast http error: {}", e)))?
            .json()
            .await
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("forecast json parse failed: {}", e))
            })?;

        Ok(ToolResult::success(
            "get_weather",
            json!({
                "query": { "city": city },
                "location": {
                    "name": r.name,
                    "admin1": r.admin1,
                    "country": r.country,
                    "timezone": r.timezone,
                    "latitude": r.latitude,
                    "longitude": r.longitude
                },
                "current": forecast.get("current").cloned().unwrap_or(Value::Null),
                "units": forecast.get("current_units").cloned().unwrap_or(Value::Null),
            }),
        ))
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn deepseek_real_weather_agent_smoke() {
    if std::env::var("DEEPSEEK_API_KEY").is_err() {
        panic!(
            "DEEPSEEK_API_KEY not set. If it's in ~/.bashrc, run: source ~/.bashrc (or export it) before running ignored tests."
        );
    }

    // DeepSeek is flaky — retry up to 3 times.
    let mut last_err = String::new();
    for attempt in 1..=3 {
        match run_weather_agent().await {
            Ok(()) => return,
            Err(e) => {
                eprintln!("[attempt {attempt}/3] {e}");
                last_err = e;
            }
        }
    }
    panic!("all 3 attempts failed; last: {last_err}");
}

async fn run_weather_agent() -> Result<(), String> {
    let os = AgentOs::builder()
        .with_tools(std::collections::HashMap::from([(
            "get_weather".to_string(),
            std::sync::Arc::new(OpenMeteoWeatherTool::new()) as std::sync::Arc<dyn Tool>,
        )]))
        .with_agent(
            "weather",
            AgentDefinition::new("deepseek-chat").with_system_prompt(
                "You are a weather assistant.\n\
Rules:\n\
- You MUST call the tool `get_weather` exactly once before answering.\n\
- Use the tool result as the source of truth.\n\
- Answer in 2-4 short sentences.\n",
            ),
        )
        .with_agent_state_store(Arc::new(tirea_store_adapters::MemoryStore::new()))
        .build()
        .unwrap();

    let run = os
        .run_stream(RunRequest {
            agent_id: "weather".to_string(),
            thread_id: Some("real-weather-smoke".to_string()),
            run_id: None,
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: RunOrigin::default(),
            state: None,
            messages: vec![Message::user(
                "What's the current weather in San Francisco? Use the tool.",
            )],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        })
        .await
        .map_err(|e| format!("run start failed: {e}"))?;
    let stream = run.events;
    tokio::pin!(stream);

    let mut saw_weather_ready = false;
    let mut saw_weather_done = false;
    let mut text = String::new();

    let mut last_error = String::new();
    let deadline = Duration::from_secs(60);
    let timed_out = tokio::time::timeout(deadline, async {
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::ToolCallReady { ref name, .. } => {
                    if name == "get_weather" {
                        saw_weather_ready = true;
                    }
                }
                AgentEvent::ToolCallDone { ref result, .. } => {
                    if result.tool_name == "get_weather" && result.is_success() {
                        saw_weather_done = true;
                    }
                }
                AgentEvent::TextDelta { delta } => text.push_str(&delta),
                AgentEvent::RunFinish { .. } => break,
                AgentEvent::Error { ref message, .. } => {
                    last_error = message.clone();
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .is_err();

    if timed_out {
        return Err("timed out after 60s".into());
    }
    if !saw_weather_ready {
        return Err(format!(
            "model did not call get_weather (last_error: {last_error})"
        ));
    }
    if !saw_weather_done {
        return Err(format!(
            "get_weather never succeeded (last_error: {last_error})"
        ));
    }
    if text.trim().is_empty() {
        return Err(format!(
            "no assistant output produced (last_error: {last_error})"
        ));
    }
    Ok(())
}
