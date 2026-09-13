//! The shared omp/pi wire format: the serde model for one JSONL record, and
//! nothing else. What the records *mean* is [`super`]; where the files
//! *live* is [`super::discovery`]. Measured directly against real
//! transcripts on this box (40 omp sessions, 37 pi sessions) and cross-checked
//! against the installed binaries (`omp` 0.2.0, `@earendil-works/pi-coding-agent`
//! 0.84.4) — see the module doc in `super` for what that measurement found.
//!
//! pi is the base lineage; omp forked it and only ever ADDS fields and record
//! types, never renames or removes one already in pi — every difference
//! measured is additive, so one wire model serves both providers.
//! omp-only pieces (`Title`, `SessionInit`, `Custom`, `CustomMessage`,
//! `TitleChange`, `CredentialPin`, `SessionEntry::title`, `ModelChangeEntry::role`,
//! `UserMessage::attribution`) are `Option`, empty, or simply never produced
//! by pi. pi's own naming for a couple of fields pi predates omp on
//! (`model_change.modelId`/`provider` vs omp's `model_change.model`) sits
//! beside the omp field, also `Option`.
//!
//! Defensive by design, like every provider here: an unknown record type, a
//! missing field, or a malformed line parses to something skippable, never a
//! panic.

use chrono::{DateTime, Utc};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Top-level record
// ---------------------------------------------------------------------------

/// One parsed record line, dispatched on `type`.
///
/// Six more top-level types are measured on this box (`compaction`,
/// `branch_summary`, `model_usage`, `ttsr_injection`, `mode_change`,
/// `reset_boundary` — at most 48 lines each across 177 omp sessions) but
/// carry nothing this graph needs; they and any future type fall to
/// [`Entry::Unknown`], same as an entry whose `type` this build has never
/// seen.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Entry {
    /// omp only, always line 1. omp rewrites this line in place as the title
    /// changes (padded to a fixed byte width so a live tailer's later
    /// offsets never shift); pi never writes it at all, which is why its
    /// presence is *the* omp discriminator (see `super::provider_of`).
    #[serde(rename = "title")]
    Title(TitleEntry),

    /// The session root: line 1 on pi, line 2 on omp. Carries the session id
    /// and `cwd`; omp additionally repeats the current title here.
    #[serde(rename = "session")]
    Session(SessionEntry),

    /// A spawned child's own root record (omp only — see `super`): which
    /// role it was given and which model it resolved to.
    #[serde(rename = "session_init")]
    SessionInit(SessionInitEntry),

    /// A user, assistant, or tool-result turn — the bulk of a transcript.
    #[serde(rename = "message")]
    Message(MessageEntry),

    /// Runtime event keyed by `customType` (omp only, measured); see `super`
    /// for what the two customTypes read here mean.
    #[serde(rename = "custom")]
    Custom(CustomEntry),

    /// Framework-injected text delivered as a turn but authored by nobody
    /// (omp only, measured: `mid-run-todo-nudge`, `async-result`). Never a
    /// human prompt, whatever its `customType` — see `super`.
    #[serde(rename = "custom_message")]
    CustomMessage(CustomMessageEntry),

    /// The agent's model changed (a user switch, or a resolved fallback).
    #[serde(rename = "model_change")]
    ModelChange(ModelChangeEntry),

    /// The agent's reasoning-effort level changed.
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange(ThinkingLevelChangeEntry),

    /// The session title changed after the header line (omp only, measured).
    #[serde(rename = "title_change")]
    TitleChange(TitleChangeEntry),

    /// Which provider a credential was pinned to. Session bookkeeping, not
    /// agent activity (omp only, measured).
    #[serde(rename = "credential_pin")]
    CredentialPin(CredentialPinEntry),

    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Header records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TitleEntry {
    #[serde(default)]
    pub title: Option<String>,
    /// The header carries no `timestamp` field — this is the record's only
    /// time, and omp rewrites it every time the title changes.
    #[serde(rename = "updatedAt", default)]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionEntry {
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// omp only: the title as of session start. pi never writes it.
    #[serde(default)]
    pub title: Option<String>,
    /// omp only: how that title was set (measured: observed only `null` on
    /// this box, so surfaced but never asserted on beyond presence).
    #[serde(rename = "titleSource", default)]
    pub title_source: Option<String>,
}

