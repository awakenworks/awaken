use awaken_contract::state::{KeyScope, MergeStrategy, StateKey};
use serde::{Deserialize, Serialize};

/// A web resource found during research.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resource {
    pub id: String,
    pub url: String,
    pub title: String,
    pub description: String,
}

/// A log entry for real-time progress tracking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogEntry {
    pub message: String,
    pub level: String,
    pub step: String,
}

/// Root state value for the research example.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResearchStateValue {
    #[serde(default)]
    pub research_question: String,
    #[serde(default)]
    pub report: String,
    #[serde(default)]
    pub resources: Vec<Resource>,
    #[serde(default)]
    pub logs: Vec<LogEntry>,
}

/// State update actions for the research state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResearchAction {
    SetResearchQuestion(String),
    SetReport(String),
    SetResources(Vec<Resource>),
    SetLogs(Vec<LogEntry>),
}

/// State key binding for the research example.
pub struct ResearchState;

impl StateKey for ResearchState {
    const KEY: &'static str = "research";
    const MERGE: MergeStrategy = MergeStrategy::Exclusive;
    const SCOPE: KeyScope = KeyScope::Run;

    type Value = ResearchStateValue;
    type Update = ResearchAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            ResearchAction::SetResearchQuestion(q) => value.research_question = q,
            ResearchAction::SetReport(r) => value.report = r,
            ResearchAction::SetResources(r) => value.resources = r,
            ResearchAction::SetLogs(l) => value.logs = l,
        }
    }
}
