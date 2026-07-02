use serde::{Deserialize, Serialize};

use crate::agent::content::{ContentBlock, extract_text};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Id(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One message in a thread. Content is multimodal: a list of content blocks,
/// never a bare string, so an image rides alongside text without a schema change.
/// Not `Eq` — a `ToolUse` block carries untyped JSON arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: Id,
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// A plain-text message — the common case, `vec![ContentBlock::text(..)]`.
    pub fn text(id: Id, role: Role, text: impl Into<String>) -> Self {
        Self {
            id,
            role,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// A message from a ready-made block list — the multimodal case, where a turn
    /// interleaves text with an image (or other media) block.
    pub fn new(id: Id, role: Role, content: Vec<ContentBlock>) -> Self {
        Self { id, role, content }
    }

    /// The concatenated text of this message's `Text` blocks, ignoring media.
    pub fn text_content(&self) -> String {
        extract_text(&self.content)
    }
}
