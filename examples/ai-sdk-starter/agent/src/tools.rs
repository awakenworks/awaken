use async_trait::async_trait;
use serde_json::{json, Value};
use tirea_agentos::contracts::runtime::tool_call::{
    Tool, ToolCallContext, ToolCallProgressStatus, ToolCallProgressUpdate, ToolDescriptor,
    ToolError, ToolResult,
};

use crate::state::StarterState;

pub struct GetWeatherTool;

#[async_trait]
impl Tool for GetWeatherTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_weather",
            "Get Weather",
            "Get weather details for a location.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "location": { "type": "string", "description": "City or location name" }
            },
            "required": ["location"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let location = args["location"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'location'".into()))?;

        Ok(ToolResult::success(
            "get_weather",
            json!({
                "location": location,
                "temperature_f": 70,
                "condition": "Sunny",
                "humidity_pct": 45
            }),
        ))
    }
}

pub struct GetStockPriceTool;

#[async_trait]
impl Tool for GetStockPriceTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_stock_price",
            "Get Stock Price",
            "Return a demo stock quote for the provided ticker symbol.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Ticker symbol, e.g. AAPL" }
            },
            "required": ["symbol"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let symbol = args["symbol"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'symbol'".into()))?
            .to_uppercase();

        let price = match symbol.as_str() {
            "AAPL" => 188.42_f64,
            "MSFT" => 421.10_f64,
            "NVDA" => 131.75_f64,
            _ => 99.99_f64,
        };

        Ok(ToolResult::success(
            "get_stock_price",
            json!({
                "symbol": symbol,
                "price_usd": price,
                "source": "starter-demo"
            }),
        ))
    }
}

pub struct AppendNoteTool;

#[async_trait]
impl Tool for AppendNoteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "append_note",
            "Append Note",
            "Append a note into backend-persisted state.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "note": { "type": "string", "description": "Note text to append" }
            },
            "required": ["note"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let note = args["note"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'note'".into()))?
            .trim();
        if note.is_empty() {
            return Err(ToolError::InvalidArguments(
                "Field 'note' cannot be empty".into(),
            ));
        }

        let state = ctx.state::<StarterState>("");
        let mut notes = state.notes().unwrap_or_default();
        notes.push(note.to_string());
        state
            .set_notes(notes.clone())
            .map_err(|err| ToolError::Internal(format!("failed to persist notes: {err}")))?;

        Ok(ToolResult::success(
            "append_note",
            json!({
                "added": note,
                "count": notes.len()
            }),
        ))
    }
}

pub struct ServerInfoTool;

#[async_trait]
impl Tool for ServerInfoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "serverInfo",
            "Server Info",
            "Return backend server identity and unix timestamp.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Ok(ToolResult::success(
            "serverInfo",
            json!({ "name": "ai-sdk-starter-agent", "timestamp": ts }),
        ))
    }
}

pub struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "failingTool",
            "Failing Tool",
            "Always fails for error-path validation.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::ExecutionFailed(
            "Intentional failingTool error for e2e validation".to_string(),
        ))
    }
}

pub struct FinishTool;

#[async_trait]
impl Tool for FinishTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "finish",
            "Finish",
            "Signal run completion for stop-policy checks.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "summary": { "type": "string", "description": "Completion summary" }
            },
            "required": ["summary"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let summary = args
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("done");
        Ok(ToolResult::success(
            "finish",
            json!({ "status": "done", "summary": summary }),
        ))
    }
}

pub struct ProgressDemoTool;

#[async_trait]
impl Tool for ProgressDemoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "progress_demo",
            "Progress Demo",
            "Emit tool-call progress events then return completion result.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let updates = [
            (0.1_f64, "queued"),
            (0.45_f64, "running"),
            (0.8_f64, "finishing"),
            (1.0_f64, "completed"),
        ];

        for (progress, message) in updates {
            ctx.report_tool_call_progress(ToolCallProgressUpdate {
                status: ToolCallProgressStatus::Running,
                progress: Some(progress),
                loaded: Some(progress * 100.0),
                total: Some(100.0),
                message: Some(message.to_string()),
            })
            .map_err(|err| ToolError::Internal(format!("progress emit failed: {err}")))?;
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }

        Ok(ToolResult::success(
            "progress_demo",
            json!({ "status": "ok", "progress": 1.0 }),
        ))
    }
}

pub struct AskUserQuestionTool;

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "askUserQuestion",
            "Ask User Question",
            "Frontend tool: request user input and wait for response.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "Question shown to user" }
            },
            "required": ["message"]
        }))
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::error(
            "askUserQuestion",
            "frontend tool should be intercepted before backend execution",
        ))
    }
}

pub struct SetBackgroundColorTool;

#[async_trait]
impl Tool for SetBackgroundColorTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "set_background_color",
            "Set Background Color",
            "Frontend tool: ask UI to change chat background color.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "colors": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Allowed colors for quick selection"
                }
            },
            "required": ["colors"]
        }))
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::error(
            "set_background_color",
            "frontend tool should be intercepted before backend execution",
        ))
    }
}