/// A spawned child's own root record (omp only — no nested spawn was ever
/// observed on this box, but any is-a-child file gets this shape). `task`
/// and `systemPrompt` are measured (up to several KB of free text — the full
/// work order, not a label) but never read: nothing in the fact vocabulary
/// carries an agent's full prompt, and using it as a short `description`
/// would misuse that field. `readOnly`/`spawns`/`tools` are measured, purely
/// informational, and likewise unread.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionInitEntry {
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    /// This agent's role/kind ("reviewer", "scout", "task", ...) — this
    /// file's `agent_type`.
    #[serde(default)]
    pub agent: Option<String>,
}

// ---------------------------------------------------------------------------
// message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct MessageEntry {
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub message: Option<MessageBody>,
}

/// One turn, dispatched on `role`. An unrecognised role parses to
/// [`MessageBody::Unknown`] rather than failing the whole line.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "role")]
pub enum MessageBody {
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserMessage {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    /// omp only. Absent on pi, where every `user` turn is treated as human —
    /// pi has no measured synthetic same-role injection to rule out (see
    /// `super`). `"user"` (or absent) is a person; measured on this box:
    /// `"agent"` (the framework re-entering its own thread as a `user` turn)
    /// is the only other value seen, never a person.
    #[serde(default)]
    pub attribution: Option<String>,
    // steering (a human interjection mid-turn rather than between turns) is
    // measured — always paired with `attribution: "user"` — but not parsed:
    // it folds as an ordinary prompt regardless (see `super`), so nothing
    // downstream reads it and keeping it would just be dead weight.
}

impl UserMessage {
    /// Whether this turn is a person's own words: `attribution` absent (pi,
    /// or an omp shape this build has not seen) or `"user"`. `"agent"` is
    /// the one measured counter-example — never a person.
    pub fn is_human(&self) -> bool {
        self.attribution.as_deref().is_none_or(|a| a == "user")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AssistantMessage {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
    /// The provider's own id for this turn's response — the key repeated
    /// cumulative usage shares, so it is the dedup key for [`Tokens`]
    /// (measured: pi never writes it, so its usage is never deduped, which
    /// is correct since pi has nothing to dedup against).
    ///
    /// [`Tokens`]: crate::fact::FactKind::Tokens
    #[serde(rename = "responseId", default)]
    pub response_id: Option<String>,
}

/// Turn usage. `output` is the delta this turn added; `totalTokens` and
/// `cost` are measured but not read — `totalTokens` is cumulative like
/// Claude's usage fields, and summing it would inflate the count the same
/// way summing Claude's would.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub output: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolResultMessage {
    #[serde(rename = "toolCallId", default)]
    pub tool_call_id: Option<String>,
    #[serde(rename = "isError", default)]
    pub is_error: Option<bool>,
    // toolName/content/details are measured but not read: the pairing is by
    // id, the outcome by isError; no fact in this vocabulary carries a tool
    // result's own output text.
}

/// A content block inside `assistant`/`user` message content. An
/// unrecognised block type falls to [`ContentBlock::Unknown`].
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "toolCall")]
    ToolCall(ToolCall),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Only parsed further when `name` is the spawn tool (see
    /// [`TaskArguments`]) — otherwise this call's arguments are never
    /// reconstructed into a summary; `intent` already is one.
    #[serde(default)]
    pub arguments: serde_json::Value,
    /// The agent's own one-line reason for the call. Used as the tool
    /// summary directly instead of reconstructing one from `arguments` (see
    /// `super`).
    #[serde(default)]
    pub intent: Option<String>,
}

/// The `arguments` shape of a `task` toolCall — the one wire-observed spawn
/// tool, and a batch one: unlike Claude's or Codex's spawn tool, which names
/// exactly one child, this can name several in `tasks[]`. Parsed on demand,
/// like Claude's `AgentToolInput`, only when [`ToolCall::name`] is `"task"`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TaskArguments {
    #[serde(default)]
    pub tasks: Vec<TaskItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TaskItem {
    /// The requested child name — absent when the caller left it to be
    /// generated. Always the file stem the runtime ends up writing, unless
    /// the name collides with one already used in this session (measured:
    /// happens across separate fan-out rounds), in which case the runtime
    /// appends `-2`, `-3`, ... and this field still records the un-suffixed
    /// request. See `super` for how that is handled.
    #[serde(default)]
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// custom / custom_message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CustomEntry {
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(rename = "customType", default)]
    pub custom_type: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
}

