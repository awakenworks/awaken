//! Request preparation utilities.

use awaken_server_contract::contract::message::Message;
use serde_json::Value;

use crate::routes::ApiError;

/// Validate messages and generate thread_id.
pub fn prepare_run_inputs(
    thread_id: Option<String>,
    messages: Vec<Message>,
) -> Result<(String, Vec<Message>), ApiError> {
    if messages.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one message is required".to_string(),
        ));
    }
    let thread_id = thread_id
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    Ok((thread_id, messages))
}

/// Inject frontend context as a separate internal system message.
///
/// Inserts a `<frontend-context>` tagged message at position 0 so it
/// precedes the conversation. Empty or null context is skipped.
pub fn inject_frontend_context(mut messages: Vec<Message>, context: Option<Value>) -> Vec<Message> {
    if let Some(ctx) = context
        && !ctx.is_null()
    {
        let ctx_text = serde_json::to_string(&ctx).unwrap_or_default();
        if !ctx_text.is_empty() && ctx_text != "{}" {
            let ctx_msg = Message::internal_system(format!(
                "<frontend-context>\n{ctx_text}\n</frontend-context>"
            ));
            messages.insert(0, ctx_msg);
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_server_contract::contract::message::Message;

    #[test]
    fn generates_thread_id_when_none() {
        let msgs = vec![Message::user("hello")];
        let (tid, _) = prepare_run_inputs(None, msgs).unwrap();
        assert!(!tid.is_empty());
    }

    #[test]
    fn empty_messages_errors() {
        let result = prepare_run_inputs(None, vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn preserves_given_thread_id() {
        let msgs = vec![Message::user("hello")];
        let (tid, _) = prepare_run_inputs(Some("my-thread".into()), msgs).unwrap();
        assert_eq!(tid, "my-thread");
    }

    #[test]
    fn trims_whitespace_from_thread_id() {
        let msgs = vec![Message::user("hello")];
        let (tid, _) = prepare_run_inputs(Some("  spaced  ".into()), msgs).unwrap();
        assert_eq!(tid, "spaced");
    }

    #[test]
    fn blank_thread_id_generates_new() {
        let msgs = vec![Message::user("hello")];
        let (tid, _) = prepare_run_inputs(Some("   ".into()), msgs).unwrap();
        assert!(!tid.is_empty());
        assert_ne!(tid.trim(), "");
    }

    #[test]
    fn inject_context_inserts_internal_system_message() {
        use awaken_server_contract::contract::message::{Role, Visibility};
        let msgs = vec![Message::user("hello")];
        let result =
            inject_frontend_context(msgs, Some(serde_json::json!({"todos": ["buy milk"]})));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, Role::System);
        assert_eq!(result[0].visibility, Visibility::Internal);
        assert!(result[0].text().contains("<frontend-context>"));
        assert!(result[0].text().contains("todos"));
        assert_eq!(result[1].text(), "hello");
    }

    #[test]
    fn inject_context_skips_empty_object() {
        let msgs = vec![Message::user("hello")];
        let result = inject_frontend_context(msgs, Some(serde_json::json!({})));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn inject_context_skips_null() {
        let msgs = vec![Message::user("hello")];
        let result = inject_frontend_context(msgs, None);
        assert_eq!(result.len(), 1);
    }
}
