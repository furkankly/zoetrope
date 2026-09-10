//! Claude Code's wire format: the serde model for one JSONL line and the
//! `meta.json` sidecar, and nothing else. What the records *mean* is the
//! provider's job ([`super`]); where the files *live* is [`super::discovery`].
//!
//! Defensive by design: the format is undocumented and shifts between Claude
//! Code versions, so unknown entry types, missing fields and malformed lines
//! parse to something skippable, never a panic.

use chrono::{DateTime, Utc};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Top-level entry
// ---------------------------------------------------------------------------

/// One parsed JSONL line.
///
/// Dispatched on the `"type"` field. Any unrecognized `type` (including the
/// documented-but-unobserved `"summary"`) lands in [`Entry::Unknown`] so new
/// Claude Code versions never break parsing.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Entry {
    /// A user turn. `message.content` is string-or-array (see [`UserMessage`]).
    #[serde(rename = "user")]
    User(Box<UserEntry>),

    /// An assistant turn. Carries `message` with content blocks and usage.
    #[serde(rename = "assistant")]
    Assistant(Box<AssistantEntry>),

    /// A system entry, distinguished further by `subtype`. Mostly ignored.
    #[serde(rename = "system")]
    System(Box<SystemEntry>),

    /// A context-injection attachment. Not graph material.
    #[serde(rename = "attachment")]
    Attachment(Box<AttachmentEntry>),

    // --- Flat metadata entries: NO uuid/parentUuid/timestamp envelope. ---
    /// Session title. Provides the header-bar title.
    #[serde(rename = "ai-title")]
    AiTitle(AiTitleEntry),

    /// The last prompt text. Lean metadata.
    #[serde(rename = "last-prompt")]
    LastPrompt(FlatValueEntry),

    /// Editor/agent mode marker. Lean metadata.
    #[serde(rename = "mode")]
    Mode(FlatValueEntry),

    /// Permission-mode marker. Lean metadata.
    #[serde(rename = "permission-mode")]
    PermissionMode(FlatValueEntry),

    /// File-history snapshot marker. Lean metadata.
    #[serde(rename = "file-history-snapshot")]
    FileHistorySnapshot(FlatValueEntry),

    /// Queue-operation marker. Lean metadata.
    #[serde(rename = "queue-operation")]
    QueueOperation(FlatValueEntry),

    // --- Ledger entries (subagent files + journal.jsonl). ---
    /// Ledger `started` entry. Excluded from the graph.
    #[serde(rename = "started")]
    Started(LedgerEntry),

    /// Ledger `result` entry. In `journal.jsonl` it marks workflow-subagent
    /// completion (join on `agentId`).
    #[serde(rename = "result")]
    Result(LedgerEntry),

    /// Catch-all for any unrecognized `type`. Always skipped by the model.
    #[serde(other)]
    Unknown,
}

/// Whether a tool name is an agent/workflow *spawn* (a branch point). Single
/// source for the rule — `Task` is Claude Code's legacy name for the `Agent`
/// tool, so all three count as spawns (provenance, scrubber markers, summaries).
pub fn is_spawn_tool(name: &str) -> bool {
    matches!(name, "Agent" | "Task" | "Workflow")
}

// ---------------------------------------------------------------------------
// Envelope (transcript entries only)
// ---------------------------------------------------------------------------

