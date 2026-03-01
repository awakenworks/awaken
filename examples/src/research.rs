use clap::Parser;
use std::sync::Arc;
use tirea_agentos::contracts::runtime::behavior::AgentBehavior;
use tirea_agentos::contracts::runtime::tool_call::Tool;
use tirea_agentos::extensions::permission::PermissionPlugin;
use tirea_agentos::orchestrator::AgentDefinition;
use tirea_examples::research::tools::*;

#[tokio::main]
async fn main() {
    let args = tirea_examples::Args::parse();

    let agent_def = AgentDefinition {
        id: "research".into(),
        model: "deepseek-chat".into(),
        system_prompt: concat!(
            "You are a research assistant. Help users research topics by searching ",
            "the web, extracting resources, and writing comprehensive reports.\n\n",
            "Workflow:\n",
            "1. Set the research question with set_research_question\n",
            "2. Search for information with search\n",
            "3. Extract useful resources with extract_resources\n",
            "4. Write a report with write_report\n\n",
            "Always keep the user informed of your progress."
        )
        .into(),
        max_rounds: 15,
        ..Default::default()
    };

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(SearchTool),
        Arc::new(WriteReportTool),
        Arc::new(SetQuestionTool),
        Arc::new(DeleteResourcesTool),
        Arc::new(ExtractResourcesTool),
    ];

    let plugins: Vec<(String, Arc<dyn AgentBehavior>)> =
        vec![("permission".into(), Arc::new(PermissionPlugin))];

    tirea_examples::serve(args, agent_def, tools, plugins).await;
}
