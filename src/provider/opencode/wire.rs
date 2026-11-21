//! OpenCode's serde model: the JSON documents in `session_message.data`.
//!
//! One message is one row, and a row is *rewritten* as it streams: an
//! assistant message is inserted with `time.created`, then its `content`
//! grows part by part and each tool part walks `running` → `completed`, all
//! under the same id with a later `time_updated`. So unlike a transcript line
//! a document is read many times, and what it says is whatever it says now.
//!
//! The message's kind (`user`, `assistant`, `idle`, ...) is a column, not a
//! field of the document; [`Message::parse`] takes the document alone.
//!
//! Defensive like the other wire modules: every field is optional, a part
//! that does not parse becomes an empty one that states nothing, and a
//! document that is not JSON is `None`. Times are epoch milliseconds.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A `session_message.data` document. One shape for every kind of message:
/// a `user` message has `text`, an `assistant` one has `content`, `tokens` and
/// `model`, and a kind this build does not read simply has none of them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub time: MessageTime,
    #[serde(default)]
    pub model: Option<ModelRef>,
    /// The assistant's parts, in the order it produced them.
    #[serde(default, deserialize_with = "lenient_parts")]
    pub content: Vec<Part>,
    #[serde(default)]
    pub tokens: Option<Tokens>,
    /// What a person (or the runtime) wrote, for the kinds that carry text.
    #[serde(default)]
    pub text: Option<String>,
}

impl Message {
    pub fn parse(data: &str) -> Option<Message> {
        serde_json::from_str(data).ok()
    }

    /// Whether the assistant has finished this message. Its `content` stops
    /// growing and its `tokens` are final.
    pub fn is_complete(&self) -> bool {
        self.time.completed.is_some()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessageTime {
    pub created: Option<i64>,
    #[serde(rename = "firstToken")]
    pub first_token: Option<i64>,
    pub completed: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelRef {
    pub id: Option<String>,
}

/// Token counts. Reasoning is counted apart from the rest of the output.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Tokens {
    pub output: Option<u64>,
    pub reasoning: Option<u64>,
}

impl Tokens {
    /// Everything the model generated: what it said and what it thought.
    pub fn generated(&self) -> u64 {
        self.output
            .unwrap_or(0)
            .saturating_add(self.reasoning.unwrap_or(0))
    }
}

/// One element of an assistant message's `content`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Part {
    /// `text`, `reasoning` or `tool`; anything else is not read.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// For a tool part, the call id: the join key between its start and end.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub state: Option<ToolState>,
    #[serde(default)]
    pub time: PartTime,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PartTime {
    pub created: Option<i64>,
    /// When the tool began running, as opposed to when the model asked for it.
    pub ran: Option<i64>,
    pub completed: Option<i64>,
}

/// A tool part's state. `status` is `running`, `completed` or `error`; `input`
/// and `metadata` stay as JSON because their shape is each tool's own.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolState {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

impl ToolState {
    /// A string field of the tool's input.
    pub fn input_str(&self, key: &str) -> Option<&str> {
        self.input.as_ref()?.get(key)?.as_str()
    }

    /// A string field of the tool's metadata.
    pub fn meta_str(&self, key: &str) -> Option<&str> {
        self.metadata.as_ref()?.get(key)?.as_str()
    }

    /// The tool ran and said it failed.
    pub fn errored(&self) -> bool {
        self.status.as_deref() == Some("error")
    }

    /// The tool finished, one way or the other.
    pub fn finished(&self) -> bool {
        matches!(self.status.as_deref(), Some("completed" | "error"))
    }

    /// A `shell` call that completed but did not succeed: a non-zero exit
    /// code, or death by signal. The tool call itself *completed*; the format
    /// records the command's verdict beside the call's status.
    pub fn shell_failed(&self) -> bool {
        let Some(md) = &self.metadata else {
            return false;
        };
        matches!(md.get("exit").and_then(Value::as_i64), Some(code) if code != 0)
            || md.get("signal").is_some_and(|s| !s.is_null())
    }
}

impl Part {
    /// The tool's name, empty when the part did not say.
    pub fn tool_name(&self) -> &str {
        self.name.as_deref().unwrap_or_default()
    }
}

/// A session's row: identity, place in the tree, and what the header shows.
/// What the database hands over, not a document: plain columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub parent_id: Option<String>,
    pub directory: String,
    pub title: Option<String>,
    pub version: String,
    /// The agent that ran it (`build`, `general`, ...), when the row says.
    pub agent: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
}

/// A message's row: the kind and the update time are columns, `data` is the
/// document [`Message::parse`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub kind: String,
    pub time_updated: i64,
    pub data: String,
}

