// @formal surface requirement=runtime.run_tool_lifecycle.lifecycle
// @formal reason: Rewrites only the outbound copy of one provider request: images from tools that the
// @formal reason: route cannot accept or that exceed the request budget become model-visible text. The
// @formal reason: committed transcript is untouched; this is a product-requirement classification.
//! Which images returned by tools one provider request may carry.
//!
//! The decision belongs at dispatch rather than inside a tool: the tool that read
//! the image runs before the model is chosen for the next attempt, a fallback may
//! be a different model, and a session's history can outlive any one route. This
//! pass runs on every attempt's own request copy, so the session events keep the
//! original image, and a later route that accepts images can still see it.
//!
//! Each image is replaced by text rather than removed, so the model is told what
//! happened and can act on it.

use awaken_agent_contract::agent::content::{ContentBlock, ImageSource, elide_tool_result_images};
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::ImageInput;
use awaken_runtime_contract::llm::ChatMessage;

/// Images one assistant step's tool calls may deliver together.
pub const MAX_TOOL_IMAGES_PER_STEP: usize = 8;
/// Images from tools one request may carry, across the whole visible history.
pub const MAX_TOOL_IMAGES_PER_REQUEST: usize = 20;
/// Base64 bytes of tool images one request may carry. Provider request bodies are
/// limited to around 32 MB, and the text of the conversation shares that room.
pub const MAX_TOOL_IMAGE_BYTES_PER_REQUEST: usize = 20 * 1024 * 1024;

const UNSUPPORTED_NOTE: &str = "Error: the current model does not support image input, so this image was not delivered. Use numeric checks instead.";
const HISTORY_NOTE: &str = "[Earlier image omitted from this request to stay within the image budget; read the file again if it is still needed.]";

fn step_limit_note() -> String {
    format!(
        "Error: this image was not delivered because this step exceeds the limit of {MAX_TOOL_IMAGES_PER_STEP} images from tools. Read fewer images at once."
    )
}

fn byte_limit_note() -> String {
    format!(
        "Error: this image was not delivered because this request exceeds the limit of {MAX_TOOL_IMAGE_BYTES_PER_REQUEST} bytes of images from tools. Read fewer images at once."
    )
}

fn encoded_size(source: &ImageSource) -> usize {
    match source {
        ImageSource::Base64 { data, .. } => data.len(),
        ImageSource::Url { .. } | ImageSource::File { .. } => 0,
    }
}

