//! The Claude Code provider: [`wire::Entry`] and `meta.json` sidecars in,
//! [`Fact`]s out. Pure functions of one record, so the same code serves replay,
//! live tailing, `inspect`, and the browser.
//!
//! What this module knows that the model must not: which tools spawn agents
//! (`Agent`, `Workflow`), that a `<task-notification>` is an agent's terminal
//! report smuggled through a user message, that `origin.kind` separates a human
//! prompt from injected text, that a `meta.json` names a fork as interactive,
//! and how to render each tool's input as a one-line summary.
//!
//! What it deliberately does not say: `meta.stoppedByUser` is never emitted. The
//! sidecar records the agent's *final* outcome but folds at its *first*
//! activity, so stating it would mark the agent stopped for the whole replay.
//! The timestamped notification is the terminal signal; without one, nothing is
//! emitted and time-derived liveness owns the agent.

use chrono::{DateTime, Utc};

pub mod discovery;
pub mod wire;

use crate::fact::{AgentKind, AgentStatus, Fact, FactKind, Outcome, Statement};
use crate::provider::summary::{short_path, truncate_summary};
use crate::state::session::MAIN_ID;
use wire::{
    AgentToolInput, ContentBlock, Entry, SubagentMeta, TaskStatus, UserContent, UserContentBlock,
    is_spawn_tool, parse_line, parse_task_notification,
};

/// Which file a line came from. Claude spreads one session over a main
/// transcript, per-subagent transcripts and per-workflow ledgers, and the file
/// is what decides which agent a line is *by*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The main `<session-uuid>.jsonl`.
    Main,
    /// A subagent file, keyed by its 17-hex-char `agentId`.
    Sub(String),
    /// A workflow `journal.jsonl`: a lifecycle ledger rather than a
    /// transcript, keyed by the workflow it reports on.
    Ledger(String),
}

/// One unit of Claude input: a parsed transcript line from a known file, or a
/// discovered `meta.json` sidecar.
#[derive(Debug)]
pub enum Record {
    Entry {
        source: Source,
        entry: Entry,
    },
    Meta {
        agent_id: String,
        workflow: Option<String>,
        meta: SubagentMeta,
    },
}

impl Record {
    /// The facts this record states.
    pub fn facts(&self) -> Vec<Fact> {
        match self {
            Record::Entry { source, entry } => facts(source, entry),
            Record::Meta {
                agent_id,
                workflow,
                meta,
            } => meta_facts(agent_id, workflow.as_deref(), meta),
        }
    }

    /// What this record states, as a fresh stream would see it (no inherited
    /// timestamp). `None` if it states nothing.
    pub fn statement(&self) -> Option<Statement> {
        match self {
            Record::Entry { source, entry } => Stream::new(source.clone()).push_entry(entry),
            Record::Meta {
                agent_id,
                workflow,
                meta,
            } => Some(Stream::meta(agent_id, workflow.as_deref(), meta)),
        }
    }
}

/// One Claude file being read: which file it is, and what a line needs from
/// the lines before it. In this format that is only the inherited timestamp: a
/// line without one rides along with its predecessor.
#[derive(Debug, Clone)]
pub struct Stream {
    source: Source,
    last_ts: Option<DateTime<Utc>>,
}

impl Stream {
    pub fn new(source: Source) -> Self {
        Stream {
            source,
            last_ts: None,
        }
    }

    /// Parse one line and state what it says. `None` for a blank or
    /// unparsable line, or one that states nothing.
    pub fn push(&mut self, line: &str) -> Option<Statement> {
        let entry = parse_line(line)?;
        self.push_entry(&entry)
    }

    /// State what an already-parsed entry says.
    pub fn push_entry(&mut self, entry: &Entry) -> Option<Statement> {
        let at = entry_timestamp(entry).or(self.last_ts);
        if at.is_some() {
            self.last_ts = at;
        }
        let mut out = facts(&self.source, entry);
        if out.is_empty() {
            return None;
        }
        for f in &mut out {
            if f.ts.is_none() {
                f.ts = at;
            }
        }
        Some(Statement { at, facts: out })
    }

