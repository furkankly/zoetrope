//! Serde data model for Claude Code JSONL transcript entries, `meta.json`
//! sidecars, and project-directory discovery / sanitization.
//!
//! The transcript format is undocumented and internal to Claude Code, so every
//! type here is built to be **defensive**: unknown entry types fall through to
//! [`Entry::Unknown`], envelope fields are `Option` so flat metadata lines
//! deserialize into lean variants, polymorphic `string | array` payloads use
//! untagged enums, and a missing `is_error` is treated as success. Parsing a
//! single line must never panic — callers skip lines that fail to deserialize.
//!
//! Many DTO fields below are deserialized for format fidelity but not read by
//! the session model (e.g. `signature`, `stop_reason`, `request_id`, the
//! lean-metadata payloads). They are retained deliberately — they document the
//! verified wire format and keep each variant a concrete struct so a new
//! Claude Code field can be promoted to a real consumer without reshaping the
//! parser. Hence the module-scoped `dead_code` allow; it covers these wire
//! fields, not logic.
#![allow(dead_code)]

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

impl Entry {
    /// Untimed session-level metadata that is routed to [`SessionInfo`] and kept
    /// OFF the timeline (`ai-title`, `last-prompt`, `mode`, `permission-mode`,
    /// `file-history-snapshot`, `queue-operation`). None of these are timed
    /// activity: being untimed they would otherwise sort to the front and push
    /// the scrubber's start off the left edge. `ai-title` in particular is
    /// session identity (the header title), not an event — so it lives in the
    /// info store, never as a fabricated-date timeline item.
    ///
    /// [`SessionInfo`]: crate::state::SessionInfo
    pub fn is_timeline_noise(&self) -> bool {
        matches!(
            self,
            Entry::AiTitle(_)
                | Entry::LastPrompt(_)
                | Entry::Mode(_)
                | Entry::PermissionMode(_)
                | Entry::FileHistorySnapshot(_)
                | Entry::QueueOperation(_)
        )
    }

    /// Number of `tool_use` blocks in this entry (an assistant turn may issue
    /// several). Drives the scrubber's tool-activity sparkline. Non-assistant
    /// entries are 0.
    pub fn tool_use_count(&self) -> usize {
        match self {
            Entry::Assistant(e) => e.message.as_ref().map_or(0, |m| {
                m.content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolUse(_)))
                    .count()
            }),
            _ => 0,
        }
    }

    /// Number of agent/workflow *spawn* tool calls in this entry (`Agent` /
    /// `Workflow` tools) — the branch points, marked distinctly on the scrubber.
    pub fn spawn_count(&self) -> usize {
        self.spawn_tool_use_ids().len()
    }

    /// The `tool_use.id`s of the agent/workflow *spawn* calls in this entry. Used
    /// as the join key to the subagent it spawns: a discovered subagent's meta
    /// carries the same id, so a spawn with a known meta is marked at the
    /// subagent's birth instead of here at the call. Empty for non-spawn entries.
    pub fn spawn_tool_use_ids(&self) -> Vec<&str> {
        match self {
            Entry::Assistant(e) => e.message.as_ref().map_or(Vec::new(), |m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse(tu)
                            if tu.name.as_deref().is_some_and(is_spawn_tool) =>
                        {
                            Some(tu.id.as_deref().unwrap_or_default())
                        }
                        _ => None,
                    })
                    .collect()
            }),
            _ => Vec::new(),
        }
    }

    /// Number of *failed* tool results in this entry (`tool_result` with
    /// `is_error: true`) — surfaced as a distinct marker so failures aren't
    /// invisible on the timeline. (Results land in `user` entries.)
    pub fn tool_failure_count(&self) -> usize {
        match self {
            Entry::User(e) => match e.message.as_ref().and_then(|m| m.content.as_ref()) {
                Some(UserContent::Blocks(blocks)) => blocks
                    .iter()
                    .filter(|b| {
                        matches!(b, UserContentBlock::ToolResult(tr) if tr.is_error == Some(true))
                    })
                    .count(),
                _ => 0,
            },
            _ => 0,
        }
    }
}