/// Common envelope fields shared by transcript entries (`user`, `assistant`,
/// `system`, `attachment`).
///
/// `parent_uuid` is `Option<Option<String>>`: the root entry has it
/// *present and null* (`Some(None)`), whereas flat metadata lines omit it
/// entirely (`None`). Distinguishing the two matters for root detection.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub uuid: Option<String>,
    /// `Some(None)` = present-and-null (root); `None` = absent (flat metadata).
    #[serde(rename = "parentUuid", default, deserialize_with = "double_option")]
    pub parent_uuid: Option<Option<String>>,
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
    /// The working directory the session ran in — used to show file paths
    /// relative to the project root (e.g. `src/main.rs`, not the absolute path).
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(rename = "isSidechain", default)]
    pub is_sidechain: Option<bool>,
    #[serde(rename = "promptId", default)]
    pub prompt_id: Option<String>,
    #[serde(rename = "requestId", default)]
    pub request_id: Option<String>,
    /// Present on every line of a subagent file. Join key for subagents.
    #[serde(rename = "agentId", default)]
    pub agent_id: Option<String>,
    /// Assistant-only, subagent files. Do NOT rely on it — join on `agentId`.
    #[serde(rename = "attributionAgent", default)]
    pub attribution_agent: Option<String>,
    /// Provenance of a `user` line. `origin.kind` is `"human"` for a typed/queued
    /// prompt and `"task-notification"` for system-injected async-agent reports —
    /// the authoritative human-vs-system discriminator, stronger than any text
    /// heuristic (see [`UserEntry::is_human_prompt`]).
    #[serde(default)]
    pub origin: Option<Origin>,
}

/// The `origin` object on a `user` line; its `kind` names who authored the text.
#[derive(Debug, Clone, Deserialize)]
pub struct Origin {
    #[serde(default)]
    pub kind: Option<String>,
}

// ---------------------------------------------------------------------------
// Assistant
// ---------------------------------------------------------------------------

/// An `assistant` transcript entry.
#[derive(Debug, Clone, Deserialize)]
pub struct AssistantEntry {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(default)]
    pub message: Option<AssistantMessage>,
}

/// The `message` object of an assistant entry.
#[derive(Debug, Clone, Deserialize)]
pub struct AssistantMessage {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// A content block inside an assistant message.
///
/// Unknown block types fall through to [`ContentBlock::Unknown`].
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    #[serde(rename = "tool_use")]
    ToolUse(ToolUse),
    #[serde(other)]
    Unknown,
}

/// A `tool_use` content block.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolUse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Raw tool input; shape varies by tool. For `Agent`: `{description,
    /// prompt, subagent_type}` (see [`AgentToolInput`]).
    #[serde(default)]
    pub input: serde_json::Value,
    /// Newer-schema field; absent on older transcripts. Observed as an object
    /// (e.g. `{"type":"direct"}`), so it is kept as a raw [`serde_json::Value`]
    /// — typing it as `Option<String>` silently dropped every tool-call line.
    #[serde(default)]
    pub caller: serde_json::Value,
}

/// Typed view of an `Agent` (or `Workflow`) tool_use `input`.
///
/// Parse a [`ToolUse::input`] into this with [`serde_json::from_value`] when
/// `name == "Agent"`; all fields are optional for defensiveness.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AgentToolInput {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub subagent_type: Option<String>,
}

/// Token usage. Sub-fields vary by version, all defaulted.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

// ---------------------------------------------------------------------------
// User
// ---------------------------------------------------------------------------

/// A `user` transcript entry.
#[derive(Debug, Clone, Deserialize)]
pub struct UserEntry {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(default)]
    pub message: Option<UserMessage>,
    /// Top-level sibling of `message`; object or string.
    #[serde(rename = "toolUseResult", default)]
    pub tool_use_result: Option<StringOrValue>,
}

/// A workflow launch recorded in a main-transcript `toolUseResult`
/// (`taskType == "local_workflow"`).
///
/// `run_id` is ground truth for identity, not a guess: it is also the
/// `subagents/workflows/<run_id>/` directory name, so it equals the workflow
/// group's node id. That lets the group be labelled with the workflow's real
/// name instead of the generic fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowLaunch {
    pub run_id: String,
    pub name: Option<String>,
    pub summary: Option<String>,
}

impl UserEntry {
    /// The workflow launch this entry acknowledges, if it is one. Requires a
    /// `runId`; without it there is nothing to attribute the name to.
    pub fn workflow_launch(&self) -> Option<WorkflowLaunch> {
        let Some(StringOrValue::Value(v)) = &self.tool_use_result else {
            return None;
        };
        if v.get("taskType").and_then(|t| t.as_str()) != Some("local_workflow") {
            return None;
        }
        let run_id = v.get("runId").and_then(|r| r.as_str())?;
        Some(WorkflowLaunch {
            run_id: run_id.to_string(),
            name: v
                .get("workflowName")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            summary: v
                .get("summary")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        })
    }

