//! Codex's wire format: the serde model for one rollout line, and nothing
//! else. What the records *mean* is the provider's job ([`super`]); where the
//! files *live* is [`super::discovery`].
//!
//! A rollout line is an envelope, `{timestamp, ordinal, type, payload}`, whose
//! payload shape depends on `type`. Two of the types carry a second layer:
//! `response_item` is what went to or came from the model (messages,
//! reasoning, tool calls and their raw outputs), `event_msg` is what Codex
//! itself observed (turn boundaries, token counts, and `item_completed`, the
//! operations that actually ran, with exit codes and thread ownership). The
//! provider reads both: the request layer proves the agent tried, the item
//! layer says what happened.
//!
//! Defensive by design: the format is undocumented and shifts between Codex
//! versions (verified against 0.149.1, 0.150.0-alpha.8 and 0.153.4), so an
//! unknown type, a missing field or a malformed line parses to something
//! skippable, never a panic. The envelope is parsed first and the payload
//! second, so an unrecognised payload still keeps its timestamp.

use chrono::{DateTime, Utc};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// One parsed rollout line: the envelope plus the payload it carried.
#[derive(Debug, Clone)]
pub struct Line {
    pub timestamp: Option<DateTime<Utc>>,
    /// Position in the file. A child rollout replays its parent's history
    /// below `session_meta.subagent_history_start_ordinal`.
    pub ordinal: Option<u64>,
    pub payload: Payload,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    timestamp: Option<DateTime<Utc>>,
    ordinal: Option<u64>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: serde_json::Value,
}

/// What a line carried, by envelope `type`.
#[derive(Debug, Clone)]
pub enum Payload {
    /// The first line of every rollout, identifying the thread the file is
    /// by. A child rollout carries a second one: its parent's, replayed.
    SessionMeta(Box<SessionMeta>),
    /// The model-facing layer.
    ResponseItem(ResponseItem),
    /// Codex's own observations.
    EventMsg(EventMsg),
    /// Per-turn settings; the model name lives here.
    TurnContext(TurnContext),
    /// `world_state`, `inter_agent_communication_metadata`,
    /// `token_usage_record`, and whatever a later version adds.
    Other(String),
}

// ---------------------------------------------------------------------------
// session_meta
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionMeta {
    /// This thread's id.
    pub id: Option<String>,
    /// The root thread's id: equal to `id` for a root, the root's for a child.
    pub session_id: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub cwd: Option<String>,
    /// `codex-tui`, `codex_exec`, `Codex Desktop`, ...
    pub originator: Option<String>,
    pub cli_version: Option<String>,
    /// `"user"` for a root thread, `"subagent"` for a spawned one. The
    /// discriminator that holds across every frontend and version seen;
    /// `source` does not (`"cli"`, `"vscode"`, `"exec"`, or an object).
    pub thread_source: Option<String>,
    #[serde(default)]
    pub source: Source,
    /// Also present at the top level on some versions; `source` is preferred.
    pub parent_thread_id: Option<String>,
    pub agent_path: Option<String>,
    pub agent_nickname: Option<String>,
    /// First ordinal that is this thread's own; lines below it are the
    /// parent's history, replayed so the child has context.
    pub subagent_history_start_ordinal: Option<u64>,
}

impl SessionMeta {
    pub fn is_root(&self) -> bool {
        match self.thread_source.as_deref() {
            Some("subagent") => false,
            Some(_) => true,
            // Older shape: a string source is a root, an object is a spawn.
            None => !matches!(self.source, Source::Spawn { .. }),
        }
    }

    /// The spawning thread, for a child. The spawn object states it when the
    /// shape carries one; older files state it at the top level instead, so
    /// both are tried before giving up.
    pub fn parent(&self) -> Option<&str> {
        match &self.source {
            Source::Spawn { subagent } => subagent
                .spawn()
                .and_then(|s| s.parent_thread_id.as_deref())
                .or(self.parent_thread_id.as_deref()),
            _ => self.parent_thread_id.as_deref(),
        }
    }

