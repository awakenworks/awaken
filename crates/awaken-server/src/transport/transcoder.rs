//! Protocol-specific event encoding utilities.

use awaken_server_contract::contract::event::AgentEvent;
use awaken_server_contract::contract::transport::Transcoder;
use bytes::Bytes;
use serde::Serialize;

use crate::http_sse::format_sse_data;

/// Encode a single agent event through a transcoder into SSE data frames.
pub fn encode_event_to_sse<E>(encoder: &mut E, event: &AgentEvent) -> Vec<Bytes>
where
    E: Transcoder<Input = AgentEvent>,
    E::Output: Serialize,
{
    encoder
        .transcode(event)
        .into_iter()
        .filter_map(|item| match serde_json::to_string(&item) {
            Ok(json) => Some(format_sse_data(&json)),
            Err(error) => {
                tracing::warn!(error = %error, "failed to serialize transcoded event");
                None
            }
        })
        .collect()
}

/// Encode prologue events from a transcoder into SSE data frames.
pub fn encode_prologue_to_sse<E>(encoder: &mut E) -> Vec<Bytes>
where
    E: Transcoder<Input = AgentEvent>,
    E::Output: Serialize,
{
    encoder
        .prologue()
        .into_iter()
        .filter_map(|item| match serde_json::to_string(&item) {
            Ok(json) => Some(format_sse_data(&json)),
            Err(error) => {
                tracing::warn!(error = %error, "failed to serialize transcoder prologue");
                None
            }
        })
        .collect()
}

