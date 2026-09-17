//! The serde model for Bob's records: one JSON line per row of its database.
//!
//! Bob (IBM Bob, the IDE agent and its `bob` shell) keeps every session in one
//! SQLite database, `~/.bob/db/bob.db`, in two tables. A `tasks` row is a
//! session (`parent_id` null) or one of its children (`task_type` `subagent`
//! or `subtask`); a `messages` row is one turn of a task's transcript, with
//! the turn itself in a JSON `data` blob. The row feeder (`db.rs`) and `zoe
//! export` turn a row into a line of this shape, and this is what the stream
//! reads, whichever way it arrived:
//!
//! ```json
//! {"table":"tasks","seen":"created","row":{"id":"…","parent_id":null,…}}
//! {"table":"messages","row":{"id":"…","task_id":"…","role":"user","data":{…}}}
//! {"table":"messages","row":{"id":"…","task_id":"<child>","via":"<call>","role":"tool","data":{…}}}
//! ```
//!
//! `seen` splits the two observations a task row records: `created` is the
//! row as first seen, and stated at `created_at`; `updated` is the row after a
//! change (status, cost), stated at `updated_at`. A finished child's single
//! row would otherwise have to fold its birth and its ending at one moment.
//! `via` marks a turn the feeder lifted out of a spawn result's transcript:
//! a child's own turn, at its own time, whose tokens the child's row totals.
//!
//! Defensive throughout: an unknown table, a missing field or a malformed
//! blob parses to something the stream skips, never a panic. The JSON blobs
//! (`env`, `costs`, `data`) are accepted as objects (how the feeder writes
//! them) or as the strings the database stores.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

/// One line: a row of one table, with how it was observed.
#[derive(Debug, Clone, Deserialize)]
pub struct Line {
    pub table: String,
    #[serde(default)]
    pub seen: Option<String>,
    #[serde(default)]
    pub row: Value,
}

/// A line, resolved to the row it carries.
#[derive(Debug, Clone)]
pub enum Record {
    Task { seen: Seen, row: TaskRow },
    Message(MessageRow),
}

/// Which observation of a task row a line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    Created,
    Updated,
}

/// Parse one line. `None` for a blank line, anything that is not a line of
/// this shape, or a row that lacks its id.
pub fn parse_line(line: &str) -> Option<Record> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let parsed: Line = serde_json::from_str(line).ok()?;
    match parsed.table.as_str() {
        "tasks" => {
            let seen = match parsed.seen.as_deref() {
                Some("updated") => Seen::Updated,
                _ => Seen::Created,
            };
            let row: TaskRow = serde_json::from_value(parsed.row).ok()?;
            Some(Record::Task { seen, row })
        }
        "messages" => {
            let row: MessageRow = serde_json::from_value(parsed.row).ok()?;
            Some(Record::Message(row))
        }
        _ => None,
    }
}