    /// Plain-string user text, if any (array-form content is tool
    /// results/attachments). This is the low-level extractor — it says nothing
    /// about *who* wrote the text: async agents' `<task-notification>` reports and
    /// background-stop notices also arrive as main-thread user strings. Gate on
    /// [`is_human_prompt`](Self::is_human_prompt) for "is this a real prompt".
    pub fn prompt_text(&self) -> Option<&str> {
        let msg = self.message.as_ref()?;
        let Some(UserContent::Text(text)) = &msg.content else {
            return None;
        };
        (!text.trim().is_empty()).then_some(text.as_str())
    }

    /// Whether this is a genuine human-typed prompt (non-empty string content
    /// that a person authored), as opposed to system-injected main-thread user
    /// text. This is the single definition of "is a prompt" — the era spine, the
    /// DVR's `[`/`]` stepping, and the scrubber chapter ticks all route through
    /// it, so they can never disagree.
    ///
    /// Recent sessions stamp `origin.kind` (`"human"` vs `"task-notification"`),
    /// which is authoritative. Legacy sessions predate that field entirely (the
    /// prompt still carries no `origin`), so there we fall back to the text
    /// heuristic the era spine always used — anything that isn't an async agent's
    /// `<task-notification>` report. Ground truth when we have it; the old
    /// heuristic only where the format can't tell us.
    pub fn is_human_prompt(&self) -> bool {
        let Some(text) = self.prompt_text() else {
            return false;
        };
        match self
            .envelope
            .origin
            .as_ref()
            .and_then(|o| o.kind.as_deref())
        {
            Some(kind) => kind == "human",
            None => parse_task_notification(text).is_none(),
        }
    }
}

/// The `message` object of a user entry.
#[derive(Debug, Clone, Deserialize)]
pub struct UserMessage {
    #[serde(default)]
    pub role: Option<String>,
    /// `content` is string OR array of blocks.
    #[serde(default)]
    pub content: Option<UserContent>,
}

/// User message `content`: a bare string or an array of blocks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<UserContentBlock>),
}

/// A block inside an array-form user `content`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum UserContentBlock {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "tool_result")]
    ToolResult(ToolResult),
    #[serde(other)]
    Unknown,
}

/// A `tool_result` block. Pairs with a prior `tool_use` via `tool_use_id`.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolResult {
    #[serde(rename = "tool_use_id", default)]
    pub tool_use_id: Option<String>,
    /// Result payload; string OR array.
    #[serde(default)]
    pub content: Option<ToolResultContent>,
    /// **Missing means success.** Only `Some(true)` indicates failure.
    #[serde(default)]
    pub is_error: Option<bool>,
}

/// `tool_result` content: a bare string or an array of blocks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<serde_json::Value>),
}

// ---------------------------------------------------------------------------
// System / attachment / flat metadata / ledger
// ---------------------------------------------------------------------------

/// A `system` transcript entry, lean — distinguished by `subtype`.
#[derive(Debug, Clone, Deserialize)]
pub struct SystemEntry {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(default)]
    pub subtype: Option<String>,
}

/// An `attachment` entry. Not graph material.
#[derive(Debug, Clone, Deserialize)]
pub struct AttachmentEntry {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(default)]
    pub attachment: Option<serde_json::Value>,
}

/// The `ai-title` flat metadata entry — provides the session title.
///
/// The wire field is `aiTitle`; renamed here so the header bar gets a real
/// value (without the rename the title silently stays `None`).
#[derive(Debug, Clone, Deserialize)]
pub struct AiTitleEntry {
    #[serde(rename = "aiTitle", default)]
    pub title: Option<String>,
}

/// A flat metadata entry with no envelope (`last-prompt`, `mode`,
/// `permission-mode`, `file-history-snapshot`, `queue-operation`).
///
/// Captures the whole object as a `Value` so no required field can fail.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FlatValueEntry {
    #[serde(flatten)]
    pub fields: serde_json::Value,
}