/// Apply the route's image capability and the request's image budget.
///
/// The current step, meaning the tool results after the last assistant message,
/// is served first and in call order, and anything over its limits is an error
/// the model must react to. Older images then fill what remains, newest first,
/// and anything further is silently expected to age out, so it gets a plain note.
pub fn shape_tool_result_images(messages: &mut [ChatMessage], image_input: ImageInput) {
    if image_input == ImageInput::Rejected {
        for message in messages {
            for block in &mut message.content {
                if elide_tool_result_images(std::slice::from_mut(block), UNSUPPORTED_NOTE) > 0
                    && let ContentBlock::ToolResult { is_error, .. } = block
                {
                    *is_error = true;
                }
            }
        }
        return;
    }

    let step_start = messages
        .iter()
        .rposition(|message| message.role == Role::Assistant)
        .map_or(0, |index| index + 1);
    let (history, step_results) = messages.split_at_mut(step_start);
    let mut count = 0_usize;
    let mut bytes = 0_usize;

    for block in step_results
        .iter_mut()
        .flat_map(|message| &mut message.content)
    {
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = block
        else {
            continue;
        };
        for nested in content {
            let ContentBlock::Image { source } = nested else {
                continue;
            };
            let size = encoded_size(source);
            let refusal = if count >= MAX_TOOL_IMAGES_PER_STEP {
                step_limit_note()
            } else if bytes + size > MAX_TOOL_IMAGE_BYTES_PER_REQUEST {
                byte_limit_note()
            } else {
                count += 1;
                bytes += size;
                continue;
            };
            *nested = ContentBlock::text(refusal);
            *is_error = true;
        }
    }

    // Once one image misses, every older one misses too, so the cut is one
    // stable boundary rather than a pattern of holes in the history.
    let mut open = true;
    for block in history
        .iter_mut()
        .rev()
        .flat_map(|message| message.content.iter_mut().rev())
    {
        let ContentBlock::ToolResult { content, .. } = block else {
            continue;
        };
        for nested in content.iter_mut().rev() {
            let ContentBlock::Image { source } = nested else {
                continue;
            };
            let size = encoded_size(source);
            open = open
                && count < MAX_TOOL_IMAGES_PER_REQUEST
                && bytes + size <= MAX_TOOL_IMAGE_BYTES_PER_REQUEST;
            if open {
                count += 1;
                bytes += size;
            } else {
                *nested = ContentBlock::text(HISTORY_NOTE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_image(call: &str, data: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Tool,
            content: vec![ContentBlock::tool_result(
                call,
                vec![
                    ContentBlock::text(format!("Image {call}")),
                    ContentBlock::image_base64("image/png", data),
                ],
            )],
        }
    }

    fn assistant() -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::text("ok")],
        }
    }

    fn images_in(message: &ChatMessage) -> usize {
        message
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult { content, .. } => content
                    .iter()
                    .filter(|nested| matches!(nested, ContentBlock::Image { .. }))
                    .count(),
                _ => 0,
            })
            .sum()
    }

    fn result_is_error(message: &ChatMessage) -> bool {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }))
    }

    #[test]
    fn a_model_without_image_input_gets_an_explicit_error_for_every_tool_image() {
        // Decision table, ImageInput::Rejected: tool image in the current step or
        // in history => error note, is_error set, description text kept; a text
        // only tool result => unchanged and not an error; an image the user
        // attached directly => not this layer's decision, unchanged.
        let text_only = ChatMessage {
            role: Role::Tool,
            content: vec![ContentBlock::tool_result(
                "t",
                vec![ContentBlock::text("ok")],
            )],
        };
        let user = ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::image_url("https://example.invalid/u.png")],
        };
        let mut messages = vec![
            user.clone(),
            tool_image("old", "AA=="),
            assistant(),
            text_only.clone(),
            tool_image("new", "AA=="),
        ];
        shape_tool_result_images(&mut messages, ImageInput::Rejected);
        for index in [1, 4] {
            assert_eq!(images_in(&messages[index]), 0);
            assert!(result_is_error(&messages[index]));
            let text =
                awaken_agent_contract::agent::content::extract_text(&messages[index].content);
            assert!(text.contains("Image "), "{text}");
            assert!(text.contains("does not support image input"), "{text}");
            assert!(text.contains("numeric checks"), "{text}");
        }
        assert_eq!(messages[3], text_only);
        assert_eq!(messages[0], user);
    }

    #[test]
    fn the_per_turn_limit_serves_calls_in_order_and_refuses_the_rest() {
        // Boundary rule: exactly the limit passes unchanged; limit + 1 keeps the
        // first calls, and the excess states the limit and is an error.
        let batch = |n: usize| {
            (0..n)
                .map(|index| tool_image(&format!("c{index}"), "AA=="))
                .collect::<Vec<_>>()
        };
        let mut exact = batch(MAX_TOOL_IMAGES_PER_STEP);
        let untouched = exact.clone();
        shape_tool_result_images(&mut exact, ImageInput::Accepted);
        assert_eq!(exact, untouched);

        let mut over = batch(MAX_TOOL_IMAGES_PER_STEP + 1);
        shape_tool_result_images(&mut over, ImageInput::Accepted);
        assert!(
            over[..MAX_TOOL_IMAGES_PER_STEP]
                .iter()
                .all(|m| images_in(m) == 1)
        );
        let refused = &over[MAX_TOOL_IMAGES_PER_STEP];
        assert_eq!(images_in(refused), 0);
        assert!(result_is_error(refused));
        assert!(
            awaken_agent_contract::agent::content::extract_text(&refused.content)
                .contains(&format!("limit of {MAX_TOOL_IMAGES_PER_STEP} images"))
        );
    }

    #[test]
    fn the_byte_budget_refuses_the_current_step_and_ages_out_history() {
        // Boundary rule on the byte budget: two images that exactly fill it pass;
        // a third in the same step is an error naming the byte limit; history is
        // served after the step, so an old image gets the age-out note, not an
        // error, and its tool result stays a success.
        let half = "A".repeat(MAX_TOOL_IMAGE_BYTES_PER_REQUEST / 2);
        let mut messages = vec![
            tool_image("old", "AA=="),
            assistant(),
            tool_image("a", &half),
            tool_image("b", &half),
            tool_image("c", "AA=="),
        ];
        shape_tool_result_images(&mut messages, ImageInput::Accepted);
        assert_eq!(images_in(&messages[2]), 1);
        assert_eq!(images_in(&messages[3]), 1);
        assert_eq!(images_in(&messages[4]), 0);
        assert!(result_is_error(&messages[4]));
        assert!(
            awaken_agent_contract::agent::content::extract_text(&messages[4].content)
                .contains("bytes of images")
        );
        assert_eq!(images_in(&messages[0]), 0);
        assert!(!result_is_error(&messages[0]));
        assert!(
            awaken_agent_contract::agent::content::extract_text(&messages[0].content)
                .contains("read the file again")
        );
    }

    #[test]
    fn history_keeps_the_newest_images_up_to_the_request_limit() {
        // Boundary rule: with no current-step images, the newest
        // MAX_TOOL_IMAGES_PER_REQUEST images survive and the older rest fall away
        // as one contiguous prefix; a request at the limit is left untouched.
        let history = |n: usize| {
            let mut messages = Vec::new();
            for index in 0..n {
                messages.push(tool_image(&format!("c{index}"), "AA=="));
                messages.push(assistant());
            }
            messages
        };
        let mut exact = history(MAX_TOOL_IMAGES_PER_REQUEST);
        let untouched = exact.clone();
        shape_tool_result_images(&mut exact, ImageInput::Accepted);
        assert_eq!(exact, untouched);

        let mut over = history(MAX_TOOL_IMAGES_PER_REQUEST + 2);
        shape_tool_result_images(&mut over, ImageInput::Accepted);
        let kept: Vec<usize> = over
            .iter()
            .filter(|message| message.role == Role::Tool)
            .map(images_in)
            .collect();
        assert_eq!(&kept[..2], &[0, 0]);
        assert!(kept[2..].iter().all(|count| *count == 1));
    }

    #[test]
    fn shaping_is_a_pure_function_of_the_messages() {
        // Retry rule: the same transcript shapes to the same request, so a retry
        // or a resumed attempt cannot change what the model was told.
        let build = || {
            vec![
                tool_image("a", "AA=="),
                assistant(),
                tool_image("b", "AA=="),
            ]
        };
        let mut first = build();
        let mut second = build();
        shape_tool_result_images(&mut first, ImageInput::Accepted);
        shape_tool_result_images(&mut second, ImageInput::Accepted);
        assert_eq!(first, second);
        let mut again = first.clone();
        shape_tool_result_images(&mut again, ImageInput::Accepted);
        assert_eq!(again, first, "idempotent");
    }
}
