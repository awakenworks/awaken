//! A2A request decoding: resolve the context (thread) id and convert the inbound
//! message to neutral runtime input. All A2A-specific parsing lives here so the
//! router only routes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_session_contract::{Pending, RunApplicationError, RunResume};
use serde_json::Value;

use crate::types::{Part, SendMessageRequest};

fn next(prefix: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{:x}-{nanos:x}-{sequence:x}", std::process::id())
}

/// A decoded `message:send` request ready for the runtime: the A2A `contextId`
/// maps to the neutral thread id, the message text is the turn input (or, when the
/// thread has an awaiting run, the tool answer the router delivers on resume).
pub struct Processed {
    pub thread_id: String,
    pub task_id: String,
    pub agent_id: Option<String>,
    pub text: String,
    /// An explicit structured decision carried by an A2A `DataPart`. Text is
    /// never interpreted as authorization.
    pub(crate) approval: Option<ApprovalDecision>,
    /// The neutral user message for a fresh turn.
    pub message: Message,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    Decision { allow: bool, note: Option<String> },
    Invalid(String),
}

/// Decode a `message:send` request: the `contextId` (or `taskId`) is the thread;
/// absent, a fresh one is minted. The message's text parts become the turn input.
pub fn process(req: SendMessageRequest, path_agent: Option<String>) -> Processed {
    let requested_task_id = req
        .message
        .task_id
        .clone()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty());
    let thread_id = req
        .message
        .context_id
        .clone()
        .or_else(|| requested_task_id.clone())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| next("thread"));

    let text = req.message.text();
    let approval = approval_decision(&req.message.parts);
    let message_id = req.message.message_id.clone();
    let blocks: Vec<ContentBlock> = req.message.parts.iter().filter_map(part_to_block).collect();
    let message = Message::new(MessageId(message_id), Role::User, blocks);

    Processed {
        thread_id,
        task_id: requested_task_id.unwrap_or_else(|| next("task")),
        // The path agent (e.g. `/v1/a2a/agents/{agent}/...`) wins over a body
        // selector; either falls back to the host default.
        agent_id: path_agent.or(req.agent_id),
        text,
        approval,
        message,
    }
}

fn approval_decision(parts: &[Part]) -> Option<ApprovalDecision> {
    let mut decisions = parts.iter().filter_map(|part| match part {
        Part::Data { data, .. }
            if data.get("type").and_then(Value::as_str) == Some("tool-approval") =>
        {
            Some(data)
        }
        _ => None,
    });
    let data = decisions.next()?;
    if decisions.next().is_some() {
        return Some(ApprovalDecision::Invalid(
            "A2A message carries multiple tool-approval decisions".into(),
        ));
    }
    let Some(allow) = data.get("allow").and_then(Value::as_bool) else {
        return Some(ApprovalDecision::Invalid(
            "A2A tool-approval data requires a boolean `allow`".into(),
        ));
    };
    let note = match data.get("note") {
        None => None,
        Some(Value::String(note)) => Some(note.clone()),
        Some(_) => {
            return Some(ApprovalDecision::Invalid(
                "A2A tool-approval `note` must be a string".into(),
            ));
        }
    };
    Some(ApprovalDecision::Decision { allow, note })
}

/// Convert decoded A2A input to the one neutral resume variant authorized by
/// the pending tool binding. This stays with request decoding so the router
/// never creates a second interpretation of approval data.
pub(crate) fn resume_for_pending(
    text: &str,
    approval: Option<&ApprovalDecision>,
    pending: &Pending,
) -> Result<RunResume, RunApplicationError> {
    if pending.client_executed {
        if approval.is_some() {
            return Err(RunApplicationError::bad_request(
                "A2A tool-approval decision cannot answer a client-executed tool",
            ));
        }
        Ok(RunResume::ClientResult {
            content: vec![ContentBlock::text(text)],
            is_error: false,
        })
    } else {
        match approval {
            Some(ApprovalDecision::Decision { allow, note }) => Ok(RunResume::Confirm {
                allow: *allow,
                note: note.clone(),
            }),
            Some(ApprovalDecision::Invalid(message)) => {
                Err(RunApplicationError::bad_request(message.clone()))
            }
            None => Err(RunApplicationError::bad_request(
                "A2A built-in tool approval requires a structured tool-approval DataPart",
            )),
        }
    }
}

