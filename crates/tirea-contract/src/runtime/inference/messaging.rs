/// Post-tool messaging extension: reminders and user messages.
///
/// Populated by `AddSystemReminder`, `AddUserMessage` actions
/// during `AfterToolExecute`.
#[derive(Debug, Default, Clone)]
pub struct MessagingContext {
    /// System reminders injected after tool results.
    pub reminders: Vec<String>,
    /// User messages to append after tool execution.
    pub user_messages: Vec<String>,
}
