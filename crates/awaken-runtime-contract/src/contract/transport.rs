//! Stream transcoder trait for protocol bridging.

use std::marker::PhantomData;

/// Stream transcoder: maps an input stream to an output stream.
///
/// Stateful, supports 1:N mapping and stream lifecycle hooks.
/// Used for both directions (recv and send) of a protocol endpoint.
pub trait Transcoder: Send {
    /// Input item type consumed by this transcoder.
    type Input: Send + 'static;
    /// Output item type produced by this transcoder.
    type Output: Send + 'static;

    /// Events emitted before the input stream starts.
    fn prologue(&mut self) -> Vec<Self::Output> {
        Vec::new()
    }

    /// Map one input item to zero or more output items.
    fn transcode(&mut self, item: &Self::Input) -> Vec<Self::Output>;

    /// Events emitted after the input stream ends.
    fn epilogue(&mut self) -> Vec<Self::Output> {
        Vec::new()
    }
}

/// Pass-through transcoder (no transformation).
pub struct Identity<T>(PhantomData<T>);

impl<T> Default for Identity<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T: Clone + Send + 'static> Transcoder for Identity<T> {
    type Input = T;
    type Output = T;

    fn transcode(&mut self, item: &Self::Input) -> Vec<Self::Output> {
        vec![item.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::event::AgentEvent;

    #[test]
    fn identity_passthrough() {
        let mut t = Identity::<u32>::default();
        assert_eq!(t.transcode(&42), vec![42]);
    }

    #[test]
    fn identity_passthrough_string() {
        let mut t = Identity::<String>::default();
        let input = "hello".to_string();
        let output = t.transcode(&input);
        assert_eq!(output, vec!["hello".to_string()]);
    }

    #[test]
    fn identity_prologue_epilogue_empty() {
        let mut t = Identity::<u32>::default();
        assert!(t.prologue().is_empty());
        assert!(t.epilogue().is_empty());
    }

    /// Mock transcoder that converts AgentEvent to JSON strings.
    struct JsonTranscoder;

    impl Transcoder for JsonTranscoder {
        type Input = AgentEvent;
        type Output = String;

        fn prologue(&mut self) -> Vec<String> {
            vec!["[".to_string()]
        }

        fn transcode(&mut self, item: &AgentEvent) -> Vec<String> {
            match serde_json::to_string(item) {
                Ok(json) => vec![json],
                Err(e) => vec![format!("{{\"error\":\"{e}\"}}")],
            }
        }

        fn epilogue(&mut self) -> Vec<String> {
            vec!["]".to_string()]
        }
    }

    #[test]
    fn mock_transcoder_converts_agent_event_to_json() {
        let mut t = JsonTranscoder;
        let event = AgentEvent::TextDelta {
            delta: "hello".into(),
        };

        let prologue = t.prologue();
        assert_eq!(prologue, vec!["["]);

        let output = t.transcode(&event);
        assert_eq!(output.len(), 1);
        assert!(output[0].contains("\"event_type\":\"text_delta\""));

        let epilogue = t.epilogue();
        assert_eq!(epilogue, vec!["]"]);
    }

    /// Mock transcoder that filters: only emits text deltas.
    struct FilterTranscoder;

    impl Transcoder for FilterTranscoder {
        type Input = AgentEvent;
        type Output = String;

        fn transcode(&mut self, item: &AgentEvent) -> Vec<String> {
            match item {
                AgentEvent::TextDelta { delta } => vec![delta.clone()],
                _ => Vec::new(),
            }
        }
    }

    #[test]
    fn filter_transcoder_drops_non_text_events() {
        let mut t = FilterTranscoder;

        let text = AgentEvent::TextDelta { delta: "hi".into() };
        assert_eq!(t.transcode(&text), vec!["hi"]);

        let step = AgentEvent::StepEnd;
        assert!(t.transcode(&step).is_empty());
    }

    /// Stateful transcoder that counts items.
    struct CountingTranscoder {
        count: usize,
    }

    impl CountingTranscoder {
        fn new() -> Self {
            Self { count: 0 }
        }
    }

    impl Transcoder for CountingTranscoder {
        type Input = String;
        type Output = (usize, String);

        fn transcode(&mut self, item: &String) -> Vec<(usize, String)> {
            self.count += 1;
            vec![(self.count, item.clone())]
        }
    }

    #[test]
    fn stateful_transcoder_counts() {
        let mut t = CountingTranscoder::new();
        assert_eq!(t.transcode(&"a".into()), vec![(1, "a".to_string())]);
        assert_eq!(t.transcode(&"b".into()), vec![(2, "b".to_string())]);
        assert_eq!(t.transcode(&"c".into()), vec![(3, "c".to_string())]);
    }
}