/// Whether a tool name is an agent/workflow *spawn* (a branch point). Single
/// source for the rule — `Task` is Claude Code's legacy name for the `Agent`
/// tool, so all three count as spawns (provenance, scrubber markers, summaries).
pub fn is_spawn_tool(name: &str) -> bool {
    matches!(name, "Agent" | "Task" | "Workflow")
}

/// Session id from a transcript path: the file stem (`<uuid>.jsonl` → `<uuid>`),
/// lossy, empty if the path has no stem. Single source for the id-from-path rule.
pub fn session_id_from_path(path: &std::path::Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
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
    pub prompt: Option<String>,
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

// ---------------------------------------------------------------------------
// Directory discovery / sanitization
// ---------------------------------------------------------------------------

/// Sanitize an absolute cwd into the project-directory name Claude Code uses:
/// every character that is not `[a-zA-Z0-9]` becomes a single `-`, one-to-one
/// (a leading slash becomes a leading dash; `/Users/me/.config` →
/// `-Users-me--config`). This is Claude Code's documented rule — "non-
/// alphanumeric characters replaced by `-`" — so `.`, `_`, and spaces all map
/// to dashes, not just path separators.
///
/// Operates on the string form of the path so it is platform-agnostic and never
/// touches the filesystem. The canonical input is an absolute Unix path.
pub fn sanitize_cwd(cwd: &std::path::Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Environment override for the projects root.
///
/// Set it to point zoetrope at a different tree of transcripts. It exists
/// because the multi-session rail is otherwise only exercisable by running
/// several real Claude Code sessions at once: with this, a fixture workspace
/// is a directory. It relocates discovery AND [`project_dir`] together, so the
/// whole app sees one consistent root rather than a half-redirected one.
pub const PROJECTS_ROOT_ENV: &str = "ZOE_PROJECTS_ROOT";

/// The user's home directory, or `None` if it cannot be resolved.
///
/// Shared so every dotfile-style root zoetrope reads or writes agrees on where
/// home is — `std::env::home_dir` with a `$HOME` fallback, both rejecting an
/// empty value (which would silently resolve to the filesystem root).
pub fn home_dir() -> Option<std::path::PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir()
        .filter(|h| !h.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(std::path::PathBuf::from)
        })
}

/// The `~/.claude/projects` root, or whatever [`PROJECTS_ROOT_ENV`] names.
pub fn claude_projects_root() -> Option<std::path::PathBuf> {
    if let Some(root) = std::env::var_os(PROJECTS_ROOT_ENV).filter(|v| !v.is_empty()) {
        return Some(std::path::PathBuf::from(root));
    }
    Some(home_dir()?.join(".claude").join("projects"))
}

/// Absolute path to the `~/.claude/projects/<sanitized-cwd>` directory for a
/// given cwd.
pub fn project_dir(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    Some(claude_projects_root()?.join(sanitize_cwd(cwd)))
}

