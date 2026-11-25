//! OpenCode's serde model: the JSON documents in `session_message.data`.
//!
//! One message is one row, and a row is *rewritten* as it streams: an
//! assistant message is inserted with `time.created`, then its `content`
//! grows part by part and each tool part walks `running` → `completed`, all
//! under the same id with a later `time_updated`. So unlike a transcript line
//! a document is read many times, and what it says is whatever it says now.
//!
//! The message's kind (`user`, `assistant`, `idle`, ...) is a column, not a
//! field of the document; [`Message::parse`] takes the document alone. With
//! OpenCode's `experimental.part_rows` the assistant's parts are rows of a
//! table of their own, which [`Message::parse_with`] folds back into `content`.
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
    /// What the runtime attached to a message it wrote itself.
    #[serde(default)]
    pub metadata: Option<Value>,
}

/// What a `synthetic` message says about a child, when it is the report that
/// a background `subagent` call's child went idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report<'a> {
    /// The child session.
    pub child: &'a str,
    /// `completed`, `error` or `cancelled`.
    pub state: &'a str,
}

impl Message {
    /// The child's end this message reports, if it reports one. The report is
    /// `metadata: {source: "subagent", childID, state}` over a text headed
    /// `<subagent ... state="..." ...>`. The two states differ in one case: a
    /// child that went idle with background work of its own still outstanding
    /// has `metadata.state` `completed` and a header saying `running`. That is
    /// not an end, and another report follows.
    pub fn report(&self) -> Option<Report<'_>> {
        let md = self.metadata.as_ref()?;
        if md.get("source")?.as_str()? != "subagent" {
            return None;
        }
        // The header is the first tag; the child's own words follow it.
        let header = self.text.as_deref().unwrap_or_default();
        let header = header.split_once(">\n").map_or(header, |(tag, _)| tag);
        if header.contains(" state=\"running\" ") {
            return None;
        }
        Some(Report {
            child: md.get("childID")?.as_str()?,
            state: md.get("state")?.as_str()?,
        })
    }

    pub fn parse(data: &str) -> Option<Message> {
        serde_json::from_str(data).ok()
    }

    /// A message whose parts may live in rows of their own. With OpenCode's
    /// `experimental.part_rows` on, each assistant part is a `session_part`
    /// row (the same document an inline `content` element is) and the
    /// envelope's `content` is empty. A database can hold both shapes on one
    /// message, after a downgrade and an upgrade; OpenCode keeps the rows
    /// first and the inline parts after them, and so does this.
    pub fn parse_with(data: &str, parts: &[String]) -> Option<Message> {
        let mut message = Message::parse(data)?;
        if !parts.is_empty() {
            let mut content: Vec<Part> = parts
                .iter()
                .map(|p| serde_json::from_str(p).unwrap_or_default())
                .collect();
            content.append(&mut message.content);
            message.content = content;
        }
        Some(message)
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

    /// The call is still being written by the model and has not been made:
    /// OpenCode first stores a tool part as `streaming` with its input as a
    /// half-written *string*, and replaces both when the call is made. (`pending`
    /// is the same state under an older name.)
    pub fn streaming(&self) -> bool {
        matches!(self.status.as_deref(), Some("streaming" | "pending"))
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
/// document [`Message::parse_with`] reads. `parts` are the documents of its
/// `session_part` rows in order, empty for a message that keeps its parts
/// inline. `time_updated` is the latest write to the message *or* any of its
/// parts: with part rows, OpenCode updates a part without touching the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub kind: String,
    pub time_updated: i64,
    pub data: String,
    pub parts: Vec<String>,
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
    fn part_rows_come_first_and_inline_parts_after_them() {
        let rows = [
            r#"{"type":"reasoning","text":"from a row"}"#.to_string(),
            "not json".to_string(),
        ];
        let m = Message::parse_with(
            r#"{"time":{"created":1},"content":[{"type":"text","text":"inline"}]}"#,
            &rows,
        )
        .unwrap();
        let kinds: Vec<&str> = m.content.iter().map(|p| p.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["reasoning", "", "text"],
            "an unreadable row is empty, not dropped"
        );
        assert_eq!(m.content[0].text.as_deref(), Some("from a row"));
        // No rows: exactly the inline message.
        let plain =
            Message::parse_with(r#"{"content":[{"type":"text","text":"x"}]}"#, &[]).unwrap();
        assert_eq!(plain.content.len(), 1);
    }

    #[test]
    fn a_report_is_final_only_when_its_header_does_not_say_running() {
        let report = |header: &str, state: &str, body: &str| {
            let text = format!(
                "<subagent sessionID=\"ses_c\" state=\"{header}\" description=\"d\">\n{body}\n</subagent>"
            );
            let data = serde_json::json!({"text": text, "metadata":
                {"source": "subagent", "childID": "ses_c", "agent": "general", "state": state}});
            Message::parse(&data.to_string())
                .unwrap()
                .report()
                .map(|r| (r.child.to_string(), r.state.to_string()))
        };
        let ended = |s: &str| Some(("ses_c".to_string(), s.to_string()));
        assert_eq!(report("completed", "completed", "ok"), ended("completed"));
        assert_eq!(report("error", "error", "boom"), ended("error"));
        assert_eq!(report("running", "completed", "still going"), None);
        // What the child wrote is not the header.
        assert_eq!(
            report(
                "completed",
                "completed",
                "it printed <x state=\"running\" y> once"
            ),
            ended("completed")
        );
        // A background shell's notice is not a subagent's report.
        let shell =
            r#"{"text":"x","metadata":{"source":"shell","childID":"ses_c","state":"completed"}}"#;
        assert_eq!(Message::parse(shell).unwrap().report(), None);
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
