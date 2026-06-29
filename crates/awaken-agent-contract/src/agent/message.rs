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

    /// The concatenated text of this message's `Text` blocks, ignoring media.
    pub fn text_content(&self) -> String {
        extract_text(&self.content)
    }
}
