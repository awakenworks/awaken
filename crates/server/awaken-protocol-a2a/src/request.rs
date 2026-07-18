//! A2A request decoding: resolve the context (thread) id and convert the inbound
//! message to neutral runtime input. All A2A-specific parsing lives here so the
//! router only routes.

use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};

use crate::types::{Part, SendMessageRequest};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn next(prefix: &str) -> String {
    format!("{prefix}-{}", SEQ.fetch_add(1, Ordering::SeqCst))
}

/// A decoded `message:send` request ready for the runtime: the A2A `contextId`
/// maps to the neutral thread id, the message text is the turn input (or, when the
/// thread has an awaiting run, the tool answer the router delivers on resume).
pub struct Processed {
    pub thread_id: String,
    pub agent_id: Option<String>,
    pub text: String,
    /// The neutral user message for a fresh turn.
    pub message: Message,
}

/// Decode a `message:send` request: the `contextId` (or `taskId`) is the thread;
/// absent, a fresh one is minted. The message's text parts become the turn input.
pub fn process(req: SendMessageRequest, path_agent: Option<String>) -> Processed {
    let thread_id = req
        .message
        .context_id
        .clone()
        .or_else(|| req.message.task_id.clone())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| next("thread"));

    let text = req.message.text();
    let message_id = req
        .message
        .message_id
        .clone()
        .unwrap_or_else(|| next("msg"));
    let blocks: Vec<ContentBlock> = req.message.parts.iter().filter_map(part_to_block).collect();
    let message = Message::new(MessageId(message_id), Role::User, blocks);

    Processed {
        thread_id,
        // The path agent (e.g. `/v1/a2a/agents/{agent}/...`) wins over a body
        // selector; either falls back to the host default.
        agent_id: path_agent.or(req.agent_id),
        text,
        message,
    }
}