    /// `/root/explore_theme` on the spawn, when the meta records it.
    pub fn path(&self) -> Option<&str> {
        match &self.source {
            Source::Spawn { subagent } => subagent
                .spawn()
                .and_then(|s| s.agent_path.as_deref())
                .or(self.agent_path.as_deref()),
            _ => self.agent_path.as_deref(),
        }
    }

    pub fn nickname(&self) -> Option<&str> {
        match &self.source {
            Source::Spawn { subagent } => subagent
                .spawn()
                .and_then(|s| s.agent_nickname.as_deref())
                .or(self.agent_nickname.as_deref()),
            _ => self.agent_nickname.as_deref(),
        }
    }
}

/// `session_meta.source`: a string naming the frontend for a root, an object
/// describing the spawn for a child.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum Source {
    Named(String),
    Spawn {
        subagent: SubagentSource,
    },
    #[default]
    Unknown,
}

/// `source.subagent`. Two shapes in the wild: an object carrying the spawn
/// detail, and — on 0.146.0 and older — the bare kind of the subagent, as in
/// `{"subagent": "review"}` for `codex exec review`. The older shape carries no
/// spawn detail at all, but those files state `parent_thread_id` at the top
/// level of the payload, so the link survives.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum SubagentSource {
    Spawn {
        #[serde(default)]
        thread_spawn: ThreadSpawn,
    },
    Kind(String),
    #[default]
    Unknown,
}