    /// A `meta.json` sidecar, stated on its own: undated, about the agent it
    /// describes. The timeline places it at that agent's first activity.
    pub fn meta(agent_id: &str, workflow: Option<&str>, meta: &SubagentMeta) -> Statement {
        Statement {
            at: None,
            facts: meta_facts(agent_id, workflow, meta),
        }
    }
}

/// The envelope timestamp of an entry, if it carries one.
fn entry_timestamp(entry: &Entry) -> Option<DateTime<Utc>> {
    match entry {
        Entry::User(e) => e.envelope.timestamp,
        Entry::Assistant(e) => e.envelope.timestamp,
        Entry::System(e) => e.envelope.timestamp,
        Entry::Attachment(e) => e.envelope.timestamp,
        _ => None,
    }
}

/// Facts stated by one transcript line, given which file it came from.
pub fn facts(source: &Source, entry: &Entry) -> Vec<Fact> {
    let mut out = Vec::new();
    match entry {
        Entry::Assistant(e) => {
            let Some(owner) = owner_of(source) else {
                return out;
            };
            let ts = e.envelope.timestamp;
            let about = |kind| Fact {
                agent: Some(owner.clone()),
                ts,
                kind,
            };
            if let Some(msg) = &e.message {
                if let Some(model) = &msg.model {
                    out.push(about(FactKind::Model(model.clone())));
                }
                if let Some(usage) = &msg.usage
                    && let Some(output) = usage.output_tokens
                {
                    // One turn spans several lines that repeat the same cumulative
                    // usage; `requestId` is the key they share.
                    out.push(about(FactKind::Tokens {
                        output,
                        dedup: e.envelope.request_id.clone(),
                    }));
                }
                // Blocks in order: a spawn carries the text nearest above it as
                // its stated reason, which the fold reads off the last Reasoning.
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            out.push(about(FactKind::Reasoning(text.clone())));
                        }
                        ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                            out.push(about(FactKind::Reasoning(thinking.clone())));
                        }
                        ContentBlock::ToolUse(tu) => {
                            let Some(call) = &tu.id else { continue };
                            let name = tu.name.clone().unwrap_or_default();
                            let summary =
                                summarize_tool(&name, &tu.input, e.envelope.cwd.as_deref());
                            out.push(about(FactKind::ToolStart {
                                call: call.clone(),
                                name: name.clone(),
                                summary,
                            }));
                            if is_spawn_tool(&name) {
                                out.push(about(FactKind::Spawn { call: call.clone() }));
                            }
                        }
                        _ => {}
                    }
                }
            }
            ensure_activity(&mut out, &owner, ts);
        }
        Entry::User(e) => {
            let Some(owner) = owner_of(source) else {
                return out;
            };
            let ts = e.envelope.timestamp;
            let is_main = matches!(source, Source::Main);
            // A workflow launch ack names the group. Naming only: the group is
            // born from its first subagent, never from the launch.
            if is_main && let Some(wf) = e.workflow_launch() {
                out.push(Fact {
                    agent: Some(wf.run_id),
                    ts,
                    kind: FactKind::Label {
                        agent_type: wf.name,
                        description: wf.summary,
                    },
                });
            }
            // Main-thread user strings are one of three things: an async agent's
            // terminal report, other injected text, or a human prompt. Only the
            // first two are facts; injected notices are nothing.
            if is_main && let Some(text) = e.prompt_text() {
                if let Some(tn) = parse_task_notification(text) {
                    if let Some(status) = terminal_status(tn.status) {
                        out.push(Fact {
                            agent: Some(tn.agent_id),
                            ts,
                            kind: FactKind::Ended(status),
                        });
                    }
                } else if e.is_human_prompt() {
                    out.push(Fact {
                        agent: Some(owner.clone()),
                        ts,
                        kind: FactKind::Prompt(text.to_string()),
                    });
                }
            }
            // A missing `is_error` means success in this format (verified against
            // real data), so every result carries an outcome.
            if let Some(msg) = &e.message
                && let Some(UserContent::Blocks(blocks)) = &msg.content
            {
                for block in blocks {
                    if let UserContentBlock::ToolResult(r) = block
                        && let Some(call) = &r.tool_use_id
                    {
                        let outcome = if r.is_error == Some(true) {
                            Outcome::Err
                        } else {
                            Outcome::Ok
                        };
                        out.push(Fact {
                            agent: Some(owner.clone()),
                            ts,
                            kind: FactKind::ToolEnd {
                                call: call.clone(),
                                outcome,
                            },
                        });
                    }
                }
            }
            ensure_activity(&mut out, &owner, ts);
        }
        // A workflow journal `result` is the ledger's completion for the agent it
        // names. Undated in the format; the timeline dates it by that agent.
        Entry::Result(ledger) if matches!(source, Source::Ledger(_)) => {
            if let Some(id) = &ledger.agent_id {
                out.push(Fact {
                    agent: Some(id.clone()),
                    ts: None,
                    kind: FactKind::Ended(AgentStatus::Done),
                });
            }
        }
        // Untimed session metadata, off the timeline.
        Entry::AiTitle(e) => {
            if let Some(title) = &e.title {
                out.push(meta(FactKind::Title(title.clone())));
            }
        }
        Entry::Mode(e) => push_session(&mut out, "mode", str_field(&e.fields, "mode")),
        Entry::PermissionMode(e) => push_session(
            &mut out,
            "permission",
            str_field(&e.fields, "permissionMode"),
        ),
        Entry::LastPrompt(e) => {
            push_session(&mut out, "last prompt", str_field(&e.fields, "lastPrompt"))
        }
        Entry::QueueOperation(e) => {
            if str_field(&e.fields, "operation").as_deref() == Some("enqueue") {
                out.push(meta(FactKind::Tally("queued".into())));
            }
        }
        Entry::FileHistorySnapshot(_) => out.push(meta(FactKind::Tally("file edits".into()))),
        // System, attachment, `started` ledgers, unknown: nothing is stated.
        _ => {}
    }
    out
}

