//! pi's wire format: the serde model for one session entry, and nothing else.
//! What the entries *mean* is the provider's job ([`super`]); where the files
//! *live* is [`super::discovery`].
//!
//! Documented by pi itself (`docs/session-format.md`, session version 3): a
//! `session` header, then entries `{type, id, parentId, timestamp}` whose
//! `message` entries carry an `AgentMessage`. Defensive all the same: extensions
//! add entry types and message roles freely, so an unknown one parses to
//! something skippable, never a panic.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// One parsed line.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    #[serde(rename = "type")]
    pub kind: String,
    pub timestamp: Option<DateTime<Utc>>,
    /// The header's working directory.
    pub cwd: Option<String>,
    /// A `message` entry's `AgentMessage`.
    pub message: Option<Message>,
    /// A `model_change` entry's model.
    #[serde(rename = "modelId")]
    pub model_id: Option<String>,
    /// A `session_info` entry's display name.
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub role: Option<String>,
    #[serde(default)]
    pub content: Content,
    /// A `toolResult`'s call, the tool it answers, and that tool's own
    /// metadata (for `subagent`, the runs it launched).
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    #[serde(default)]
    pub details: serde_json::Value,
    /// A `toolResult`'s outcome. Required by the format; absent reads as no
    /// recorded outcome rather than success.
    pub is_error: Option<bool>,
    /// An assistant message's model and usage.
    pub model: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    pub output: Option<u64>,
}

/// A message's content: a bare string, or typed blocks.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<Block>),
    #[default]
    None,
}

/// One content block. Flat on purpose: a block type this model does not name
/// still parses, with the fields it lacks left empty.
#[derive(Debug, Clone, Deserialize)]
pub struct Block {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub text: Option<String>,
    pub thinking: Option<String>,
    /// A `toolCall`'s id, name and input.
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

impl Block {
    /// A string argument of a `toolCall`.
    pub fn argument(&self, key: &str) -> Option<&str> {
        self.arguments.get(key)?.as_str()
    }
}

impl Content {
    /// The typed blocks, none for a bare string.
    pub fn blocks(&self) -> &[Block] {
        match self {
            Content::Blocks(blocks) => blocks,
            Content::Text(_) | Content::None => &[],
        }
    }

    /// Every thinking block, joined.
    pub fn thinking(&self) -> String {
        self.blocks()
            .iter()
            .filter_map(|b| b.thinking.as_deref())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every text block, joined.
    pub fn text(&self) -> String {
        match self {
            Content::Text(t) => t.clone(),
            Content::Blocks(blocks) => blocks
                .iter()
                .filter(|b| b.kind.as_deref() == Some("text"))
                .filter_map(|b| b.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n"),
            Content::None => String::new(),
        }
    }
}

/// Parse one line. `None` for a blank or malformed one.
pub fn parse_line(line: &str) -> Option<Entry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}