impl SubagentSource {
    /// The spawn detail, when this shape carries any.
    fn spawn(&self) -> Option<&ThreadSpawn> {
        match self {
            Self::Spawn { thread_spawn } => Some(thread_spawn),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThreadSpawn {
    pub parent_thread_id: Option<String>,
    pub depth: Option<u32>,
    pub agent_path: Option<String>,
    pub agent_nickname: Option<String>,
}

// ---------------------------------------------------------------------------
// response_item
// ---------------------------------------------------------------------------

/// The model-facing layer, by `payload.type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseItem {
    #[serde(rename = "message")]
    Message(Message),
    /// Encrypted on the wire; `summary` is usually empty. The readable form
    /// is the `Reasoning` item in the event layer.
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default)]
        summary: Vec<serde_json::Value>,
    },
    /// A built-in or collaboration tool: `spawn_agent`, `wait_agent`,
    /// `send_message`, ...
    #[serde(rename = "function_call")]
    FunctionCall(FunctionCall),
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: Option<String>,
        output: Option<serde_json::Value>,
    },
    /// The `exec` tool: a JavaScript program calling `tools.*`, which runs
    /// zero or more real operations. Those surface as `item_completed`.
    #[serde(rename = "custom_tool_call")]
    CustomToolCall(CustomToolCall),
    #[serde(rename = "custom_tool_call_output")]
    CustomToolCallOutput {
        call_id: Option<String>,
        #[serde(default)]
        output: Output,
    },
    /// A message between agents (`author` → `recipient` by path).
    #[serde(rename = "agent_message")]
    AgentMessage {
        author: Option<String>,
        recipient: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Message {
    /// `user`, `assistant`, `developer`.
    pub role: Option<String>,
    #[serde(default)]
    pub content: Vec<ContentPart>,
}

impl Message {
    /// The message's text parts, joined.
    pub fn text(&self) -> String {
        let parts: Vec<&str> = self
            .content
            .iter()
            .filter_map(|p| p.text.as_deref())
            .collect();
        parts.join("\n")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FunctionCall {
    pub call_id: Option<String>,
    pub name: Option<String>,
    /// JSON, as a string.
    pub arguments: Option<String>,
}

impl FunctionCall {
    pub fn argument(&self, key: &str) -> Option<String> {
        let args: serde_json::Value = serde_json::from_str(self.arguments.as_deref()?).ok()?;
        args.get(key)?.as_str().map(str::to_owned)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CustomToolCall {
    pub call_id: Option<String>,
    pub name: Option<String>,
    /// The program, for `exec`.
    pub input: Option<String>,
}

/// A tool output: a string, or a list of text parts.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Output {
    Text(String),
    Parts(Vec<ContentPart>),
    Other(serde_json::Value),
}

impl Default for Output {
    fn default() -> Self {
        Output::Other(serde_json::Value::Null)
    }
}

impl Output {
    /// The leading text, where the `exec` tool writes its verdict line.
    pub fn head(&self) -> Option<&str> {
        match self {
            Output::Text(s) => Some(s),
            Output::Parts(parts) => parts.first().and_then(|p| p.text.as_deref()),
            Output::Other(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// event_msg
// ---------------------------------------------------------------------------

/// Codex's own observations, by `payload.type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum EventMsg {
    /// A turn began. Keyed by `turn_id`: a turn boundary within one thread,
    /// not an agent's start.
    #[serde(rename = "task_started")]
    TaskStarted { turn_id: Option<String> },
    /// A turn ended. Likewise not an agent's end.
    #[serde(rename = "task_complete")]
    TaskComplete { turn_id: Option<String> },
    #[serde(rename = "turn_aborted")]
    TurnAborted,
    /// Cumulative and last-call usage; `last_token_usage` is the delta.
    #[serde(rename = "token_count")]
    TokenCount { info: Option<TokenInfo> },
    /// An operation that ran, with what the request layer lacks: ownership
    /// (`thread_id`), timing, exit codes.
    #[serde(rename = "item_completed")]
    ItemCompleted(Box<ItemCompleted>),
    #[serde(rename = "thread_settings_applied")]
    ThreadSettingsApplied {
        thread_settings: Option<ThreadSettings>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TokenInfo {
    /// Cumulative for the thread. Codex re-emits a `token_count` with an
    /// unchanged total, so the total is what the provider diffs.
    pub total_token_usage: Option<TokenUsage>,
    pub last_token_usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TokenUsage {
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThreadSettings {
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItemCompleted {
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item: Item,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
}

impl ItemCompleted {
    pub fn started_at(&self) -> Option<DateTime<Utc>> {
        self.started_at_ms.and_then(DateTime::from_timestamp_millis)
    }

    pub fn completed_at(&self) -> Option<DateTime<Utc>> {
        self.completed_at_ms
            .and_then(DateTime::from_timestamp_millis)
    }
}

/// What ran, by `item.type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Item {
    /// A shell command, with its own `exec-<uuid>` id, exit code and Codex's
    /// parse of what the command does.
    CommandExecution(CommandExecution),
    /// A built-in extension: `kind: "web.search"` with its query.
    Extension(Extension),
    /// A patch applied to files.
    FileChange(FileChange),
    /// A spawned agent's lifecycle, seen from the spawner: `started`,
    /// `interacted`, `completed`. `id` is the spawning call's id.
    SubAgentActivity(SubAgentActivity),
    /// `wait`, `spawn`, `send` as observed items. The request layer already
    /// carries these as function calls.
    CollabAgentToolCall {
        tool: Option<String>,
        status: Option<String>,
    },
    /// Text a person typed. Present only in a root thread's rollout.
    UserMessage {
        #[serde(default)]
        content: Vec<ContentPart>,
    },
    /// The assistant's text; a duplicate of the response layer's message.
    AgentMessage,
    /// The readable reasoning summary, when the model produced one.
    Reasoning {
        #[serde(default)]
        summary_text: Vec<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CommandExecution {
    pub id: Option<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub parsed_cmd: Vec<ParsedCmd>,
    /// `completed`, `failed`.
    pub status: Option<String>,
    pub exit_code: Option<i64>,
}

impl CommandExecution {
    /// The command as a person would type it: the shell wrapper
    /// (`/bin/zsh -lc <cmd>`) stripped.
    pub fn display_command(&self) -> String {
        match self.command.as_slice() {
            [shell, flag, cmd] if flag == "-lc" || flag == "-c" => {
                let _ = shell;
                cmd.clone()
            }
            parts => parts.join(" "),
        }
    }
}

/// Codex's own classification of a command: `read`, `search`, `list_files`,
/// `unknown`, ...
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ParsedCmd {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub cmd: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Extension {
    pub id: Option<String>,
    pub kind: Option<String>,
    pub query: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FileChange {
    pub id: Option<String>,
    /// Keyed by path.
    #[serde(default)]
    pub changes: serde_json::Map<String, serde_json::Value>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SubAgentActivity {
    /// The spawning call's id.
    pub id: Option<String>,
    /// `started`, `interacted`, `completed`.
    pub kind: Option<String>,
    pub agent_thread_id: Option<String>,
    pub agent_path: Option<String>,
}

// ---------------------------------------------------------------------------
// turn_context
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TurnContext {
    pub turn_id: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse one rollout line. `None` for a blank line, or one whose envelope is
/// not a Codex record at all.
pub fn parse_line(line: &str) -> Option<Line> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let env: Envelope = serde_json::from_str(trimmed).ok()?;
    let payload = match env.kind.as_str() {
        "session_meta" => serde_json::from_value(env.payload)
            .map(|m| Payload::SessionMeta(Box::new(m)))
            .unwrap_or_else(|_| Payload::Other(env.kind.clone())),
        "response_item" => serde_json::from_value(env.payload)
            .map(Payload::ResponseItem)
            .unwrap_or(Payload::ResponseItem(ResponseItem::Other)),
        "event_msg" => serde_json::from_value(env.payload)
            .map(Payload::EventMsg)
            .unwrap_or(Payload::EventMsg(EventMsg::Other)),
        "turn_context" => serde_json::from_value(env.payload)
            .map(Payload::TurnContext)
            .unwrap_or_else(|_| Payload::Other(env.kind.clone())),
        other => Payload::Other(other.to_string()),
    };
    Some(Line {
        timestamp: env.timestamp,
        ordinal: env.ordinal,
        payload,
    })
}

/// The last path segment of an agent path: `/root/explore_theme` →
/// `explore_theme`.
pub fn path_leaf(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_meta_with_string_source_is_root() {
        let l = parse_line(r#"{"timestamp":"2026-08-26T15:30:09.955Z","ordinal":0,"type":"session_meta","payload":{"id":"a","session_id":"a","cwd":"/p","originator":"codex-tui","cli_version":"0.149.1","source":"cli","thread_source":"user"}}"#).unwrap();
        let Payload::SessionMeta(m) = l.payload else {
            panic!("not meta");
        };
        assert!(m.is_root());
        assert_eq!(m.parent(), None);
        assert_eq!(l.ordinal, Some(0));
    }

    #[test]
    fn child_meta_reads_the_spawn_object() {
        let l = parse_line(r#"{"timestamp":"2026-08-26T16:13:35.494Z","ordinal":0,"type":"session_meta","payload":{"id":"c","session_id":"a","source":{"subagent":{"thread_spawn":{"parent_thread_id":"a","depth":1,"agent_path":"/root/explore_theme","agent_nickname":"Huygens"}}},"thread_source":"subagent","subagent_history_start_ordinal":19}}"#).unwrap();
        let Payload::SessionMeta(m) = l.payload else {
            panic!("not meta");
        };
        assert!(!m.is_root());
        assert_eq!(m.parent(), Some("a"));
        assert_eq!(m.path(), Some("/root/explore_theme"));
        assert_eq!(m.nickname(), Some("Huygens"));
        assert_eq!(m.subagent_history_start_ordinal, Some(19));
    }

    /// 0.146.0 writes the subagent kind as a bare string and states the parent
    /// at the top level. Before this shape was accepted, `session_meta` failed
    /// to deserialize and the whole file was rejected as unreadable, which hid
    /// every `codex exec review` — the child holds all the work, the parent
    /// holds none.
    #[test]
    fn child_meta_reads_the_older_string_subagent() {
        let l = parse_line(r#"{"timestamp":"2026-09-03T09:28:42.858Z","ordinal":0,"type":"session_meta","payload":{"id":"c","session_id":"a","cwd":"/p","originator":"codex_exec","cli_version":"0.146.0","source":{"subagent":"review"},"thread_source":"subagent","parent_thread_id":"a","multi_agent_version":"disabled"}}"#).unwrap();
        let Payload::SessionMeta(m) = l.payload else {
            panic!("not meta");
        };
        assert!(!m.is_root());
        assert_eq!(m.parent(), Some("a"));
        assert_eq!(m.path(), None);
        assert_eq!(m.nickname(), None);
    }

    #[test]
    fn item_completed_command_execution() {
        let l = parse_line(r#"{"timestamp":"2026-08-26T16:10:54.934Z","ordinal":24,"type":"event_msg","payload":{"type":"item_completed","thread_id":"a","turn_id":"t","item":{"type":"CommandExecution","id":"exec-1","command":["/bin/zsh","-lc","cargo test"],"parsed_cmd":[{"type":"unknown","cmd":"cargo test"}],"status":"failed","exit_code":101},"started_at_ms":1787760650000,"completed_at_ms":1787760654000}}"#).unwrap();
        let Payload::EventMsg(EventMsg::ItemCompleted(ic)) = l.payload else {
            panic!("not item");
        };
        let Item::CommandExecution(c) = &ic.item else {
            panic!("not command");
        };
        assert_eq!(c.display_command(), "cargo test");
        assert_eq!(c.exit_code, Some(101));
        assert!(ic.started_at().unwrap() < ic.completed_at().unwrap());
    }

    #[test]
    fn unknown_kinds_are_skippable_not_fatal() {
        let l = parse_line(r#"{"timestamp":"2026-08-26T16:10:14.933Z","ordinal":6,"type":"world_state","payload":{"full":true}}"#).unwrap();
        assert!(matches!(l.payload, Payload::Other(ref k) if k == "world_state"));
        let l = parse_line(r#"{"timestamp":"2026-08-26T16:10:14.933Z","type":"event_msg","payload":{"type":"brand_new_event"}}"#).unwrap();
        assert!(matches!(l.payload, Payload::EventMsg(EventMsg::Other)));
        let l = parse_line(r#"{"type":"response_item","payload":{"type":"whatever"}}"#).unwrap();
        assert!(matches!(
            l.payload,
            Payload::ResponseItem(ResponseItem::Other)
        ));
        assert!(parse_line("not json").is_none());
        assert!(parse_line("").is_none());
    }

    #[test]
    fn exec_output_head_is_the_verdict_line() {
        let l = parse_line(r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c","output":[{"type":"input_text","text":"Script completed\nWall time 0.2 seconds\nOutput:\n"},{"type":"input_text","text":"---"}]}}"#).unwrap();
        let Payload::ResponseItem(ResponseItem::CustomToolCallOutput { output, .. }) = l.payload
        else {
            panic!("not output");
        };
        assert!(output.head().unwrap().starts_with("Script completed"));
    }

    #[test]
    fn function_call_argument() {
        let fc = FunctionCall {
            call_id: Some("c".into()),
            name: Some("spawn_agent".into()),
            arguments: Some(r#"{"task_name":"explore_theme","fork_turns":"all"}"#.into()),
        };
        assert_eq!(fc.argument("task_name").as_deref(), Some("explore_theme"));
        assert_eq!(fc.argument("missing"), None);
    }
}