/// Encode epilogue events from a transcoder into SSE data frames.
pub fn encode_epilogue_to_sse<E>(encoder: &mut E) -> Vec<Bytes>
where
    E: Transcoder<Input = AgentEvent>,
    E::Output: Serialize,
{
    encoder
        .epilogue()
        .into_iter()
        .filter_map(|item| match serde_json::to_string(&item) {
            Ok(json) => Some(format_sse_data(&json)),
            Err(error) => {
                tracing::warn!(error = %error, "failed to serialize transcoder epilogue");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_server_contract::contract::transport::Identity;

    #[test]
    fn encode_identity_event_to_sse() {
        let mut encoder = Identity::<AgentEvent>::default();
        let event = AgentEvent::TextDelta { delta: "hi".into() };
        let frames = encode_event_to_sse(&mut encoder, &event);
        assert_eq!(frames.len(), 1);
        let frame = String::from_utf8(frames[0].to_vec()).unwrap();
        assert!(frame.starts_with("data: "));
        assert!(frame.contains("text_delta"));
        assert!(frame.ends_with("\n\n"));
    }

    #[test]
    fn encode_prologue_empty_for_identity() {
        let mut encoder = Identity::<AgentEvent>::default();
        let frames = encode_prologue_to_sse(&mut encoder);
        assert!(frames.is_empty());
    }

    #[test]
    fn encode_epilogue_empty_for_identity() {
        let mut encoder = Identity::<AgentEvent>::default();
        let frames = encode_epilogue_to_sse(&mut encoder);
        assert!(frames.is_empty());
    }

    #[test]
    fn encode_ai_sdk_event_to_sse() {
        use crate::protocols::ai_sdk_v6::encoder::AiSdkEncoder;
        let mut encoder = AiSdkEncoder::new();
        let event = AgentEvent::TextDelta {
            delta: "hello".into(),
        };
        let frames = encode_event_to_sse(&mut encoder, &event);
        assert!(!frames.is_empty());
        let frame = String::from_utf8(frames[0].to_vec()).unwrap();
        assert!(frame.starts_with("data: "));
    }

    #[test]
    fn encode_ag_ui_event_to_sse() {
        use crate::protocols::ag_ui::encoder::AgUiEncoder;
        let mut encoder = AgUiEncoder::new();
        encoder.on_agent_event(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            identity: None,
            parent_run_id: None,
        });
        let event = AgentEvent::TextDelta {
            delta: "hello".into(),
        };
        let frames = encode_event_to_sse(&mut encoder, &event);
        assert!(!frames.is_empty());
    }

    #[test]
    fn encode_acp_event_to_sse() {
        use crate::protocols::acp::encoder::AcpEncoder;
        let mut encoder = AcpEncoder::new();
        let event = AgentEvent::TextDelta {
            delta: "hello".into(),
        };
        let frames = encode_event_to_sse(&mut encoder, &event);
        assert_eq!(frames.len(), 1);
    }

    // ── Edge case tests ──

    #[test]
    fn encode_step_end_event_identity() {
        let mut encoder = Identity::<AgentEvent>::default();
        let event = AgentEvent::StepEnd;
        let frames = encode_event_to_sse(&mut encoder, &event);
        assert_eq!(frames.len(), 1);
        let frame = String::from_utf8(frames[0].to_vec()).unwrap();
        assert!(frame.contains("step_end"));
    }

    #[test]
    fn encode_multiple_events_sequentially() {
        let mut encoder = Identity::<AgentEvent>::default();
        let events = vec![
            AgentEvent::TextDelta {
                delta: "first".into(),
            },
            AgentEvent::TextDelta {
                delta: "second".into(),
            },
            AgentEvent::StepEnd,
        ];
        let mut all_frames = Vec::new();
        for event in &events {
            all_frames.extend(encode_event_to_sse(&mut encoder, event));
        }
        assert_eq!(all_frames.len(), 3);
        let first = String::from_utf8(all_frames[0].to_vec()).unwrap();
        assert!(first.contains("first"));
        let third = String::from_utf8(all_frames[2].to_vec()).unwrap();
        assert!(third.contains("step_end"));
    }

    #[test]
    fn encode_run_start_event_identity() {
        let mut encoder = Identity::<AgentEvent>::default();
        let event = AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            identity: None,
            parent_run_id: None,
        };
        let frames = encode_event_to_sse(&mut encoder, &event);
        assert_eq!(frames.len(), 1);
        let frame = String::from_utf8(frames[0].to_vec()).unwrap();
        assert!(frame.contains("run_start"));
        assert!(frame.contains("t1"));
    }

    #[test]
    fn encode_run_finish_event_identity() {
        use awaken_server_contract::contract::lifecycle::TerminationReason;
        let mut encoder = Identity::<AgentEvent>::default();
        let event = AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            identity: None,
            result: None,
            termination: TerminationReason::NaturalEnd,
        };
        let frames = encode_event_to_sse(&mut encoder, &event);
        assert_eq!(frames.len(), 1);
        let frame = String::from_utf8(frames[0].to_vec()).unwrap();
        assert!(frame.contains("run_finish"));
    }

    #[test]
    fn encode_ag_ui_prologue_produces_frames() {
        use crate::protocols::ag_ui::encoder::AgUiEncoder;
        let mut encoder = AgUiEncoder::new();
        encoder.on_agent_event(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            identity: None,
            parent_run_id: None,
        });
        let frames = encode_prologue_to_sse(&mut encoder);
        // AG-UI prologue may produce frames depending on state
        // At minimum, verify no panic and correct format
        for frame in &frames {
            let s = String::from_utf8(frame.to_vec()).unwrap();
            assert!(s.starts_with("data: "));
            assert!(s.ends_with("\n\n"));
        }
    }

    #[test]
    fn encode_ai_sdk_prologue_and_epilogue() {
        use crate::protocols::ai_sdk_v6::encoder::AiSdkEncoder;
        let mut encoder = AiSdkEncoder::new();
        let prologue = encode_prologue_to_sse(&mut encoder);
        // AI SDK may have prologue items
        for frame in &prologue {
            let s = String::from_utf8(frame.to_vec()).unwrap();
            assert!(s.starts_with("data: "));
        }

        // Feed an event so epilogue has context
        let _ = encode_event_to_sse(
            &mut encoder,
            &AgentEvent::TextDelta {
                delta: "test".into(),
            },
        );
        let epilogue = encode_epilogue_to_sse(&mut encoder);
        for frame in &epilogue {
            let s = String::from_utf8(frame.to_vec()).unwrap();
            assert!(s.starts_with("data: "));
        }
    }

    #[test]
    fn sse_frame_format_consistency() {
        // All SSE data frames must follow the pattern: "data: {json}\n\n"
        let mut encoder = Identity::<AgentEvent>::default();
        let events = vec![
            AgentEvent::StepStart {
                message_id: "m1".into(),
            },
            AgentEvent::TextDelta { delta: "x".into() },
            AgentEvent::StepEnd,
        ];
        for event in &events {
            let frames = encode_event_to_sse(&mut encoder, event);
            for frame in frames {
                let s = String::from_utf8(frame.to_vec()).unwrap();
                assert!(s.starts_with("data: "), "frame must start with 'data: '");
                assert!(s.ends_with("\n\n"), "frame must end with double newline");
                // The middle part must be valid JSON
                let json_part = &s["data: ".len()..s.len() - 2];
                assert!(
                    serde_json::from_str::<serde_json::Value>(json_part).is_ok(),
                    "frame data must be valid JSON, got: {}",
                    json_part
                );
            }
        }
    }
}
