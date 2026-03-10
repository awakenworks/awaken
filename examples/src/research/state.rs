use serde::{Deserialize, Serialize};
use tirea_state_derive::State;

/// A web resource found during research.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resource {
    pub id: String,
    pub url: String,
    pub title: String,
    pub description: String,
}

/// A log entry for real-time progress tracking (Pattern 4: Generative UI).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogEntry {
    pub message: String,
    pub level: String,
    pub step: String,
}

/// Root state for the research canvas example.
///
/// Corresponds to CopilotKit's research-canvas agent state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(action = "ResearchAction")]
pub struct ResearchState {
    #[serde(default)]
    pub research_question: String,
    #[serde(default)]
    pub report: String,
    #[serde(default)]
    pub resources: Vec<Resource>,
    #[serde(default)]
    pub logs: Vec<LogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResearchAction {
    SetResearchQuestion(String),
    SetReport(String),
    SetResources(Vec<Resource>),
    SetLogs(Vec<LogEntry>),
}

impl ResearchState {
    pub fn reduce(&mut self, action: ResearchAction) {
        match action {
            ResearchAction::SetResearchQuestion(question) => self.research_question = question,
            ResearchAction::SetReport(report) => self.report = report,
            ResearchAction::SetResources(resources) => self.resources = resources,
            ResearchAction::SetLogs(logs) => self.logs = logs,
        }
    }
}