/// Facts stated by a discovered `meta.json` sidecar.
///
/// One fact: the subagent itself. A workflow subagent's parent is its group,
/// which the fold creates on demand (a group is born from its first child).
/// The sidecar is undated; the timeline places it at the agent's first activity.
pub fn meta_facts(agent_id: &str, workflow: Option<&str>, meta: &SubagentMeta) -> Vec<Fact> {
    vec![Fact {
        agent: Some(agent_id.to_string()),
        ts: None,
        kind: FactKind::Agent {
            kind: AgentKind::Subagent,
            parent: Some(workflow.unwrap_or(MAIN_ID).to_string()),
            agent_type: meta.agent_type.clone(),
            description: meta.description.clone(),
            spawned_by: meta.tool_use_id.clone(),
            // Forks are interactive sidechains: verified to carry no completion
            // marker anywhere in the format.
            interactive: meta.agent_type.as_deref() == Some("fork"),
        },
    }]
}

/// The agent whose transcript a line came from. Ledgers are about agents, not
/// by one.
fn owner_of(source: &Source) -> Option<String> {
    match source {
        Source::Main => Some(MAIN_ID.to_string()),
        Source::Sub(id) => Some(id.clone()),
        Source::Ledger(_) => None,
    }
}

/// A line is its owner's activity even when it states nothing else.
fn ensure_activity(out: &mut Vec<Fact>, owner: &str, ts: Option<DateTime<Utc>>) {
    if !out.iter().any(|f| f.agent.as_deref() == Some(owner)) {
        out.push(Fact {
            agent: Some(owner.to_string()),
            ts,
            kind: FactKind::Activity,
        });
    }
}

/// A `<task-notification>` status the model can act on. An unrecognised status
/// string states nothing rather than overriding derived liveness.
fn terminal_status(status: TaskStatus) -> Option<AgentStatus> {
    match status {
        TaskStatus::Completed => Some(AgentStatus::Done),
        TaskStatus::Stopped => Some(AgentStatus::Stopped),
        TaskStatus::Failed => Some(AgentStatus::Failed),
        TaskStatus::Other => None,
    }
}

fn meta(kind: FactKind) -> Fact {
    Fact {
        agent: None,
        ts: None,
        kind,
    }
}

fn push_session(out: &mut Vec<Fact>, label: &str, value: Option<String>) {
    if let Some(value) = value {
        out.push(meta(FactKind::Session {
            label: label.into(),
            value,
        }));
    }
}