/// Convert an A2A part into a neutral content block: a `text` part becomes text; a
/// `file` part with an image mime becomes an image block (inline base64 `bytes`
/// preferred, else a remote `uri`). Other parts are dropped.
fn part_to_block(part: &Part) -> Option<ContentBlock> {
    match part {
        Part::Text { text, .. } => (!text.is_empty()).then(|| ContentBlock::text(text)),
        Part::Data { .. } => None,
        Part::File { file, .. } => match file {
            crate::types::FilePart::Bytes(file) => file
                .mime_type
                .as_deref()
                .filter(|mime| mime.starts_with("image/"))
                .map(|mime| ContentBlock::image_base64(mime, &file.bytes)),
            crate::types::FilePart::Uri(file) => file
                .mime_type
                .as_deref()
                .filter(|mime| mime.starts_with("image/"))
                .map(|_| ContentBlock::image_url(&file.uri)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageKind, MessageRole, Part};

    fn wire_message(context: Option<&str>, parts: Vec<Part>) -> crate::types::Message {
        crate::types::Message {
            kind: MessageKind::Message,
            message_id: "m1".into(),
            context_id: context.map(Into::into),
            task_id: None,
            role: MessageRole::User,
            parts,
            extensions: Vec::new(),
            metadata: None,
            reference_task_ids: Vec::new(),
        }
    }

    fn pending(client_executed: bool) -> Pending {
        Pending {
            tool_use_id: "c1".into(),
            name: "t".into(),
            input: serde_json::Value::Null,
            client_executed,
        }
    }

    #[test]
    fn resume_delivers_text_only_to_a_client_executed_tool() {
        let resume = resume_for_pending("the answer", None, &pending(true)).unwrap();
        assert!(
            matches!(resume, RunResume::ClientResult { content, is_error: false } if content == vec![ContentBlock::text("the answer")])
        );
    }

    #[test]
    fn resume_requires_an_explicit_builtin_tool_decision() {
        /* Resume FMECA decision table. C1 pending binding is client-executed;
         * C2 an explicit approval decision exists; C3 it is valid; C4 allow bit.
         * R1=C1+!C2 => exact ClientResult; R2=!C1+C2+C3+C4 => allow;
         * R3=!C1+C2+C3+!C4 => deny; R4=!C1+(!C2|!C3) => bad request;
         * R5=C1+C2 => bad request. Effects are mutually exclusive and no text
         * value can become authorization.
         */
        let allow = ApprovalDecision::Decision {
            allow: true,
            note: Some("reviewed".into()),
        };
        assert!(
            matches!(
                resume_for_pending("ignored", Some(&allow), &pending(false)).unwrap(),
                RunResume::Confirm { allow: true, note: Some(note) } if note == "reviewed"
            ),
            "R2"
        );
        let deny = ApprovalDecision::Decision {
            allow: false,
            note: Some("unsafe".into()),
        };
        assert!(
            matches!(
                resume_for_pending("ignored", Some(&deny), &pending(false)).unwrap(),
                RunResume::Confirm { allow: false, note: Some(note) } if note == "unsafe"
            ),
            "R3"
        );
        assert!(
            resume_for_pending("approve", None, &pending(false)).is_err(),
            "R4"
        );
        assert!(
            resume_for_pending(
                "ignored",
                Some(&ApprovalDecision::Invalid("bad".into())),
                &pending(false)
            )
            .is_err(),
            "R4 malformed"
        );
        assert!(
            resume_for_pending("result", Some(&allow), &pending(true)).is_err(),
            "R5"
        );
    }

    fn req(context: Option<&str>, text: &str) -> SendMessageRequest {
        SendMessageRequest {
            agent_id: None,
            configuration: None,
            metadata: None,
            message: wire_message(context, vec![Part::text(text)]),
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
        use crate::types::{FilePart, FileWithBytes};
        let r = SendMessageRequest {
            agent_id: None,
            configuration: None,
            metadata: None,
            message: wire_message(
                Some("c"),
                vec![
                    Part::File {
                        file: FilePart::Bytes(FileWithBytes {
                            bytes: "AAAA".into(),
                            mime_type: Some("image/png".into()),
                            name: None,
                        }),
                        metadata: None,
                    },
                    Part::text("what is this"),
                ],
            ),
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
            configuration: None,
            metadata: None,
            message: wire_message(
                Some("c"),
                vec![Part::File {
                    file,
                    metadata: None,
                }],
            ),
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
        use crate::types::{FilePart, FileWithUri};
        use awaken_agent_contract::agent::content::ImageSource;
        let p = process(
            file_req(FilePart::Uri(FileWithUri {
                uri: "https://x/y.png".into(),
                mime_type: Some("image/png".into()),
                name: None,
            })),
            None,
        );
        assert!(matches!(
            &p.message.content[0],
            ContentBlock::Image { source: ImageSource::Url { url } } if url == "https://x/y.png"
        ));
    }

    #[test]
    fn non_image_file_part_is_dropped() {
        use crate::types::{FilePart, FileWithBytes};
        let p = process(
            file_req(FilePart::Bytes(FileWithBytes {
                bytes: "AAAA".into(),
                mime_type: Some("application/pdf".into()),
                name: None,
            })),
            None,
        );
        assert!(p.message.content.is_empty());
    }

    #[test]
    fn a_data_kind_part_with_no_text_or_file_is_dropped() {
        // Decision table at the runtime ACL:
        // | A2A part | neutral mapping | prompt effect |
        // | data     | none            | none          |
        // | text     | Text            | retained      |
        // A2A owns arbitrary data; inventing a generic neutral ContentBlock would
        // leak wire vocabulary into every protocol and model adapter.
        let r = SendMessageRequest {
            agent_id: None,
            configuration: None,
            metadata: None,
            message: wire_message(
                Some("c"),
                vec![
                    Part::Data {
                        data: std::collections::BTreeMap::from([(
                            "ignored".into(),
                            serde_json::json!(true),
                        )]),
                        metadata: None,
                    },
                    Part::text("keep me"),
                ],
            ),
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

    #[test]
    fn structured_tool_approval_decision_table_is_explicit_and_fail_closed() {
        /* A2A approval FMECA graph. C1 one DataPart has type=tool-approval;
         * C2 allow is boolean; C3 note is absent/string; C4 duplicate decision.
         * Effects: E1 exact allow/deny decision; E2 invalid input, never implicit
         * authorization. Rules A1=C1+C2+C3=>E1; A2=!C2|!C3|C4=>E2;
         * A3=!C1=>no decision (the router then rejects text-only approvals).
         */
        let decision = |data: std::collections::BTreeMap<String, Value>| Part::Data {
            data,
            metadata: None,
        };
        let valid = decision(std::collections::BTreeMap::from([
            ("type".into(), Value::String("tool-approval".into())),
            ("allow".into(), Value::Bool(false)),
            ("note".into(), Value::String("operator denied".into())),
        ]));
        assert_eq!(
            approval_decision(std::slice::from_ref(&valid)),
            Some(ApprovalDecision::Decision {
                allow: false,
                note: Some("operator denied".into()),
            }),
            "A1"
        );
        assert!(
            matches!(
                approval_decision(&[decision(std::collections::BTreeMap::from([
                    ("type".into(), Value::String("tool-approval".into())),
                    ("allow".into(), Value::String("deny".into())),
                ]))]),
                Some(ApprovalDecision::Invalid(_))
            ),
            "A2 malformed"
        );
        assert!(
            matches!(
                approval_decision(&[valid.clone(), valid]),
                Some(ApprovalDecision::Invalid(_))
            ),
            "A2 duplicate"
        );
        assert_eq!(approval_decision(&[Part::text("deny")]), None, "A3");
    }
}
