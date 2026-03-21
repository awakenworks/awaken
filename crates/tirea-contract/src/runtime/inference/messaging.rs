use super::ContextMessage;

/// Post-tool runtime messages.
///
/// This buffer intentionally reuses [`ContextMessage`] so ordinary
/// conversation messages and hidden context share one data model while keeping
/// different lifecycle handling in the runtime.
#[derive(Debug, Default, Clone)]
pub struct MessagingContext {
    /// Messages emitted during tool execution.
    pub messages: Vec<ContextMessage>,
}

impl MessagingContext {
    pub fn push(&mut self, message: ContextMessage) {
        self.messages.push(message);
    }

    pub fn add_system_reminder(&mut self, text: impl Into<String>) {
        self.messages.push(ContextMessage::system_reminder(text));
    }

    pub fn add_user_message(&mut self, text: impl Into<String>) {
        self.messages.push(ContextMessage::conversation_user(text));
    }
}