fn str_field(fields: &serde_json::Value, key: &str) -> Option<String> {
    fields.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Tool summaries: the Claude tool vocabulary and what makes a good one-liner
// ---------------------------------------------------------------------------

/// Derive a short one-line summary from a tool_use input, if a natural field
/// exists for the tool. Defensive: any shape that doesn't match yields `None`.
pub(crate) fn summarize_tool(
    name: &str,
    input: &serde_json::Value,
    cwd: Option<&str>,
) -> Option<String> {
    let pick = |key: &str| {
        input
            .get(key)
            .and_then(|v| v.as_str())
            .map(truncate_summary)
    };
    // File paths: show project-relative (`src/main.rs`) instead of the absolute
    // path, and keep the basename if it still needs truncating.
    let pick_path = |key: &str| {
        input
            .get(key)
            .and_then(|v| v.as_str())
            .map(|p| short_path(p, cwd))
    };
    match name {
        "Bash" => pick("command").or_else(|| pick("description")),
        "Read" | "Write" | "Edit" => pick_path("file_path").or_else(|| pick_path("path")),
        n if wire::is_spawn_tool(n) => {
            // Prefer the typed view for description/subagent_type.
            let typed: AgentToolInput = serde_json::from_value(input.clone()).unwrap_or_default();
            typed
                .description
                .or(typed.subagent_type)
                .map(|s| truncate_summary(&s))
        }
        "WebFetch" => pick("url"),
        "ToolSearch" => pick("query"),
        _ => pick("description").or_else(|| pick("query")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped demo session, discovered the way a live one is, stated per
    /// file, and run through the whole conformance check: order invariance
    /// plus the model and timeline goldens beside the fixture. A Codex provider
    /// proves itself with the same call over `assets/codex/`.
    #[test]
    fn demo_session_conforms() {
        let Some(root) = crate::provider::harness::fixture_dir("claude") else {
            return;
        };
        let main = root.join("demo.jsonl");
        let streams = || {
            let file = |path: &std::path::Path, source: Source| -> Vec<Statement> {
                let text = std::fs::read_to_string(path).unwrap();
                let mut stream = Stream::new(source);
                text.lines().filter_map(|l| stream.push(l)).collect()
            };
            let mut streams = vec![file(&main, Source::Main)];
            let subs = discovery::subagents_dir(&main).unwrap();
            let agent = |f: discovery::SubagentFile| -> Vec<Statement> {
                let mut s = Vec::new();
                if let Some(meta) = std::fs::read_to_string(&f.meta)
                    .ok()
                    .and_then(|t| wire::parse_meta(&t))
                {
                    s.push(Stream::meta(&f.agent_id, f.workflow.as_deref(), &meta));
                }
                s.extend(file(&f.transcript, Source::Sub(f.agent_id.clone())));
                s
            };
            for f in discovery::scan_subagent_files(&subs, None) {
                streams.push(agent(f));
            }
            for wf in discovery::scan_workflow_ids(&subs) {
                for f in
                    discovery::scan_subagent_files(&discovery::workflow_dir(&subs, &wf), Some(&wf))
                {
                    streams.push(agent(f));
                }
                let journal = discovery::workflow_journal(&subs, &wf);
                streams.push(file(&journal, Source::Ledger(wf.clone())));
            }
            streams
        };
        crate::provider::harness::conform("claude", "demo", streams);
    }

    #[test]
    fn summarize_tool_relativizes_paths() {
        // File paths show project-relative, not absolute.
        let edit = serde_json::json!({ "file_path": "/proj/src/main.rs" });
        assert_eq!(
            summarize_tool("Edit", &edit, Some("/proj")).as_deref(),
            Some("src/main.rs")
        );
        // A path outside the cwd is kept as-is.
        let outside = serde_json::json!({ "file_path": "/other/x.rs" });
        assert_eq!(
            summarize_tool("Read", &outside, Some("/proj")).as_deref(),
            Some("/other/x.rs")
        );
        // Non-path tools are unaffected (command kept, head-truncated elsewhere).
        let bash = serde_json::json!({ "command": "cargo test" });
        assert_eq!(
            summarize_tool("Bash", &bash, Some("/proj")).as_deref(),
            Some("cargo test")
        );
    }
}