/// Whether a line is a session's root task row: the head of an export.
pub fn is_root_head(line: &str) -> Option<TaskRow> {
    match parse_line(line)? {
        Record::Task { row, .. } if row.parent_id.is_none() => Some(row),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct TaskRow {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    /// `file:/path/to/workspace` for a local workspace.
    #[serde(default)]
    pub project_id: Option<String>,
    /// The session's title, which for a child is its whole spawn prompt.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    /// Why the task stopped, when it stopped on an error: a JSON object
    /// with a `name` and a `message`, or the string `"null"`.
    #[serde(default, deserialize_with = "blob")]
    pub last_error: Option<LastError>,
    #[serde(default, deserialize_with = "blob")]
    pub env: Option<Env>,
    #[serde(default, deserialize_with = "blob")]
    pub costs: Option<Costs>,
    /// Epoch milliseconds.
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

impl TaskRow {
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }

    pub fn created(&self) -> Option<DateTime<Utc>> {
        self.created_at.and_then(millis)
    }

    pub fn updated(&self) -> Option<DateTime<Utc>> {
        self.updated_at.and_then(millis)
    }

    /// The workspace the session ran in: `env.workspace`, else the
    /// `project_id` without its `file:` scheme.
    pub fn workspace(&self) -> Option<String> {
        self.env
            .as_ref()
            .and_then(|e| e.workspace.clone())
            .or_else(|| {
                self.project_id
                    .as_deref()
                    .map(|p| p.strip_prefix("file:").unwrap_or(p).to_string())
            })
            .filter(|w| !w.is_empty())
    }

    /// The first line of the title, for a label.
    pub fn headline(&self) -> Option<&str> {
        self.title
            .as_deref()
            .and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
            .map(str::trim)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LastError {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl LastError {
    /// One line: the name, and the head of the message.
    pub fn headline(&self) -> Option<String> {
        let message = self
            .message
            .as_deref()
            .and_then(|m| m.lines().next())
            .map(str::trim)
            .filter(|m| !m.is_empty());
        match (self.name.as_deref(), message) {
            (Some(n), Some(m)) if !m.starts_with(n) => Some(format!("{n}: {m}")),
            (_, Some(m)) => Some(m.to_string()),
            (Some(n), None) => Some(n.to_string()),
            (None, None) => None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Env {
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default, rename = "modeId")]
    pub mode_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Costs {
    /// Cumulative output tokens.
    #[serde(default)]
    pub output: Option<u64>,
    /// Cumulative cost in USD.
    #[serde(default)]
    pub cost: Option<f64>,
}

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct MessageRow {
    pub id: String,
    pub task_id: String,
    /// The spawning call whose result carried this turn, when the feeder
    /// lifted it out of that result's transcript.
    #[serde(default)]
    pub via: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default, deserialize_with = "blob")]
    pub data: Option<Data>,
}

impl MessageRow {
    /// The role, from the row column or the blob.
    pub fn role(&self) -> Option<&str> {
        self.role
            .as_deref()
            .or_else(|| self.data.as_ref().and_then(|d| d.role.as_deref()))
    }

    /// When the turn happened: `_meta.timestamp` inside the blob. The row's
    /// `created_at` is not that: Bob rewrites a session's rows when it ends
    /// or is reopened, and stamps them all with the rewrite time.
    pub fn timestamp(&self) -> Option<DateTime<Utc>> {
        self.data.as_ref().and_then(Data::timestamp)
    }
}

/// The turn: what Bob stores as the `data` blob, and the shape of each turn
/// a spawn's result embeds under `messages`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Data {
    #[serde(default)]
    pub role: Option<String>,
    /// The turn's own id; the row's id for a row of its own.
    #[serde(default)]
    pub id: Option<String>,
    /// A string, or a list of typed parts when images are attached.
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default, rename = "toolCalls")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, rename = "toolUsage")]
    pub tool_usage: Option<ToolUsage>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
    /// A spawn result's copy of the child's transcript, turn by turn.
    #[serde(default)]
    pub messages: Vec<Data>,
}

impl Data {
    /// When the turn happened: `_meta.timestamp`.
    pub fn timestamp(&self) -> Option<DateTime<Utc>> {
        self.meta
            .as_ref()
            .and_then(|m| m.timestamp)
            .and_then(millis)
    }

    /// The call a tool turn answers.
    pub fn call_id(&self) -> Option<&str> {
        self.tool_usage
            .as_ref()
            .and_then(|u| u.signature.as_ref())
            .and_then(|s| s.id.as_deref())
    }

    /// The verdict a tool turn carries.
    pub fn is_error(&self) -> Option<bool> {
        self.tool_usage
            .as_ref()
            .and_then(|u| u.signature.as_ref())
            .and_then(|s| s.is_error)
    }

    /// The text of `content`, whichever shape it takes.
    pub fn text(&self) -> String {
        match &self.content {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    }

    /// What the person typed: the mask when the harness expanded the turn
    /// (a `/skill` command becomes the skill's whole text), else the content.
    pub fn prompt_text(&self) -> String {
        let mask = self
            .meta
            .as_ref()
            .and_then(|m| m.mask.as_deref())
            .map(str::trim)
            .filter(|m| !m.is_empty());
        match mask {
            Some(m) => m.to_string(),
            None => self.text(),
        }
    }

    /// A turn the harness marked hidden: not shown to the user, states nothing.
    pub fn hidden(&self) -> bool {
        self.meta.as_ref().is_some_and(|m| m.hide == Some(true))
    }

    /// A turn the harness wrote in the model's voice (a workflow step's
    /// narration): activity, not the model's reasoning.
    pub fn not_ai(&self) -> bool {
        self.meta.as_ref().is_some_and(|m| m.not_ai == Some(true))
    }

    /// Output tokens this assistant turn spent, when the harness recorded them.
    pub fn output_tokens(&self) -> Option<u64> {
        self.meta
            .as_ref()
            .and_then(|m| m.spend.as_ref())
            .and_then(|s| s.output)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Meta {
    /// Epoch milliseconds.
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(default)]
    pub spend: Option<Spend>,
    #[serde(default)]
    pub hide: Option<bool>,
    #[serde(default, rename = "notAi")]
    pub not_ai: Option<bool>,
    /// What the person typed, when `content` is not it: a `/skill` command
    /// is stored expanded into the skill's text, and the command itself is
    /// kept here.
    #[serde(default)]
    pub mask: Option<String>,
    /// On a spawn's result: the child task it ran as.
    #[serde(default, rename = "subagentId")]
    pub subagent_id: Option<String>,
    /// On a spawn's result: what kind of child it was.
    #[serde(default, rename = "agentType")]
    pub agent_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Spend {
    #[serde(default)]
    pub output: Option<u64>,
}

/// One call in an assistant turn's `toolCalls`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "object")]
    pub arguments: Map<String, Value>,
}

impl ToolCall {
    /// A string argument by name.
    pub fn argument(&self, key: &str) -> Option<&str> {
        self.arguments.get(key).and_then(Value::as_str)
    }

    /// The first string argument, whatever its name.
    pub fn first_string(&self) -> Option<&str> {
        self.arguments.values().find_map(Value::as_str)
    }
}

/// What a tool turn says about the call it answers.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolUsage {
    #[serde(default)]
    pub signature: Option<Signature>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Signature {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "isError")]
    pub is_error: Option<bool>,
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Epoch milliseconds as a time.
pub fn millis(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(ms)
}

/// A JSON blob column: an object, or the string the database stores it as.
/// Anything else, or a string that does not parse, is `None`.
fn blob<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::String(s) => serde_json::from_str(&s).ok(),
        Value::Null => None,
        other => serde_json::from_value(other).ok(),
    })
}