/// A ledger entry (`started` / `result`) from subagent files and
/// `journal.jsonl`.
#[derive(Debug, Clone, Deserialize)]
pub struct LedgerEntry {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(rename = "agentId", default)]
    pub agent_id: Option<String>,
    /// Present on `result`; absent on `started`.
    #[serde(default)]
    pub result: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// meta.json sidecar
// ---------------------------------------------------------------------------

/// A subagent `meta.json` sidecar.
///
/// Direct Agent calls carry all three; workflow subagents carry only
/// `agent_type: "workflow-subagent"`.
///
/// Linkage: `tool_use_id` === the `Agent` tool_use block `.id` in the main
/// transcript; `agent_type` === that tool_use's `input.subagent_type`.
#[derive(Debug, Clone, Deserialize)]
pub struct SubagentMeta {
    #[serde(rename = "agentType", default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "toolUseId", default)]
    pub tool_use_id: Option<String>,
    /// Async background-agent terminal flag: the user stopped it mid-run. A
    /// reliable terminal signal (the async `Agent` result is only a spawn ack).
    #[serde(rename = "stoppedByUser", default)]
    pub stopped_by_user: Option<bool>,
}

// ---------------------------------------------------------------------------
// task-notification (async background-agent termination report)
// ---------------------------------------------------------------------------

/// The terminal status an async background-agent reports (via a
/// `<task-notification>`), distinct from `Done` because "the user stopped it"
/// and "it finished" are different facts worth surfacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Completed,
    Stopped,
    Failed,
    /// A status string we don't recognize — leave the agent's status alone.
    Other,
}

/// A parsed `<task-notification>` — the async background-agent system's terminal
/// report for a subagent, delivered to the MAIN transcript when the agent
/// finishes or is stopped. It arrives embedded in a `user` entry's string
/// content (there is no dedicated entry `type`), so it must be sniffed.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskNotification {
    pub agent_id: String,
    pub status: TaskStatus,
}

/// Parse a `<task-notification>` block if `text` is one — extracting its
/// `<task-id>` (the agentId) and `<status>`. Returns `None` for ordinary user
/// text, so callers can tell a real prompt from a notification.
pub fn parse_task_notification(text: &str) -> Option<TaskNotification> {
    let text = text.trim_start();
    if !text.starts_with("<task-notification>") {
        return None;
    }
    let tag = |name: &str| -> Option<&str> {
        let open = format!("<{name}>");
        let start = text.find(&open)? + open.len();
        let end = text[start..].find(&format!("</{name}>"))? + start;
        Some(text[start..end].trim())
    };
    let agent_id = tag("task-id")?.to_string();
    let status = match tag("status") {
        Some("completed") => TaskStatus::Completed,
        Some("stopped") => TaskStatus::Stopped,
        Some("failed") | Some("error") => TaskStatus::Failed,
        _ => TaskStatus::Other,
    };
    Some(TaskNotification { agent_id, status })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A value that may be serialized as a bare string or as an arbitrary object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrValue {
    Text(String),
    Value(serde_json::Value),
}

/// Deserialize a possibly-absent, possibly-null field into `Option<Option<T>>`
/// so callers can distinguish present-and-null from absent.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

// ---------------------------------------------------------------------------
// Parsing entry point
// ---------------------------------------------------------------------------

/// Parse a single JSONL line into an [`Entry`].
///
/// Returns `None` on blank lines or any deserialization failure — never panics.
/// This is the defensive boundary the rest of the codebase relies on.
pub fn parse_line(line: &str) -> Option<Entry> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<Entry>(trimmed).ok()
}

