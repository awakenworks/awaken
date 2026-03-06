//! Generic inference request transformation.
//!
//! Plugins register [`InferenceRequestTransform`] implementations to modify
//! the message list and set request-level hints before inference. The core
//! loop applies all registered transforms without knowing their semantics.

use crate::runtime::tool_call::ToolDescriptor;
use crate::thread::Message;
use std::sync::Arc;

/// Output of an inference request transform.
pub struct InferenceTransformOutput {
    /// Transformed message list (system + history).
    pub messages: Vec<Message>,
    /// Whether to enable prompt caching for this request.
    pub enable_prompt_cache: bool,
}

/// Trait for plugins that transform the inference request before it is sent
/// to the LLM. The core loop calls all registered transforms in order,
/// piping the output messages of one into the input of the next.
///
/// Implementing this trait is how plugins inject behaviors like context
/// window truncation, message summarization, or content filtering — without
/// the core loop needing domain-specific knowledge.
pub trait InferenceRequestTransform: Send + Sync {
    /// Transform the message list. `tool_descriptors` are provided so
    /// transforms can account for tool token overhead.
    fn transform(
        &self,
        messages: Vec<Message>,
        tool_descriptors: &[ToolDescriptor],
    ) -> InferenceTransformOutput;
}

/// Apply a chain of transforms, piping messages through each in order.
///
/// Returns the final messages and whether any transform requested prompt caching.
pub fn apply_request_transforms(
    mut messages: Vec<Message>,
    tool_descriptors: &[ToolDescriptor],
    transforms: &[Arc<dyn InferenceRequestTransform>],
) -> InferenceTransformOutput {
    let mut enable_prompt_cache = false;
    for transform in transforms {
        let output = transform.transform(messages, tool_descriptors);
        messages = output.messages;
        enable_prompt_cache |= output.enable_prompt_cache;
    }
    InferenceTransformOutput {
        messages,
        enable_prompt_cache,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::Message;

    /// A transform that prepends a system message.
    struct PrependSystem(String);

    impl InferenceRequestTransform for PrependSystem {
        fn transform(
            &self,
            mut messages: Vec<Message>,
            _tool_descriptors: &[ToolDescriptor],
        ) -> InferenceTransformOutput {
            messages.insert(0, Message::system(&self.0));
            InferenceTransformOutput {
                messages,
                enable_prompt_cache: false,
            }
        }
    }

    /// A transform that enables prompt caching without modifying messages.
    struct EnableCache;

    impl InferenceRequestTransform for EnableCache {
        fn transform(
            &self,
            messages: Vec<Message>,
            _tool_descriptors: &[ToolDescriptor],
        ) -> InferenceTransformOutput {
            InferenceTransformOutput {
                messages,
                enable_prompt_cache: true,
            }
        }
    }

    /// A transform that drops messages exceeding a count limit.
    struct LimitMessages(usize);

    impl InferenceRequestTransform for LimitMessages {
        fn transform(
            &self,
            messages: Vec<Message>,
            _tool_descriptors: &[ToolDescriptor],
        ) -> InferenceTransformOutput {
            let kept: Vec<Message> = messages.into_iter().take(self.0).collect();
            InferenceTransformOutput {
                messages: kept,
                enable_prompt_cache: false,
            }
        }
    }

    #[test]
    fn empty_transforms_is_passthrough() {
        let messages = vec![Message::user("Hello"), Message::assistant("Hi")];
        let output = apply_request_transforms(messages.clone(), &[], &[]);
        assert_eq!(output.messages.len(), 2);
        assert_eq!(output.messages[0].content, "Hello");
        assert_eq!(output.messages[1].content, "Hi");
        assert!(!output.enable_prompt_cache);
    }

    #[test]
    fn single_transform_applied() {
        let messages = vec![Message::user("Hello")];
        let transforms: Vec<Arc<dyn InferenceRequestTransform>> =
            vec![Arc::new(PrependSystem("System prompt".into()))];
        let output = apply_request_transforms(messages, &[], &transforms);
        assert_eq!(output.messages.len(), 2);
        assert_eq!(output.messages[0].content, "System prompt");
        assert_eq!(output.messages[1].content, "Hello");
    }

    #[test]
    fn transforms_chain_pipes_output_to_next() {
        let messages = vec![Message::user("Hello")];
        let transforms: Vec<Arc<dyn InferenceRequestTransform>> = vec![
            Arc::new(PrependSystem("First".into())),
            Arc::new(PrependSystem("Second".into())),
        ];
        let output = apply_request_transforms(messages, &[], &transforms);
        // First transform: [System("First"), User("Hello")]
        // Second transform: [System("Second"), System("First"), User("Hello")]
        assert_eq!(output.messages.len(), 3);
        assert_eq!(output.messages[0].content, "Second");
        assert_eq!(output.messages[1].content, "First");
        assert_eq!(output.messages[2].content, "Hello");
    }

    #[test]
    fn enable_prompt_cache_or_aggregated() {
        let messages = vec![Message::user("Hello")];
        // First transform: cache=false, Second: cache=true
        let transforms: Vec<Arc<dyn InferenceRequestTransform>> = vec![
            Arc::new(PrependSystem("sys".into())), // cache=false
            Arc::new(EnableCache),                  // cache=true
        ];
        let output = apply_request_transforms(messages, &[], &transforms);
        assert!(output.enable_prompt_cache, "should be true via OR aggregation");
    }

    #[test]
    fn enable_prompt_cache_stays_false_when_none_request() {
        let messages = vec![Message::user("Hello")];
        let transforms: Vec<Arc<dyn InferenceRequestTransform>> = vec![
            Arc::new(PrependSystem("a".into())),
            Arc::new(PrependSystem("b".into())),
        ];
        let output = apply_request_transforms(messages, &[], &transforms);
        assert!(!output.enable_prompt_cache);
    }

    #[test]
    fn chain_with_limiting_transform() {
        let messages = vec![
            Message::user("1"),
            Message::user("2"),
            Message::user("3"),
        ];
        let transforms: Vec<Arc<dyn InferenceRequestTransform>> = vec![
            Arc::new(PrependSystem("sys".into())), // 4 messages
            Arc::new(LimitMessages(2)),             // keep first 2
        ];
        let output = apply_request_transforms(messages, &[], &transforms);
        assert_eq!(output.messages.len(), 2);
        assert_eq!(output.messages[0].content, "sys");
        assert_eq!(output.messages[1].content, "1");
    }
}
