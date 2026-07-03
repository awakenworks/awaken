//! Wire-agnostic content helpers shared by the protocol adapters.

use awaken_agent_contract::agent::content::ContentBlock;

/// Flatten content blocks to their concatenated text, dropping non-text blocks.
/// Adapters use this to bound a tool result (or an assistant turn) to plain text.
pub fn blocks_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concatenates_text_blocks_and_drops_media() {
        let blocks = vec![
            ContentBlock::text("hello "),
            ContentBlock::image_url("https://example/x.png"),
            ContentBlock::text("world"),
        ];
        assert_eq!(blocks_text(&blocks), "hello world");
    }

    #[test]
    fn empty_when_no_text() {
        assert_eq!(blocks_text(&[]), "");
    }
}