/// Parse a subagent `meta.json` sidecar. Returns `None` on read/parse failure.
pub fn parse_meta(text: &str) -> Option<SubagentMeta> {
    serde_json::from_str::<SubagentMeta>(text.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_notification_extracts_id_and_status() {
        let text = "<task-notification>\n<task-id>a725391d5b4367772</task-id>\n<output-file>/x.output</output-file>\n<status>stopped</status>\n<summary>No completion record</summary>\n</task-notification>";
        let tn = parse_task_notification(text).expect("is a task-notification");
        assert_eq!(tn.agent_id, "a725391d5b4367772");
        assert_eq!(tn.status, TaskStatus::Stopped);

        // Status variants + unknown → Other.
        let completed = text.replace("stopped", "completed");
        assert_eq!(
            parse_task_notification(&completed).unwrap().status,
            TaskStatus::Completed
        );
        let weird = text.replace("stopped", "paused");
        assert_eq!(
            parse_task_notification(&weird).unwrap().status,
            TaskStatus::Other
        );

        // Ordinary user text is NOT a task-notification (so it stays a prompt).
        assert!(parse_task_notification("please review the codebase").is_none());
    }

    #[test]
    fn is_human_prompt_uses_origin_then_falls_back_to_heuristic() {
        let user = |line: &str| match parse_line(line).unwrap() {
            Entry::User(e) => *e,
            _ => panic!("not a user entry"),
        };

        // origin.kind == "human" → a real prompt (authoritative).
        let human = user(
            r#"{"type":"user","uuid":"u","origin":{"kind":"human"},"timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"fix the bug"}}"#,
        );
        assert!(human.is_human_prompt());

        // origin.kind != "human" → system-injected, NOT a prompt, even with text.
        // This is the case the old text heuristic missed: a background-stop notice
        // that is not itself a `<task-notification>` block.
        let system = user(
            r#"{"type":"user","uuid":"u","origin":{"kind":"task-notification"},"timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"3 background agents were stopped by the user"}}"#,
        );
        assert!(system.prompt_text().is_some());
        assert!(!system.is_human_prompt());

        // Legacy line (no origin) with plain text → counted via the fallback.
        let legacy = user(
            r#"{"type":"user","uuid":"u","timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"legacy prompt"}}"#,
        );
        assert!(legacy.is_human_prompt());

        // Legacy `<task-notification>` (no origin) → excluded by the fallback.
        let legacy_tn = user(
            "{\"type\":\"user\",\"uuid\":\"u\",\"timestamp\":\"2026-06-05T10:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"<task-notification>\\n<task-id>a1</task-id>\\n<status>stopped</status>\\n</task-notification>\"}}",
        );
        assert!(!legacy_tn.is_human_prompt());

        // Array (tool-result) content is never a prompt, whatever the origin.
        let tool = user(
            r#"{"type":"user","uuid":"u","origin":{"kind":"human"},"timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}}"#,
        );
        assert!(!tool.is_human_prompt());
    }

    // --- parse_line: every entry type, real-shaped fixtures ---------------

    #[test]
    fn assistant_text_thinking_tool_use_with_caller() {
        // Real shape: assistant message with thinking + text + a tool_use that
        // carries the newer `caller` field. parentUuid present (not root).
        // `caller` is a real-world object (`{"type":"direct"}`), not a string.
        let line = r#"{"type":"assistant","uuid":"u1","parentUuid":"p0","timestamp":"2026-06-05T13:51:15.151Z","sessionId":"s","isSidechain":false,"requestId":"req_1","message":{"role":"assistant","model":"claude-opus-4-8","stop_reason":"tool_use","content":[{"type":"thinking","thinking":"let me think","signature":"sig"},{"type":"text","text":"On it."},{"type":"tool_use","id":"toolu_01","name":"Agent","caller":{"type":"direct"},"input":{"description":"do x","prompt":"go","subagent_type":"explorer"}}],"usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":5}}}"#;
        let entry = parse_line(line).expect("assistant parses");
        let Entry::Assistant(a) = entry else {
            panic!("expected assistant, got {entry:?}");
        };
        assert_eq!(a.envelope.uuid.as_deref(), Some("u1"));
        // present-and-non-null parent → Some(Some(..))
        assert_eq!(a.envelope.parent_uuid, Some(Some("p0".to_string())));
        assert!(a.envelope.timestamp.is_some());
        let msg = a.message.expect("message present");
        assert_eq!(msg.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(msg.content.len(), 3);
        assert!(matches!(msg.content[0], ContentBlock::Thinking { .. }));
        assert!(matches!(msg.content[1], ContentBlock::Text { .. }));
        let ContentBlock::ToolUse(tu) = &msg.content[2] else {
            panic!("third block is tool_use");
        };
        assert_eq!(tu.name.as_deref(), Some("Agent"));
        assert_eq!(
            tu.caller.get("type").and_then(|v| v.as_str()),
            Some("direct")
        );
        // Agent input is parseable into the typed view.
        let agent: AgentToolInput =
            serde_json::from_value(tu.input.clone()).expect("agent input parses");
        assert_eq!(agent.subagent_type.as_deref(), Some("explorer"));
        assert_eq!(agent.description.as_deref(), Some("do x"));
        let usage = msg.usage.expect("usage");
        assert_eq!(usage.output_tokens, Some(20));
    }

    #[test]
    fn assistant_root_has_present_null_parent() {
        // The single root entry: parentUuid present AND null → Some(None).
        let line = r#"{"type":"user","uuid":"root","parentUuid":null,"timestamp":"2026-06-05T13:51:00.000Z","sessionId":"s","isSidechain":false,"message":{"role":"user","content":"hi"}}"#;
        let Entry::User(u) = parse_line(line).expect("parses") else {
            panic!("expected user");
        };
        assert_eq!(u.envelope.parent_uuid, Some(None));
    }

    #[test]
    fn user_with_string_content() {
        let line = r#"{"type":"user","uuid":"u2","parentUuid":"u1","timestamp":"2026-06-05T13:52:00.000Z","sessionId":"s","isSidechain":false,"message":{"role":"user","content":"please find the model preferences"}}"#;
        let Entry::User(u) = parse_line(line).expect("parses") else {
            panic!("expected user");
        };
        let msg = u.message.expect("message");
        match msg.content.expect("content") {
            UserContent::Text(t) => assert_eq!(t, "please find the model preferences"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn workflow_launch_reads_run_id_and_name() {
        // `runId` is the `workflows/<id>/` dir name, so it equals the group id.
        let line = r#"{"type":"user","uuid":"u","timestamp":"2026-06-05T10:00:00.000Z","toolUseResult":{"status":"async_launched","taskType":"local_workflow","workflowName":"code-review","runId":"wf_0658f85f-b29","summary":"one finder per angle"}}"#;
        let Some(Entry::User(e)) = parse_line(line) else {
            panic!("expected a user entry");
        };
        let wf = e.workflow_launch().expect("a workflow launch");
        assert_eq!(wf.run_id, "wf_0658f85f-b29");
        assert_eq!(wf.name.as_deref(), Some("code-review"));
        assert_eq!(wf.summary.as_deref(), Some("one finder per angle"));
    }

    #[test]
    fn workflow_launch_ignores_other_tool_results() {
        // A non-workflow toolUseResult, and one with no runId to attribute.
        for line in [
            r#"{"type":"user","uuid":"u","timestamp":"2026-06-05T10:00:00.000Z","toolUseResult":{"status":"ok","taskType":"agent"}}"#,
            r#"{"type":"user","uuid":"u","timestamp":"2026-06-05T10:00:00.000Z","toolUseResult":{"taskType":"local_workflow","workflowName":"x"}}"#,
            r#"{"type":"user","uuid":"u","timestamp":"2026-06-05T10:00:00.000Z","toolUseResult":"a plain string"}"#,
        ] {
            let Some(Entry::User(e)) = parse_line(line) else {
                panic!("expected a user entry");
            };
            assert_eq!(e.workflow_launch(), None, "must not claim a launch: {line}");
        }
    }

    #[test]
    fn user_tool_result_is_error_absent_means_success() {
        // is_error omitted entirely → None (treated as success downstream).
        let line = r#"{"type":"user","uuid":"u3","parentUuid":"a1","timestamp":"2026-06-05T13:53:00.000Z","sessionId":"s","isSidechain":false,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"done ok"}]}}"#;
        let Entry::User(u) = parse_line(line).expect("parses") else {
            panic!("expected user");
        };
        let UserContent::Blocks(blocks) = u.message.unwrap().content.unwrap() else {
            panic!("expected blocks");
        };
        let UserContentBlock::ToolResult(tr) = &blocks[0] else {
            panic!("expected tool_result");
        };
        assert_eq!(tr.tool_use_id.as_deref(), Some("toolu_01"));
        assert_eq!(tr.is_error, None, "absent is_error must be None");
        assert!(matches!(tr.content, Some(ToolResultContent::Text(_))));
    }

    #[test]
    fn user_tool_result_content_as_array() {
        // tool_result.content polymorphic: here an array of blocks.
        let line = r#"{"type":"user","uuid":"u5","parentUuid":"a3","timestamp":"2026-06-05T13:55:00.000Z","sessionId":"s","isSidechain":false,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_03","content":[{"type":"text","text":"line one"},{"type":"text","text":"line two"}]}]}}"#;
        let Entry::User(u) = parse_line(line).expect("parses") else {
            panic!("expected user");
        };
        let UserContent::Blocks(blocks) = u.message.unwrap().content.unwrap() else {
            panic!("expected blocks");
        };
        let UserContentBlock::ToolResult(tr) = &blocks[0] else {
            panic!("expected tool_result");
        };
        match tr.content.as_ref().expect("content") {
            ToolResultContent::Blocks(v) => assert_eq!(v.len(), 2),
            other => panic!("expected array content, got {other:?}"),
        }
    }

    // --- Flat metadata: no uuid/timestamp envelope ------------------------

    #[test]
    fn flat_ai_title_uses_aititle_key() {
        // ai-title carries `aiTitle` (camelCase) and NO envelope.
        let line = r#"{"type":"ai-title","aiTitle":"Find model preferences storage location","sessionId":"s"}"#;
        let Entry::AiTitle(t) = parse_line(line).expect("parses") else {
            panic!("expected ai-title");
        };
        assert_eq!(
            t.title.as_deref(),
            Some("Find model preferences storage location")
        );
    }

    #[test]
    fn flat_metadata_variants_without_envelope() {
        // These lines have NO uuid/parentUuid/timestamp; lean variants must
        // not require envelope fields.
        let cases = [
            (
                r#"{"type":"last-prompt","lastPrompt":"x","leafUuid":"l","sessionId":"s"}"#,
                "last-prompt",
            ),
            (
                r#"{"type":"mode","mode":"default","sessionId":"s"}"#,
                "mode",
            ),
            (
                r#"{"type":"permission-mode","permissionMode":"acceptEdits","sessionId":"s"}"#,
                "permission-mode",
            ),
            (
                r#"{"type":"file-history-snapshot","isSnapshotUpdate":false,"messageId":"m","snapshot":{}}"#,
                "file-history-snapshot",
            ),
            (
                r#"{"type":"queue-operation","operation":"add","content":"c","sessionId":"s","timestamp":"2026-06-05T13:50:00.000Z"}"#,
                "queue-operation",
            ),
        ];
        for (line, label) in cases {
            let entry = parse_line(line).unwrap_or_else(|| panic!("{label} parses"));
            match (label, &entry) {
                ("last-prompt", Entry::LastPrompt(_)) => {}
                ("mode", Entry::Mode(_)) => {}
                ("permission-mode", Entry::PermissionMode(_)) => {}
                ("file-history-snapshot", Entry::FileHistorySnapshot(_)) => {}
                ("queue-operation", Entry::QueueOperation(_)) => {}
                _ => panic!("{label} mis-dispatched to {entry:?}"),
            }
        }
    }

    #[test]
    fn system_and_attachment_entries() {
        let sys = r#"{"type":"system","uuid":"sy1","parentUuid":"p","timestamp":"2026-06-05T13:51:30.000Z","sessionId":"s","isSidechain":false,"subtype":"turn_duration","level":"info"}"#;
        let Entry::System(s) = parse_line(sys).expect("system parses") else {
            panic!("expected system");
        };
        assert_eq!(s.subtype.as_deref(), Some("turn_duration"));

        let att = r#"{"type":"attachment","uuid":"at1","parentUuid":"p","timestamp":"2026-06-05T13:51:31.000Z","sessionId":"s","isSidechain":false,"attachment":{"type":"file"}}"#;
        let Entry::Attachment(a) = parse_line(att).expect("attachment parses") else {
            panic!("expected attachment");
        };
        assert!(a.attachment.is_some());
    }

    // --- Ledger entries ----------------------------------------------------

    #[test]
    fn ledger_started_and_result() {
        let started = r#"{"type":"started","key":"v2:abcd","agentId":"af7dfc2eb54813aec"}"#;
        let Entry::Started(l) = parse_line(started).expect("started parses") else {
            panic!("expected started");
        };
        assert_eq!(l.agent_id.as_deref(), Some("af7dfc2eb54813aec"));
        assert!(l.result.is_none());

        let result = r#"{"type":"result","key":"v2:abcd","agentId":"a0fc04979e8dfcd68","result":{"summary":"done"}}"#;
        let Entry::Result(l) = parse_line(result).expect("result parses") else {
            panic!("expected result");
        };
        assert_eq!(l.agent_id.as_deref(), Some("a0fc04979e8dfcd68"));
        assert!(l.result.is_some());
    }

    #[test]
    fn subagent_lines_carry_agent_id_and_sidechain() {
        // A subagent file line: agentId on every line, isSidechain true.
        let line = r#"{"type":"assistant","uuid":"su1","parentUuid":"x","timestamp":"2026-06-05T13:56:00.000Z","sessionId":"s","isSidechain":true,"agentId":"a5301c73ab04591b2","attributionAgent":"a5301c73ab04591b2","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"text","text":"hi"}]}}"#;
        let Entry::Assistant(a) = parse_line(line).expect("parses") else {
            panic!("expected assistant");
        };
        assert_eq!(a.envelope.agent_id.as_deref(), Some("a5301c73ab04591b2"));
        assert_eq!(a.envelope.is_sidechain, Some(true));
    }

    // --- Unknown / garbage / blank ----------------------------------------

    #[test]
    fn unknown_type_falls_through() {
        // `summary` is documented but unobserved → Unknown.
        let line = r#"{"type":"summary","summary":"whatever","leafUuid":"l"}"#;
        assert!(matches!(parse_line(line), Some(Entry::Unknown)));
        // A wholly novel type also lands on Unknown.
        let novel = r#"{"type":"brand-new-future-type","x":1}"#;
        assert!(matches!(parse_line(novel), Some(Entry::Unknown)));
    }

    #[test]
    fn garbage_and_blank_return_none() {
        assert!(parse_line("not json at all").is_none());
        assert!(parse_line("{ broken json").is_none());
        // skill-injections.jsonl-style line: no `type` field → fails the tag.
        assert!(parse_line(r#"{"content":"x","sessionId":"s"}"#).is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        assert!(parse_line("\n").is_none());
    }

    #[test]
    fn parse_meta_real_shape() {
        let text = r#"{"agentType":"claude-code-guide","description":"Find where Claude Code model preferences are saved","toolUseId":"toolu_01QaU4sRkZ8zoCYdqxWbb8Ey"}"#;
        let meta = parse_meta(text).expect("meta parses");
        assert_eq!(meta.agent_type.as_deref(), Some("claude-code-guide"));
        assert_eq!(
            meta.tool_use_id.as_deref(),
            Some("toolu_01QaU4sRkZ8zoCYdqxWbb8Ey")
        );
        // Workflow subagent meta: only agentType.
        let wf = r#"{"agentType":"workflow-subagent"}"#;
        let m = parse_meta(wf).expect("wf meta parses");
        assert_eq!(m.agent_type.as_deref(), Some("workflow-subagent"));
        assert!(m.description.is_none());
        assert!(m.tool_use_id.is_none());
        assert!(parse_meta("garbage").is_none());
    }
}