/// `data` of a `session_exit` custom record — the one `customType` besides
/// `tool_execution_start` this build reads past `CustomEntry::data`. Any
/// other `customType` (measured: only those two on this box) is skipped
/// with its `data` left as an opaque `Value`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionExit {
    /// Measured: `"normal"` (a clean exit), `"signal"` (sigterm/sighup — cut
    /// short externally), `"fatal"` (an unhandled rejection — a crash).
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CustomMessageEntry {
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    // customType/content/display/details/attribution are measured, but no
    // customType changes how this record folds: it is never a Prompt
    // whatever it says (see `super`), so only its timestamp is read.
}

// ---------------------------------------------------------------------------
// model_change / thinking_level_change / title_change / credential_pin
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelChangeEntry {
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    /// omp's field name for the new model id.
    #[serde(default)]
    pub model: Option<String>,
    /// pi's field name for the same thing.
    #[serde(rename = "modelId", default)]
    pub model_id: Option<String>,
    /// omp only. Absent, or `"default"`, describes the agent's own running
    /// model; `"fallback"`/`"temporary"` (both measured) describe a
    /// one-off override of a different model slot, not this agent's
    /// standing model — see `super`.
    #[serde(default)]
    pub role: Option<String>,
    /// omp only: this change resolved to a fallback because the requested
    /// model was unavailable.
    #[serde(rename = "resolvedModelIsFallback", default)]
    pub resolved_model_is_fallback: Option<bool>,
}

impl ModelChangeEntry {
    /// The new model id, whichever field this dialect used.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref().or(self.model_id.as_deref())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThinkingLevelChangeEntry {
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(rename = "thinkingLevel", default)]
    pub thinking_level: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TitleChangeEntry {
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub title: Option<String>,
    // source: measured, same shape as SessionEntry::title_source, not read
    // again here — the session-info row it would feed is already kept
    // current by the header, and re-stating it on every title change would
    // just flicker the same value.
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CredentialPinEntry {
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub provider: Option<String>,
    // hash: measured, a credential fingerprint with no display purpose
    // beyond confirming a pin happened.
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// The one wire-observed tool that spawns agents: a batch fan-out, so one
/// call can name several children (see [`TaskArguments`]) — unlike Claude's
/// or Codex's spawn tool, which names exactly one.
pub fn is_spawn_tool(name: &str) -> bool {
    name == "task"
}

/// Parse one record line. `None` for a blank line or any deserialization
/// failure — never panics. This is the defensive boundary the rest of the
/// codebase relies on.
pub fn parse_line(line: &str) -> Option<Entry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_type_and_malformed_lines_are_skippable() {
        assert!(matches!(parse_line(r#"{"type":"future_thing"}"#), Some(Entry::Unknown)));
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        assert!(parse_line("not json").is_none());
        assert!(parse_line("{").is_none());
    }

    #[test]
    fn user_message_is_human_only_without_agent_attribution() {
        let human = UserMessage {
            attribution: None,
            ..Default::default()
        };
        let also_human = UserMessage {
            attribution: Some("user".into()),
            ..Default::default()
        };
        let injected = UserMessage {
            attribution: Some("agent".into()),
            ..Default::default()
        };
        assert!(human.is_human());
        assert!(also_human.is_human());
        assert!(!injected.is_human());
    }

    #[test]
    fn model_change_reads_either_dialect_field_name() {
        let omp = ModelChangeEntry {
            model: Some("anthropic/claude-opus-5".into()),
            ..Default::default()
        };
        let pi = ModelChangeEntry {
            model_id: Some("claude-opus-5".into()),
            ..Default::default()
        };
        assert_eq!(omp.model(), Some("anthropic/claude-opus-5"));
        assert_eq!(pi.model(), Some("claude-opus-5"));
    }

    #[test]
    fn tool_call_content_block_parses_intent_and_arguments() {
        let entry: Entry = serde_json::from_str(
            r#"{"type":"message","timestamp":"2026-09-11T11:43:04.647Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"toolu_1","name":"task","arguments":{"tasks":[{"name":"TickReviewer"}]},"intent":"spawn a reviewer"}]}}"#,
        )
        .unwrap();
        let Entry::Message(m) = entry else {
            panic!("expected a message entry");
        };
        let Some(MessageBody::Assistant(a)) = m.message else {
            panic!("expected an assistant body");
        };
        let ContentBlock::ToolCall(tc) = &a.content[0] else {
            panic!("expected a toolCall block");
        };
        assert_eq!(tc.intent.as_deref(), Some("spawn a reviewer"));
        let args: TaskArguments = serde_json::from_value(tc.arguments.clone()).unwrap();
        assert_eq!(args.tasks[0].name.as_deref(), Some("TickReviewer"));
    }
}