/// Convert an A2A part into a neutral content block: a `text` part becomes text; a
/// `file` part with an image mime becomes an image block (inline base64 `bytes`
/// preferred, else a remote `uri`). Other parts are dropped.
fn part_to_block(part: &Part) -> Option<ContentBlock> {
    if let Some(text) = &part.text {
        return (!text.is_empty()).then(|| ContentBlock::text(text));
    }
    let file = part.file.as_ref()?;
    let mime = file
        .mime_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    if !mime.starts_with("image/") {
        return None;
    }
    if let Some(bytes) = &file.bytes {
        Some(ContentBlock::image_base64(mime, bytes))
    } else {
        file.uri.as_ref().map(ContentBlock::image_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageRole, Part, SendMessage};

    fn req(context: Option<&str>, text: &str) -> SendMessageRequest {
        SendMessageRequest {
            agent_id: None,
            message: SendMessage {
                message_id: Some("m1".into()),
                context_id: context.map(Into::into),
                task_id: None,
                role: MessageRole::User,
                parts: vec![Part::text(text)],
            },
        }
    }

    #[test]
    fn context_id_becomes_the_thread() {
        let p = process(req(Some("ctx-1"), "hi"), None);
        assert_eq!(p.thread_id, "ctx-1");
        assert_eq!(p.text, "hi");
        assert_eq!(p.message.role, Role::User);
    }

    #[test]
    fn absent_context_mints_a_thread() {
        let p = process(req(None, "hi"), None);
        assert!(p.thread_id.starts_with("thread-"));
    }

    #[test]
    fn image_file_part_becomes_an_image_block() {
        use crate::types::FilePart;
        let r = SendMessageRequest {
            agent_id: None,
            message: SendMessage {
                message_id: Some("m1".into()),
                context_id: Some("c".into()),
                task_id: None,
                role: MessageRole::User,
                parts: vec![
                    Part {
                        kind: Some("file".into()),
                        text: None,
                        file: Some(FilePart {
                            bytes: Some("AAAA".into()),
                            uri: None,
                            mime_type: Some("image/png".into()),
                        }),
                    },
                    Part::text("what is this"),
                ],
            },
        };
        let p = process(r, None);
        assert_eq!(p.message.content.len(), 2);
        assert!(matches!(p.message.content[0], ContentBlock::Image { .. }));
        assert_eq!(p.message.content[1], ContentBlock::text("what is this"));
    }

    #[test]
    fn path_agent_wins_over_body_selector() {
        let mut r = req(Some("c"), "hi");
        r.agent_id = Some("body-agent".into());
        let p = process(r, Some("path-agent".into()));
        assert_eq!(p.agent_id.as_deref(), Some("path-agent"));
    }

    fn file_req(file: crate::types::FilePart) -> SendMessageRequest {
        SendMessageRequest {
            agent_id: None,
            message: SendMessage {
                message_id: Some("m1".into()),
                context_id: Some("c".into()),
                task_id: None,
                role: MessageRole::User,
                parts: vec![Part {
                    kind: Some("file".into()),
                    text: None,
                    file: Some(file),
                }],
            },
        }
    }

    #[test]
    fn task_id_is_the_thread_when_no_context_id() {
        let mut r = req(None, "hi");
        r.message.task_id = Some("task-9".into());
        let p = process(r, None);
        assert_eq!(p.thread_id, "task-9");
    }

    #[test]
    fn whitespace_only_context_is_trimmed_then_minted() {
        let p = process(req(Some("   "), "hi"), None);
        assert!(p.thread_id.starts_with("thread-"));
    }

    #[test]
    fn uri_image_file_part_becomes_an_image_url_block() {
        use crate::types::FilePart;
        use awaken_agent_contract::agent::content::ImageSource;
        let p = process(
            file_req(FilePart {
                bytes: None,
                uri: Some("https://x/y.png".into()),
                mime_type: Some("image/png".into()),
            }),
            None,
        );
        assert!(matches!(
            &p.message.content[0],
            ContentBlock::Image { source: ImageSource::Url { url } } if url == "https://x/y.png"
        ));
    }

    #[test]
    fn non_image_file_part_is_dropped() {
        use crate::types::FilePart;
        let p = process(
            file_req(FilePart {
                bytes: Some("AAAA".into()),
                uri: None,
                mime_type: Some("application/pdf".into()),
            }),
            None,
        );
        assert!(p.message.content.is_empty());
    }

    #[test]
    fn image_file_part_without_bytes_or_uri_is_dropped() {
        use crate::types::FilePart;
        let p = process(
            file_req(FilePart {
                bytes: None,
                uri: None,
                mime_type: Some("image/png".into()),
            }),
            None,
        );
        assert!(p.message.content.is_empty());
    }

    #[test]
    fn absent_message_id_is_minted() {
        let mut r = req(Some("c"), "hi");
        r.message.message_id = None;
        let p = process(r, None);
        assert!(p.message.id.0.starts_with("msg-"));
    }

    #[test]
    fn a_data_kind_part_with_no_text_or_file_is_dropped() {
        let r = SendMessageRequest {
            agent_id: None,
            message: SendMessage {
                message_id: Some("m1".into()),
                context_id: Some("c".into()),
                task_id: None,
                role: MessageRole::User,
                parts: vec![
                    Part {
                        kind: Some("data".into()),
                        text: None,
                        file: None,
                    },
                    Part::text("keep me"),
                ],
            },
        };
        let p = process(r, None);
        // The unsupported `data` part is dropped; only the text survives.
        assert_eq!(p.message.content.len(), 1);
        assert_eq!(p.message.content[0], ContentBlock::text("keep me"));
    }

    #[test]
    fn the_inbound_message_is_always_the_user_turn() {
        // `message/send` carries the caller's turn; the wire `role` is not honored.
        let mut r = req(Some("c"), "hi");
        r.message.role = MessageRole::Agent;
        let p = process(r, None);
        assert_eq!(p.message.role, Role::User);
    }
}