/// Epoch milliseconds as a time. `None` for a missing or out-of-range value.
pub fn ms(t: Option<i64>) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(t?)
}

/// A `content` array where one malformed element costs that element, not the
/// message: a part this build cannot read is skipped, the rest still state.
fn lenient_parts<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Part>, D::Error> {
    let raw = Vec::<Value>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap_or_default())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_assistant_message_parses_with_its_parts() {
        let m = Message::parse(
            r#"{"time":{"created":1000,"firstToken":1100,"completed":2000},
                "model":{"id":"m-1","providerID":"p"},
                "tokens":{"input":9,"output":40,"reasoning":2,"cache":{"read":1,"write":0}},
                "content":[
                  {"type":"reasoning","text":"thinking"},
                  {"type":"tool","id":"call_1","name":"read",
                   "state":{"status":"completed","input":{"path":"a.rs"},"metadata":{}},
                   "time":{"created":1200,"ran":1210,"completed":1300}}]}"#,
        )
        .unwrap();
        assert!(m.is_complete());
        assert_eq!(m.model.unwrap().id.as_deref(), Some("m-1"));
        assert_eq!(m.tokens.unwrap().generated(), 42);
        assert_eq!(m.content.len(), 2);
        let tool = &m.content[1];
        assert_eq!(tool.kind, "tool");
        assert_eq!(tool.tool_name(), "read");
        assert_eq!(tool.state.as_ref().unwrap().input_str("path"), Some("a.rs"));
        assert_eq!(tool.time.ran, Some(1210));
    }

    #[test]
    fn a_malformed_part_costs_only_itself() {
        let m = Message::parse(
            r#"{"content":[{"type":"text","text":"ok"},{"type":7,"id":[]},{"type":"text","text":"also ok"}]}"#,
        )
        .unwrap();
        assert_eq!(m.content.len(), 3);
        assert_eq!(m.content[0].text.as_deref(), Some("ok"));
        assert!(m.content[1].kind.is_empty(), "unreadable part is empty");
        assert_eq!(m.content[2].text.as_deref(), Some("also ok"));
        assert!(Message::parse("not json").is_none());
        assert!(!Message::parse("{}").unwrap().is_complete());
    }

    #[test]
    fn shell_verdicts_live_in_the_metadata() {
        let state = |json: &str| serde_json::from_str::<ToolState>(json).unwrap();
        assert!(!state(r#"{"status":"completed","metadata":{"exit":0}}"#).shell_failed());
        assert!(state(r#"{"status":"completed","metadata":{"exit":2}}"#).shell_failed());
        assert!(state(r#"{"status":"completed","metadata":{"signal":"SIGKILL"}}"#).shell_failed());
        // A background shell: the call completed as a launch acknowledgement.
        assert!(!state(r#"{"status":"completed","metadata":{"status":"running"}}"#).shell_failed());
        assert!(state(r#"{"status":"error"}"#).errored());
        assert!(!state(r#"{"status":"running"}"#).finished());
    }

    #[test]
    fn milliseconds_become_times() {
        assert_eq!(
            ms(Some(1_795_294_817_584)).unwrap().to_rfc3339(),
            "2026-11-21T21:00:17.584+00:00"
        );
        assert_eq!(ms(None), None);
    }
}
