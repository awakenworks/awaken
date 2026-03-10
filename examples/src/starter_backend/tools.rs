use async_trait::async_trait;
use serde_json::{json, Value};
use tirea_agentos::contracts::runtime::tool_call::{
    Tool, ToolCallContext, ToolCallProgressStatus, ToolCallProgressUpdate, ToolDescriptor,
    ToolError, ToolExecutionEffect, ToolResult,
};
use tirea_agentos::contracts::AnyStateAction;

use crate::starter_backend::state::{StarterAction, StarterState};

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
        Ok(<Self as Tool>::execute_effect(self, args, ctx)
            .await?
            .result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let note = args["note"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'note'".into()))?
            .trim();
        if note.is_empty() {
            return Err(ToolError::InvalidArguments(
                "Field 'note' cannot be empty".into(),
            ));
        }

        let mut notes = ctx.snapshot_of::<StarterState>().unwrap_or_default().notes;
        notes.push(note.to_string());

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "append_note",
            json!({
                "added": note,
                "count": notes.len()
            }),
        ))
        .with_action(AnyStateAction::new::<StarterState>(
            StarterAction::SetNotes(notes),
        )))
    }
}

pub struct ServerInfoTool {
    service_name: String,
}

impl ServerInfoTool {
    pub fn new(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
        }
    }
}

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
            json!({ "name": self.service_name, "timestamp": ts }),
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
            "Run a long-running task with real-time progress updates. \
             Use the 'scenario' parameter to pick a demo pattern.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "scenario": {
                    "type": "string",
                    "description": "Progress scenario to demonstrate",
                    "enum": ["default", "data_pipeline", "deploy", "slow_build", "multi_phase"],
                    "default": "default"
                }
            },
            "required": []
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let scenario = args["scenario"].as_str().unwrap_or("default");
        let report = |status: ToolCallProgressStatus,
                      progress: f64,
                      loaded: f64,
                      total: f64,
                      message: &str|
         -> Result<(), ToolError> {
            ctx.report_tool_call_progress(ToolCallProgressUpdate {
                status,
                progress: Some(progress),
                loaded: Some(loaded),
                total: Some(total),
                message: Some(message.to_string()),
            })
            .map_err(|err| ToolError::Internal(format!("progress emit failed: {err}")))
        };

        match scenario {
            "data_pipeline" => {
                // Simulates an ETL pipeline: extract → transform → load
                let phases = [
                    (0.0, "Connecting to data source..."),
                    (0.05, "Extracting records (0/500)..."),
                    (0.15, "Extracting records (100/500)..."),
                    (0.25, "Extracting records (250/500)..."),
                    (0.35, "Extraction complete (500 records)"),
                    (0.40, "Transforming: schema validation..."),
                    (0.50, "Transforming: field mapping..."),
                    (0.60, "Transforming: deduplication..."),
                    (0.65, "Transform complete"),
                    (0.70, "Loading: batch 1/3..."),
                    (0.80, "Loading: batch 2/3..."),
                    (0.90, "Loading: batch 3/3..."),
                    (0.95, "Verifying data integrity..."),
                    (1.0, "Pipeline complete: 500 records processed"),
                ];
                for (progress, message) in phases {
                    let status = if progress < 1.0 {
                        ToolCallProgressStatus::Running
                    } else {
                        ToolCallProgressStatus::Done
                    };
                    report(status, progress, progress * 500.0, 500.0, message)?;
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                }
                Ok(ToolResult::success(
                    "progress_demo",
                    json!({ "scenario": "data_pipeline", "records_processed": 500, "status": "ok" }),
                ))
            }
            "deploy" => {
                // Simulates a deployment pipeline
                let phases = [
                    (
                        ToolCallProgressStatus::Pending,
                        0.0,
                        "Queued for deployment...",
                    ),
                    (
                        ToolCallProgressStatus::Running,
                        0.10,
                        "Building container image...",
                    ),
                    (
                        ToolCallProgressStatus::Running,
                        0.25,
                        "Pushing image to registry...",
                    ),
                    (
                        ToolCallProgressStatus::Running,
                        0.35,
                        "Running pre-deploy checks...",
                    ),
                    (
                        ToolCallProgressStatus::Running,
                        0.45,
                        "Rolling update: 0/3 pods ready",
                    ),
                    (
                        ToolCallProgressStatus::Running,
                        0.60,
                        "Rolling update: 1/3 pods ready",
                    ),
                    (
                        ToolCallProgressStatus::Running,
                        0.75,
                        "Rolling update: 2/3 pods ready",
                    ),
                    (
                        ToolCallProgressStatus::Running,
                        0.85,
                        "Rolling update: 3/3 pods ready",
                    ),
                    (
                        ToolCallProgressStatus::Running,
                        0.92,
                        "Running health checks...",
                    ),
                    (
                        ToolCallProgressStatus::Running,
                        0.97,
                        "Switching traffic...",
                    ),
                    (
                        ToolCallProgressStatus::Done,
                        1.0,
                        "Deployment successful (v2.4.1)",
                    ),
                ];
                for (status, progress, message) in phases {
                    report(status, progress, progress * 100.0, 100.0, message)?;
                    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                }
                Ok(ToolResult::success(
                    "progress_demo",
                    json!({ "scenario": "deploy", "version": "v2.4.1", "pods": 3, "status": "ok" }),
                ))
            }
            "slow_build" => {
                // Simulates a long compile/build with fine-grained percentage ticks
                let total_steps = 20u32;
                report(
                    ToolCallProgressStatus::Pending,
                    0.0,
                    0.0,
                    total_steps as f64,
                    "Build queued...",
                )?;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                for step in 1..=total_steps {
                    let progress = step as f64 / total_steps as f64;
                    let message = match step {
                        1 => "Resolving dependencies...".to_string(),
                        2..=4 => format!("Compiling crate {}/{total_steps}...", step),
                        5 => "Linking stage 1...".to_string(),
                        6..=15 => format!("Compiling crate {}/{total_steps}...", step),
                        16 => "Linking stage 2...".to_string(),
                        17..=19 => format!("Running tests {}/3...", step - 16),
                        _ => "Build successful".to_string(),
                    };
                    let status = if step == total_steps {
                        ToolCallProgressStatus::Done
                    } else {
                        ToolCallProgressStatus::Running
                    };
                    report(status, progress, step as f64, total_steps as f64, &message)?;
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                }
                Ok(ToolResult::success(
                    "progress_demo",
                    json!({ "scenario": "slow_build", "crates_compiled": total_steps, "status": "ok" }),
                ))
            }
            "multi_phase" => {
                // Three distinct phases with phase labels
                let phases: &[(&str, &[(f64, &str)])] = &[
                    (
                        "Phase 1: Analysis",
                        &[
                            (0.0, "Scanning codebase..."),
                            (0.08, "Parsing AST (125 files)..."),
                            (0.15, "Building dependency graph..."),
                            (0.22, "Analysis complete"),
                        ],
                    ),
                    (
                        "Phase 2: Processing",
                        &[
                            (0.25, "Applying transformations..."),
                            (0.35, "Optimizing hot paths..."),
                            (0.45, "Generating intermediate output..."),
                            (0.55, "Processing complete"),
                        ],
                    ),
                    (
                        "Phase 3: Finalization",
                        &[
                            (0.60, "Writing output artifacts..."),
                            (0.70, "Computing checksums..."),
                            (0.80, "Uploading to artifact store..."),
                            (0.90, "Cleaning up temp files..."),
                            (1.0, "All phases complete"),
                        ],
                    ),
                ];
                for (phase_name, steps) in phases {
                    for (progress, step_msg) in *steps {
                        let msg = format!("[{phase_name}] {step_msg}");
                        let status = if *progress >= 1.0 {
                            ToolCallProgressStatus::Done
                        } else {
                            ToolCallProgressStatus::Running
                        };
                        report(status, *progress, progress * 100.0, 100.0, &msg)?;
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
                Ok(ToolResult::success(
                    "progress_demo",
                    json!({ "scenario": "multi_phase", "phases": 3, "status": "ok" }),
                ))
            }
            _ => {
                // Default: steady 10-step progress over ~5 seconds
                let steps = 10u32;
                for step in 0..=steps {
                    let progress = step as f64 / steps as f64;
                    let status = if step == steps {
                        ToolCallProgressStatus::Done
                    } else if step == 0 {
                        ToolCallProgressStatus::Pending
                    } else {
                        ToolCallProgressStatus::Running
                    };
                    let message = match step {
                        0 => "Initializing...".to_string(),
                        s if s == steps => "Complete".to_string(),
                        s => format!("Processing step {s}/{steps}..."),
                    };
                    report(status, progress, step as f64, steps as f64, &message)?;
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Ok(ToolResult::success(
                    "progress_demo",
                    json!({ "scenario": "default", "steps": steps, "status": "ok" }),
                ))
            }
        }
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