/// Whether a filename stem is a canonical lowercase UUID (8-4-4-4-12 hex).
///
/// Transcript files are exactly `<uuid>.jsonl`; this filter rejects sidecars
/// like `skill-injections.jsonl` and metadata like `sessions-index.json`.
fn is_uuid(stem: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut parts = stem.split('-');
    for &len in &GROUPS {
        match parts.next() {
            Some(p) if p.len() == len && p.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

/// Whether a path is a `<uuid>.jsonl` transcript file (UUID stem + `.jsonl`).
pub fn is_session_file(path: &std::path::Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return false;
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(is_uuid)
}

/// Find the newest `<uuid>.jsonl` transcript directly inside `project_dir`
/// (ignoring non-transcript files like `skill-injections.jsonl`,
/// `sessions-index.json`, and subdirectories).
pub fn latest_session_file(project_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(project_dir).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !is_session_file(&path) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        // Newest wins; equal mtimes break ties on the (lexicographically greater)
        // path so the choice is deterministic, not `read_dir` order.
        let better = match &best {
            None => true,
            Some((bt, bp)) => mtime > *bt || (mtime == *bt && path > *bp),
        };
        if better {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

// ---------------------------------------------------------------------------
// Subagent directory scanning
// ---------------------------------------------------------------------------

/// A discovered subagent file pair inside a `subagents/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentFile {
    /// The 17-hex `agentId` parsed from the `agent-<id>.jsonl` filename.
    pub agent_id: String,
    /// Absolute path to the `agent-<id>.jsonl` transcript.
    pub transcript: std::path::PathBuf,
    /// Absolute path to the `agent-<id>.meta.json` sidecar (may not exist yet).
    pub meta: std::path::PathBuf,
    /// `Some(wf_id)` when this lives under `subagents/workflows/<wf-id>/`.
    pub workflow: Option<String>,
}

/// The `subagents/` directory for a session transcript path.
///
/// `<dir>/<uuid>.jsonl` → `<dir>/<uuid>/subagents`.
pub fn subagents_dir(session_file: &std::path::Path) -> Option<std::path::PathBuf> {
    let parent = session_file.parent()?;
    let stem = session_file.file_stem()?.to_str()?;
    Some(parent.join(stem).join("subagents"))
}

/// Extract the `agentId` from an `agent-<id>.jsonl` filename, if it matches.
fn agent_id_from_filename(path: &std::path::Path) -> Option<String> {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    stem.strip_prefix("agent-")
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Scan a single `subagents/` (or `subagents/workflows/<wf-id>/`) directory for
/// `agent-*.jsonl` files, pairing each with its `.meta.json` sidecar.
///
/// `workflow` tags the discovered files; missing directories yield an empty
/// vec (subagent dirs are created lazily — that is expected, never an error).
pub fn scan_subagent_files(dir: &std::path::Path, workflow: Option<&str>) -> Vec<SubagentFile> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Some(agent_id) = agent_id_from_filename(&path) else {
            continue;
        };
        let meta = dir.join(format!("agent-{agent_id}.meta.json"));
        out.push(SubagentFile {
            agent_id,
            transcript: path,
            meta,
            workflow: workflow.map(str::to_owned),
        });
    }
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    out
}

/// List workflow ids found under `subagents/workflows/` (each is a directory
/// containing a `journal.jsonl` plus its own `agent-*.jsonl` files).
pub fn scan_workflow_ids(subagents_dir: &std::path::Path) -> Vec<String> {
    let workflows = subagents_dir.join("workflows");
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(&workflows) else {
        return out;
    };
    for entry in read.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            && let Some(name) = entry.file_name().to_str()
        {
            out.push(name.to_owned());
        }
    }
    out.sort();
    out
}

/// The `journal.jsonl` ledger path for a workflow inside a `subagents/` dir.
pub fn workflow_journal(subagents_dir: &std::path::Path, wf_id: &str) -> std::path::PathBuf {
    subagents_dir
        .join("workflows")
        .join(wf_id)
        .join("journal.jsonl")
}

/// The `subagents/workflows/<wf-id>` directory that holds a workflow's agents.
pub fn workflow_dir(subagents_dir: &std::path::Path, wf_id: &str) -> std::path::PathBuf {
    subagents_dir.join("workflows").join(wf_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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
    fn tool_use_count_counts_blocks() {
        // An assistant turn issuing two tool calls → count 2.
        let line = r#"{"type":"assistant","uuid":"a","timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"on it"},{"type":"tool_use","id":"t1","name":"Bash","input":{}},{"type":"tool_use","id":"t2","name":"Read","input":{}}]}}"#;
        assert_eq!(parse_line(line).unwrap().tool_use_count(), 2);
        // A user entry has none.
        let u = r#"{"type":"user","uuid":"u","timestamp":"2026-06-05T10:00:01.000Z","message":{"role":"user","content":"hi"}}"#;
        assert_eq!(parse_line(u).unwrap().tool_use_count(), 0);
        // Spawn count: only Agent/Workflow tools.
        let s = r#"{"type":"assistant","uuid":"a","message":{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"Agent","input":{}},{"type":"tool_use","id":"t2","name":"Bash","input":{}}]}}"#;
        assert_eq!(parse_line(s).unwrap().spawn_count(), 1);
    }

    #[test]
    fn tool_failure_count_counts_errored_results() {
        let fail = r#"{"type":"user","uuid":"u","timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"boom","is_error":true},{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}"#;
        assert_eq!(parse_line(fail).unwrap().tool_failure_count(), 1);
        // No error flag → success → not counted.
        let ok = r#"{"type":"user","uuid":"u","timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"done"}]}}"#;
        assert_eq!(parse_line(ok).unwrap().tool_failure_count(), 0);
    }

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

    // --- Sanitization ------------------------------------------------------

    #[test]
    fn sanitize_cwd_rule() {
        assert_eq!(
            sanitize_cwd(Path::new("/Users/furkan/personal/projects/flyradar")),
            "-Users-furkan-personal-projects-flyradar"
        );
        // Leading slash → leading dash; root stays a single dash.
        assert_eq!(sanitize_cwd(Path::new("/")), "-");
        assert_eq!(sanitize_cwd(Path::new("/a")), "-a");
        // Every non-alphanumeric char maps to a dash, one-to-one: a dotfile dir
        // yields a double dash (slash + dot), and `_`/spaces become dashes too.
        assert_eq!(
            sanitize_cwd(Path::new("/Users/me/.config/foo")),
            "-Users-me--config-foo"
        );
        assert_eq!(
            sanitize_cwd(Path::new("/Users/me/my_project v2")),
            "-Users-me-my-project-v2"
        );
    }

    // --- UUID filename filter ---------------------------------------------

    #[test]
    fn uuid_filename_filter() {
        assert!(is_uuid("0e599cbe-23c4-460b-b097-cbd1d6bc0e3d"));
        assert!(is_uuid("55badaf6-c5d2-4b85-af5b-f41f42b3a8a7"));
        // Wrong group lengths / shapes.
        assert!(!is_uuid("0e599cbe-23c4-460b-b097-cbd1d6bc0e3"));
        assert!(!is_uuid("0e599cbe23c4460bb097cbd1d6bc0e3d"));
        assert!(!is_uuid("skill-injections"));
        assert!(!is_uuid("sessions-index"));
        // Non-hex characters rejected.
        assert!(!is_uuid("zzzzzzzz-23c4-460b-b097-cbd1d6bc0e3d"));
        // Trailing group rejected.
        assert!(!is_uuid("0e599cbe-23c4-460b-b097-cbd1d6bc0e3d-extra"));
    }

    #[test]
    fn is_session_file_only_uuid_jsonl() {
        assert!(is_session_file(Path::new(
            "/p/0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.jsonl"
        )));
        // Rejected: known non-transcript sidecars and wrong extensions.
        assert!(!is_session_file(Path::new("/p/skill-injections.jsonl")));
        assert!(!is_session_file(Path::new("/p/sessions-index.json")));
        assert!(!is_session_file(Path::new("/p/journal.jsonl")));
        assert!(!is_session_file(Path::new(
            "/p/0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.json"
        )));
    }

    // --- Subagent path helpers --------------------------------------------

    /// The projects-root override must relocate discovery and `project_dir`
    /// together — a half-redirected app would list fixture sessions while
    /// resolving the current project against the real home directory.
    ///
    /// Serialized against other env-touching tests by running them in one test
    /// function: `set_var` is process-global and Rust runs tests in threads.
    #[test]
    fn the_projects_root_override_relocates_everything() {
        let real = claude_projects_root();

        // SAFETY: no other thread reads this variable during this test; the
        // override is restored before returning.
        unsafe { std::env::set_var(PROJECTS_ROOT_ENV, "/tmp/zoe-fixture") };
        assert_eq!(
            claude_projects_root(),
            Some(std::path::PathBuf::from("/tmp/zoe-fixture"))
        );
        assert_eq!(
            project_dir(std::path::Path::new("/home/u/repo/app")),
            Some(std::path::PathBuf::from(
                "/tmp/zoe-fixture/-home-u-repo-app"
            ))
        );

        // Empty is treated as unset, not as the filesystem root.
        unsafe { std::env::set_var(PROJECTS_ROOT_ENV, "") };
        assert_eq!(claude_projects_root(), real);

        unsafe { std::env::remove_var(PROJECTS_ROOT_ENV) };
        assert_eq!(claude_projects_root(), real);
    }

    #[test]
    fn subagents_dir_derivation() {
        let session = Path::new("/root/-proj/0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.jsonl");
        let dir = subagents_dir(session).expect("derivable");
        assert_eq!(
            dir,
            Path::new("/root/-proj/0e599cbe-23c4-460b-b097-cbd1d6bc0e3d/subagents")
        );
    }

    #[test]
    fn agent_id_from_filename_parsing() {
        assert_eq!(
            agent_id_from_filename(Path::new("/x/agent-a5301c73ab04591b2.jsonl")).as_deref(),
            Some("a5301c73ab04591b2")
        );
        // meta.json is not a transcript file.
        assert!(
            agent_id_from_filename(Path::new("/x/agent-a5301c73ab04591b2.meta.json")).is_none()
        );
        // No agent- prefix.
        assert!(agent_id_from_filename(Path::new("/x/journal.jsonl")).is_none());
        // Empty id rejected.
        assert!(agent_id_from_filename(Path::new("/x/agent-.jsonl")).is_none());
    }

    #[test]
    fn workflow_path_helpers() {
        let sub = Path::new("/s/subagents");
        assert_eq!(
            workflow_journal(sub, "wf_6e734a65-3c6"),
            Path::new("/s/subagents/workflows/wf_6e734a65-3c6/journal.jsonl")
        );
        assert_eq!(
            workflow_dir(sub, "wf_6e734a65-3c6"),
            Path::new("/s/subagents/workflows/wf_6e734a65-3c6")
        );
    }

    #[test]
    fn scan_missing_dir_is_empty_not_error() {
        // Lazily-created dirs: scanning a nonexistent path is a no-op.
        let missing = Path::new("/definitely/not/a/real/zoetrope/subagents/xyz");
        assert!(scan_subagent_files(missing, None).is_empty());
        assert!(scan_workflow_ids(missing).is_empty());
    }

    #[test]
    fn scan_subagent_files_pairs_transcript_and_meta() {
        let tmp = std::env::temp_dir().join(format!(
            "zoetrope-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        std::fs::write(tmp.join("agent-a5301c73ab04591b2.jsonl"), b"{}\n").unwrap();
        std::fs::write(tmp.join("agent-a5301c73ab04591b2.meta.json"), b"{}").unwrap();
        std::fs::write(tmp.join("agent-a9dd56e1137830d9d.jsonl"), b"{}\n").unwrap();
        // Noise that must be ignored.
        std::fs::write(tmp.join("journal.jsonl"), b"{}\n").unwrap();
        std::fs::write(tmp.join("readme.txt"), b"x").unwrap();

        let found = scan_subagent_files(&tmp, Some("wf_x"));
        assert_eq!(found.len(), 2, "two agent transcripts, noise ignored");
        // Sorted by agent_id.
        assert_eq!(found[0].agent_id, "a5301c73ab04591b2");
        assert_eq!(found[1].agent_id, "a9dd56e1137830d9d");
        assert_eq!(found[0].workflow.as_deref(), Some("wf_x"));
        assert_eq!(found[0].meta, tmp.join("agent-a5301c73ab04591b2.meta.json"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn latest_session_file_picks_newest_uuid_jsonl() {
        let tmp = std::env::temp_dir().join(format!(
            "zoetrope-latest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let older = tmp.join("11111111-1111-1111-1111-111111111111.jsonl");
        let newer = tmp.join("22222222-2222-2222-2222-222222222222.jsonl");
        // Non-transcript files must be ignored even if they are the newest.
        std::fs::write(tmp.join("skill-injections.jsonl"), b"{}\n").unwrap();
        std::fs::write(tmp.join("sessions-index.json"), b"{}\n").unwrap();
        std::fs::write(&older, b"{}\n").unwrap();
        // Ensure a real mtime gap across coarse-granularity filesystems, then
        // write `newer` strictly after `older`.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&newer, b"{}\n").unwrap();

        let latest = latest_session_file(&tmp).expect("finds one");
        assert_eq!(latest, newer, "newest uuid .jsonl wins; sidecars ignored");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
