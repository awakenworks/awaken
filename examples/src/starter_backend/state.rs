use serde::{Deserialize, Serialize};
use tirea_state_derive::State;

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(action = "StarterAction")]
pub struct StarterState {
    #[serde(default)]
    pub todos: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub theme_color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StarterAction {
    SetNotes(Vec<String>),
}

impl StarterState {
    pub fn reduce(&mut self, action: StarterAction) {
        match action {
            StarterAction::SetNotes(notes) => self.notes = notes,
        }
    }
}