/// An object, or an empty map for anything else.
fn object<'de, D>(d: D) -> Result<Map<String, Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Value::deserialize(d)? {
        Value::Object(m) => m,
        Value::String(s) => serde_json::from_str::<Value>(&s)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default(),
        _ => Map::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_parse_as_objects_or_strings() {
        let as_object = r#"{"table":"tasks","row":{"id":"t","parent_id":null,"env":{"workspace":"/w","modeId":"agent"},"costs":{"output":5,"cost":0.1},"created_at":1789649352547}}"#;
        let as_string = r#"{"table":"tasks","seen":"updated","row":{"id":"t","parent_id":null,"env":"{\"workspace\":\"/w\"}","costs":"{\"output\":5}","created_at":1789649352547}}"#;
        for (line, seen) in [(as_object, Seen::Created), (as_string, Seen::Updated)] {
            let Some(Record::Task { seen: s, row }) = parse_line(line) else {
                panic!("not a task row");
            };
            assert_eq!(s, seen);
            assert!(row.is_root());
            assert_eq!(row.workspace().as_deref(), Some("/w"));
            assert_eq!(row.costs.as_ref().and_then(|c| c.output), Some(5));
            assert!(row.created().is_some());
        }
        // A workspace falls back to the project id without its scheme.
        let Some(Record::Task { row, .. }) =
            parse_line(r#"{"table":"tasks","row":{"id":"t","project_id":"file:/p"}}"#)
        else {
            panic!()
        };
        assert_eq!(row.workspace().as_deref(), Some("/p"));
    }

    #[test]
    fn message_time_comes_from_the_blob_not_the_row() {
        let line = r#"{"table":"messages","row":{"id":"m","task_id":"t","role":"assistant","created_at":1789649372772,"data":{"role":"assistant","content":"hi","_meta":{"timestamp":1789649354389,"spend":{"output":468}},"toolCalls":[{"id":"c1","name":"read_file","arguments":{"path":"a.rs"}}]}}}"#;
        let Some(Record::Message(m)) = parse_line(line) else {
            panic!()
        };
        assert_eq!(m.timestamp(), millis(1789649354389));
        let d = m.data.unwrap();
        assert_eq!(d.text(), "hi");
        assert_eq!(d.output_tokens(), Some(468));
        assert_eq!(d.tool_calls[0].argument("path"), Some("a.rs"));
    }

    #[test]
    fn garbage_and_other_tables_are_skipped() {
        assert!(parse_line("").is_none());
        assert!(parse_line("not json").is_none());
        assert!(parse_line(r#"{"table":"attribution_logs","row":{"id":"x"}}"#).is_none());
        assert!(parse_line(r#"{"table":"tasks","row":{"parent_id":null}}"#).is_none());
        assert!(is_root_head(r#"{"table":"tasks","row":{"id":"c","parent_id":"p"}}"#).is_none());
        assert!(is_root_head(r#"{"table":"tasks","row":{"id":"p"}}"#).is_some());
    }
}
