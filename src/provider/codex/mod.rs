//! The Codex provider: rollout lines in, [`Fact`]s out.
//!
//! One rollout file is one thread, and every line in it is *by* that thread,
//! which the file's first `session_meta` names. So unlike Claude, where the
//! file decides the owner, a Codex [`Stream`] learns its owner from its first
//! line and needs that state for every line after. Two more pieces of state
//! come from the same line: the root thread's id, so references to it map to
//! the model's `main`, and the ordinal below which the file is replayed parent
//! history rather than the thread's own record.
//!
//! What this module knows that the model must not: that `spawn_agent` spawns
//! and `wait_agent` only waits (its output names nobody and is never an
//! ending); that the `exec` tool is a program whose real operations surface
//! as `item_completed` items with their own ids and exit codes, so both layers
//! are stated; that `Script completed` at the head of an `exec` output is its
//! verdict; that `task_started`/`task_complete` are turn boundaries, not an
//! agent's lifetime; that `SubAgentActivity{completed}` is the one record that
//! observes a child's end, and that some versions never write it.

use chrono::{DateTime, Utc};

pub mod discovery;
pub mod wire;

use crate::fact::{AgentKind, AgentStatus, Fact, FactKind, Outcome, Statement};
use crate::provider::summary::{short_path, truncate_summary};
use crate::state::session::MAIN_ID;
use wire::{EventMsg, Item, Line, Payload, ResponseItem, SessionMeta, parse_line, path_leaf};

/// One rollout file being read.
#[derive(Debug, Clone, Default)]
pub struct Stream {
    /// This file's thread, from its first `session_meta`. Lines before it
    /// (there should be none) state nothing.
    thread: Option<String>,
    /// The root thread of the session: maps to `main`.
    root: Option<String>,
    /// First ordinal that is this thread's own.
    own_from: Option<u64>,
    /// For relativising paths in summaries.
    cwd: Option<String>,
    /// The cumulative output tokens last seen, so each `token_count` states
    /// only what it added.
    output_tokens: u64,
}

impl Stream {
    pub fn new() -> Self {
        Stream::default()
    }

    /// Parse one line and state what it says. `None` for a blank or
    /// unparsable line, replayed parent history, or a line that states
    /// nothing.
    pub fn push(&mut self, line: &str) -> Option<Statement> {
        let line = parse_line(line)?;
        self.push_line(&line)
    }

    /// State what an already-parsed line says.
    pub fn push_line(&mut self, line: &Line) -> Option<Statement> {
        if let Payload::SessionMeta(meta) = &line.payload {
            if self.thread.is_some() {
                // The parent's meta, replayed into a child. Not ours.
                return None;
            }
            return self.adopt(meta, line.timestamp);
        }
        let owner = self.owner()?;
        if let (Some(ord), Some(from)) = (line.ordinal, self.own_from)
            && ord < from
        {
            return None;
        }
        let ts = line.timestamp;
        let mut facts = self.facts(&owner, line);
        if facts.is_empty() {
            return None;
        }
        for f in &mut facts {
            if f.ts.is_none() {
                f.ts = ts;
            }
        }
        Some(Statement { at: ts, facts })
    }

    /// The first line: learn whose file this is, and state that agent.
    fn adopt(&mut self, meta: &SessionMeta, at: Option<DateTime<Utc>>) -> Option<Statement> {
        let id = meta.id.clone()?;
        let root = if meta.is_root() {
            id.clone()
        } else {
            meta.session_id.clone().unwrap_or_else(|| id.clone())
        };
        self.thread = Some(id.clone());
        self.root = Some(root);
        self.own_from = meta.subagent_history_start_ordinal;
        self.cwd = meta.cwd.clone();
        let owner = self.owner()?;
        let ts = meta.timestamp.or(at);
        let mut facts = Vec::new();
        if meta.is_root() {
            facts.push(Fact {
                agent: Some(owner.clone()),
                ts,
                kind: FactKind::Agent {
                    kind: AgentKind::Main,
                    parent: None,
                    agent_type: Some("codex".into()),
                    description: None,
                    spawned_by: None,
                    interactive: true,
                },
            });
            let session = |label: &str, value: &Option<String>| {
                value.as_ref().map(|v| Fact {
                    agent: None,
                    ts: None,
                    kind: FactKind::Session {
                        label: label.into(),
                        value: v.clone(),
                    },
                })
            };
            facts.extend(session("app", &meta.originator));
            facts.extend(session("version", &meta.cli_version));
            facts.extend(session("cwd", &meta.cwd));
        } else {
            facts.push(Fact {
                agent: Some(owner.clone()),
                ts,
                kind: FactKind::Agent {
                    kind: AgentKind::Subagent,
                    parent: meta.parent().map(|p| self.node_id(p)),
                    agent_type: meta.path().map(|p| path_leaf(p).to_string()),
                    description: meta.nickname().map(str::to_owned),
                    // The spawning call is the parent's record to state.
                    spawned_by: None,
                    interactive: false,
                },
            });
        }
        Some(Statement { at: ts, facts })
    }

    /// The model's id for this file's thread.
    fn owner(&self) -> Option<String> {
        self.thread.as_deref().map(|t| self.node_id(t))
    }

    /// A thread id as the model knows it: the root is `main`.
    fn node_id(&self, thread: &str) -> String {
        if self.root.as_deref() == Some(thread) {
            MAIN_ID.to_string()
        } else {
            thread.to_string()
        }
    }

    /// Facts stated by one line of this thread's own record.
    fn facts(&mut self, owner: &str, line: &Line) -> Vec<Fact> {
        let mut out = Vec::new();
        let by = |kind| Fact {
            agent: Some(owner.to_string()),
            ts: None,
            kind,
        };
        match &line.payload {
            Payload::ResponseItem(item) => {
                match item {
                    ResponseItem::Message(m) => {
                        if m.role.as_deref() == Some("assistant") {
                            let text = m.text();
                            if !text.trim().is_empty() {
                                out.push(by(FactKind::Reasoning(text)));
                            }
                        }
                    }
                    ResponseItem::FunctionCall(fc) => {
                        if let Some(call) = &fc.call_id {
                            let name = fc.name.clone().unwrap_or_default();
                            let summary = summarize_function(&name, fc);
                            out.push(by(FactKind::ToolStart {
                                call: call.clone(),
                                name: name.clone(),
                                summary,
                            }));
                            if is_spawn_tool(&name) {
                                out.push(by(FactKind::Spawn { call: call.clone() }));
                            }
                        }
                    }
                    ResponseItem::FunctionCallOutput { call_id, .. } => {
                        // The format records no error flag on these outputs.
                        if let Some(call) = call_id {
                            out.push(by(FactKind::ToolEnd {
                                call: call.clone(),
                                outcome: Outcome::Ok,
                            }));
                        }
                    }
                    ResponseItem::CustomToolCall(tc) => {
                        if let Some(call) = &tc.call_id {
                            let name = tc.name.clone().unwrap_or_default();
                            let summary = tc.input.as_deref().map(summarize_program);
                            out.push(by(FactKind::ToolStart {
                                call: call.clone(),
                                name,
                                summary,
                            }));
                        }
                    }
                    ResponseItem::CustomToolCallOutput { call_id, output } => {
                        if let Some(call) = call_id {
                            // `exec` writes its verdict on the first line:
                            // `Script completed`, `Script failed`, or a host
                            // error. Anything but the first is a failure.
                            let outcome = match output.head() {
                                Some(head) if head.starts_with("Script completed") => Outcome::Ok,
                                Some(_) => Outcome::Err,
                                None => Outcome::Ok,
                            };
                            out.push(by(FactKind::ToolEnd {
                                call: call.clone(),
                                outcome,
                            }));
                        }
                    }
                    ResponseItem::Reasoning { .. }
                    | ResponseItem::AgentMessage { .. }
                    | ResponseItem::Other => {}
                }
                ensure_activity(&mut out, owner);
            }
            Payload::EventMsg(ev) => {
                match ev {
                    EventMsg::ItemCompleted(ic) => self.item_facts(owner, ic, &mut out),
                    EventMsg::TokenCount { info } => {
                        let total = info
                            .as_ref()
                            .and_then(|i| i.total_token_usage.as_ref())
                            .and_then(|u| u.output_tokens);
                        let last = info
                            .as_ref()
                            .and_then(|i| i.last_token_usage.as_ref())
                            .and_then(|u| u.output_tokens);
                        // Codex re-emits a count with an unchanged total, so
                        // what a record adds is the total's advance, not its
                        // `last` field. A record with no total (older shapes)
                        // is taken as its own delta.
                        let added = match total {
                            Some(t) => {
                                let d = t.saturating_sub(self.output_tokens);
                                self.output_tokens = self.output_tokens.max(t);
                                d
                            }
                            None => last.unwrap_or(0),
                        };
                        if added > 0 {
                            out.push(by(FactKind::Tokens {
                                output: added,
                                dedup: None,
                            }));
                        }
                    }
                    EventMsg::ThreadSettingsApplied { thread_settings } => {
                        if let Some(model) = thread_settings.as_ref().and_then(|s| s.model.clone())
                        {
                            out.push(by(FactKind::Model(model)));
                        }
                    }
                    EventMsg::TaskStarted { .. }
                    | EventMsg::TaskComplete { .. }
                    | EventMsg::TurnAborted
                    | EventMsg::Other => {}
                }
                ensure_activity(&mut out, owner);
            }
            Payload::TurnContext(tc) => {
                if let Some(model) = &tc.model {
                    out.push(by(FactKind::Model(model.clone())));
                }
                ensure_activity(&mut out, owner);
            }
            // `world_state`, inter-agent metadata, usage records: context the
            // runtime wrote around the thread, not the thread speaking.
            Payload::SessionMeta(_) | Payload::Other(_) => {}
        }
        out
    }

    /// Facts stated by one `item_completed`: what ran, with its own timing.
    /// Start and end are stated together, each at its own time, so a
    /// command's duration is right while the record sits on the timeline
    /// where the file wrote it.
    fn item_facts(&self, owner: &str, ic: &wire::ItemCompleted, out: &mut Vec<Fact>) {
        let started = ic.started_at();
        let completed = ic.completed_at();
        let mut ran = |call: &Option<String>, name: String, summary: Option<String>, outcome| {
            let Some(call) = call else { return };
            out.push(Fact {
                agent: Some(owner.to_string()),
                ts: started,
                kind: FactKind::ToolStart {
                    call: call.clone(),
                    name,
                    summary,
                },
            });
            out.push(Fact {
                agent: Some(owner.to_string()),
                ts: completed.or(started),
                kind: FactKind::ToolEnd {
                    call: call.clone(),
                    outcome,
                },
            });
        };
        match &ic.item {
            Item::CommandExecution(c) => {
                let name = c
                    .parsed_cmd
                    .first()
                    .and_then(|p| p.kind.as_deref())
                    .map(command_kind_name)
                    .unwrap_or("shell")
                    .to_string();
                let summary = Some(truncate_summary(&c.display_command()));
                let outcome = match (c.status.as_deref(), c.exit_code) {
                    (Some("failed"), _) => Outcome::Err,
                    (_, Some(code)) if code != 0 => Outcome::Err,
                    _ => Outcome::Ok,
                };
                ran(&c.id, name, summary, outcome);
            }
            Item::Extension(e) => {
                let name = e.kind.clone().unwrap_or_else(|| "extension".into());
                let summary = e.query.as_deref().map(truncate_summary);
                let outcome = match e.status.as_deref() {
                    Some("failed") => Outcome::Err,
                    _ => Outcome::Ok,
                };
                ran(&e.id, name, summary, outcome);
            }
            Item::FileChange(f) => {
                let files: Vec<String> = f
                    .changes
                    .keys()
                    .map(|p| short_path(p, self.cwd.as_deref()))
                    .collect();
                let summary = (!files.is_empty()).then(|| truncate_summary(&files.join(", ")));
                let outcome = match f.status.as_deref() {
                    Some("failed") => Outcome::Err,
                    _ => Outcome::Ok,
                };
                ran(&f.id, "apply_patch".into(), summary, outcome);
            }
            Item::SubAgentActivity(a) => {
                let Some(child) = &a.agent_thread_id else {
                    return;
                };
                let child = self.node_id(child);
                match a.kind.as_deref() {
                    Some("started") => out.push(Fact {
                        agent: Some(child),
                        ts: started,
                        kind: FactKind::Agent {
                            kind: AgentKind::Subagent,
                            parent: Some(owner.to_string()),
                            agent_type: a.agent_path.as_deref().map(|p| path_leaf(p).to_string()),
                            description: None,
                            spawned_by: a.id.clone(),
                            interactive: false,
                        },
                    }),
                    // The one record in the format that observes a child's end.
                    Some("completed") => out.push(Fact {
                        agent: Some(child),
                        ts: completed.or(started),
                        kind: FactKind::Ended(AgentStatus::Done),
                    }),
                    // `interacted` and anything newer: the child is alive, which
                    // its own file already says.
                    _ => {}
                }
            }
            // A child's `UserMessage` would be its spawner's instruction, not a
            // person's prompt; only the root thread's are stated.
            Item::UserMessage { content } if self.thread == self.root => {
                let text: Vec<&str> = content.iter().filter_map(|p| p.text.as_deref()).collect();
                let text = text.join("\n");
                if !text.trim().is_empty() {
                    out.push(Fact {
                        agent: Some(owner.to_string()),
                        ts: started,
                        kind: FactKind::Prompt(text.trim_end().to_string()),
                    });
                }
            }
            Item::Reasoning { summary_text } => {
                let text = summary_text.join("\n");
                if !text.trim().is_empty() {
                    out.push(Fact {
                        agent: Some(owner.to_string()),
                        ts: completed.or(started),
                        kind: FactKind::Reasoning(text),
                    });
                }
            }
            // The request layer states these already, or they say nothing.
            Item::UserMessage { .. }
            | Item::CollabAgentToolCall { .. }
            | Item::AgentMessage
            | Item::Other => {}
        }
    }
}

/// A line is its owner's activity even when it states nothing else.
fn ensure_activity(out: &mut Vec<Fact>, owner: &str) {
    if !out.iter().any(|f| f.agent.as_deref() == Some(owner)) {
        out.push(Fact {
            agent: Some(owner.to_string()),
            ts: None,
            kind: FactKind::Activity,
        });
    }
}

/// Which function tools spawn an agent. `wait_agent` waits and
/// `send_message` talks; neither creates or ends anyone.
pub fn is_spawn_tool(name: &str) -> bool {
    name == "spawn_agent"
}

// ---------------------------------------------------------------------------
// Tool summaries: the Codex tool vocabulary and what makes a good one-liner
// ---------------------------------------------------------------------------

/// A display name for a command, from Codex's own classification of it.
fn command_kind_name(kind: &str) -> &str {
    match kind {
        "read" => "read",
        "search" => "search",
        "list_files" => "list",
        _ => "shell",
    }
}

/// One line for a collaboration or built-in function call.
fn summarize_function(name: &str, fc: &wire::FunctionCall) -> Option<String> {
    match name {
        "spawn_agent" => fc.argument("task_name"),
        "send_message" => fc.argument("recipient").or_else(|| fc.argument("message")),
        _ => None,
    }
    .map(|s| truncate_summary(&s))
}

/// One line for an `exec` program. The usual shape is one `tools.<name>(...)`
/// call: for `exec_command` that is the command itself, for anything else
/// the tool's name and its first string argument. Otherwise the first line.
fn summarize_program(program: &str) -> String {
    if let Some(cmd) = json_string_after(program, "\"cmd\":\"") {
        return truncate_summary(&cmd);
    }
    // A patch program carries the whole diff before the call that applies
    // it; the files it touches are the summary.
    if program.contains("*** Begin Patch") {
        let files: Vec<&str> = program
            .split("*** ")
            .filter_map(|s| {
                s.strip_prefix("Add File: ")
                    .or_else(|| s.strip_prefix("Update File: "))
                    .or_else(|| s.strip_prefix("Delete File: "))
            })
            .filter_map(|rest| rest.split("\\n").next())
            .map(|p| p.rsplit('/').next().unwrap_or(p))
            .collect();
        if !files.is_empty() {
            return truncate_summary(&format!("apply_patch: {}", files.join(", ")));
        }
    }
    let call = program
        .find("tools.")
        .map(|i| &program[i + 6..])
        .and_then(|rest| {
            let name_end = rest.find('(')?;
            let name = &rest[..name_end];
            Some(match first_quoted(&rest[name_end + 1..]) {
                Some(arg) => format!("{name}: {arg}"),
                None => name.to_string(),
            })
        });
    truncate_summary(
        call.as_deref()
            .unwrap_or_else(|| program.lines().next().unwrap_or("")),
    )
}

/// The JSON string value that follows `key` in `text`, with escaped quotes
/// and newlines undone.
fn json_string_after(text: &str, key: &str) -> Option<String> {
    let rest = &text[text.find(key)? + key.len()..];
    let mut end = None;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => {
                end = Some(i);
                break;
            }
            _ => escaped = false,
        }
    }
    end.map(|e| rest[..e].replace("\\\"", "\"").replace("\\n", " "))
}

/// The first double-quoted string in `text`, if any.
fn first_quoted(text: &str) -> Option<String> {
    let open = text.find('"')?;
    json_string_after(&text[open..], "\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every capture under `assets/codex/`, each a real session as Codex wrote
    /// it (trimmed, see `docs/DEMO-ASSETS.md`), discovered the way a live one
    /// is and run through the whole conformance check.
    #[test]
    fn captures_conform() {
        let Some(dir) = crate::provider::harness::fixture_dir("codex") else {
            return;
        };
        let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        fixtures.sort();
        assert!(!fixtures.is_empty(), "no fixtures under {}", dir.display());
        for fixture in fixtures {
            let name = fixture.file_name().unwrap().to_str().unwrap().to_string();
            let rollouts = discovery::all_rollouts(&fixture);
            let root = rollouts
                .iter()
                .find(|p| discovery::read_meta(p).is_some_and(|m| m.is_root()))
                .unwrap_or_else(|| panic!("{name}: no root rollout"))
                .clone();
            let streams = move || -> Vec<Vec<Statement>> {
                discovery::session_rollouts(&root)
                    .into_iter()
                    .map(|(path, _)| {
                        let text = std::fs::read_to_string(&path).unwrap();
                        let mut stream = Stream::new();
                        text.lines().filter_map(|l| stream.push(l)).collect()
                    })
                    .collect()
            };
            crate::provider::harness::conform("codex", &name, streams);
        }
    }

    use std::path::PathBuf;

    #[test]
    fn replayed_parent_history_states_nothing() {
        let mut s = Stream::new();
        let own = s.push(r#"{"timestamp":"2026-08-26T16:13:35.494Z","ordinal":0,"type":"session_meta","payload":{"id":"c","session_id":"a","source":{"subagent":{"thread_spawn":{"parent_thread_id":"a","agent_path":"/root/x"}}},"thread_source":"subagent","subagent_history_start_ordinal":3}}"#).unwrap();
        assert!(matches!(
            &own.facts[0].kind,
            FactKind::Agent { kind: AgentKind::Subagent, parent, .. } if parent.as_deref() == Some("main")
        ));
        // The parent's meta, replayed.
        assert!(s.push(r#"{"timestamp":"2026-08-26T16:10:05.298Z","ordinal":1,"type":"session_meta","payload":{"id":"a","session_id":"a","source":"cli","thread_source":"user"}}"#).is_none());
        // Parent history below the start ordinal.
        assert!(s.push(r#"{"timestamp":"2026-08-26T16:10:17.936Z","ordinal":2,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"parent said"}]}}"#).is_none());
        // The child's own record.
        let st = s.push(r#"{"timestamp":"2026-08-26T16:13:40.000Z","ordinal":3,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"child said"}]}}"#).unwrap();
        assert_eq!(st.facts[0].agent.as_deref(), Some("c"));
        assert!(matches!(st.facts[0].kind, FactKind::Reasoning(ref t) if t == "child said"));
    }

    #[test]
    fn root_meta_states_main_and_session_rows() {
        let mut s = Stream::new();
        let st = s.push(r#"{"timestamp":"2026-08-26T15:30:09.955Z","ordinal":0,"type":"session_meta","payload":{"id":"a","session_id":"a","cwd":"/p","originator":"codex-tui","cli_version":"0.149.1","source":"cli","thread_source":"user"}}"#).unwrap();
        assert!(matches!(
            &st.facts[0].kind,
            FactKind::Agent { kind: AgentKind::Main, interactive: true, agent_type, .. } if agent_type.as_deref() == Some("codex")
        ));
        assert_eq!(st.facts[0].agent.as_deref(), Some("main"));
        assert_eq!(st.facts.iter().filter(|f| f.is_session_meta()).count(), 3);
    }

    #[test]
    fn exec_verdict_and_command_layers() {
        let mut s = Stream::new();
        s.push(r#"{"ordinal":0,"type":"session_meta","payload":{"id":"a","session_id":"a","source":"cli","thread_source":"user"}}"#);
        let start = s.push(r#"{"timestamp":"2026-08-26T16:10:54.748Z","ordinal":1,"type":"response_item","payload":{"type":"custom_tool_call","call_id":"call_1","name":"exec","input":"const r = await tools.exec_command({\"cmd\":\"cargo test\", \"workdir\":\"/p\"});"}}"#).unwrap();
        assert!(matches!(
            &start.facts[0].kind,
            FactKind::ToolStart { call, name, summary } if call == "call_1" && name == "exec" && summary.as_deref() == Some("cargo test")
        ));
        let ran = s.push(r#"{"timestamp":"2026-08-26T16:10:54.934Z","ordinal":2,"type":"event_msg","payload":{"type":"item_completed","thread_id":"a","item":{"type":"CommandExecution","id":"exec-1","command":["/bin/zsh","-lc","cargo test"],"parsed_cmd":[{"type":"unknown","cmd":"cargo test"}],"status":"failed","exit_code":101},"started_at_ms":1787760650000,"completed_at_ms":1787760654000}}"#).unwrap();
        assert!(
            matches!(&ran.facts[0].kind, FactKind::ToolStart { call, name, .. } if call == "exec-1" && name == "shell")
        );
        assert!(matches!(
            &ran.facts[1].kind,
            FactKind::ToolEnd {
                outcome: Outcome::Err,
                ..
            }
        ));
        assert!(ran.facts[0].ts < ran.facts[1].ts, "start before end");
        let end = s.push(r#"{"timestamp":"2026-08-26T16:10:54.947Z","ordinal":3,"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_1","output":[{"type":"input_text","text":"Script completed\nWall time 0.2 seconds\nOutput:\n"}]}}"#).unwrap();
        assert!(
            matches!(&end.facts[0].kind, FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "call_1")
        );
        let host = s.push(r#"{"timestamp":"2026-08-26T16:10:55.947Z","ordinal":4,"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_2","output":[{"type":"input_text","text":"timed out negotiating with the code-mode host"}]}}"#).unwrap();
        assert!(matches!(
            &host.facts[0].kind,
            FactKind::ToolEnd {
                outcome: Outcome::Err,
                ..
            }
        ));
    }

    #[test]
    fn spawn_is_stated_and_wait_is_not_an_ending() {
        let mut s = Stream::new();
        s.push(r#"{"ordinal":0,"type":"session_meta","payload":{"id":"a","session_id":"a","source":"cli","thread_source":"user"}}"#);
        let spawn = s.push(r#"{"timestamp":"2026-08-26T16:13:35.485Z","ordinal":1,"type":"response_item","payload":{"type":"function_call","name":"spawn_agent","arguments":"{\"task_name\":\"explore_theme\"}","call_id":"call_s"}}"#).unwrap();
        assert!(
            spawn
                .facts
                .iter()
                .any(|f| matches!(&f.kind, FactKind::Spawn { call } if call == "call_s"))
        );
        let started = s.push(r#"{"timestamp":"2026-08-26T16:13:35.505Z","ordinal":2,"type":"event_msg","payload":{"type":"item_completed","thread_id":"a","item":{"type":"SubAgentActivity","id":"call_s","kind":"started","agent_thread_id":"c","agent_path":"/root/explore_theme"},"started_at_ms":1787760815505,"completed_at_ms":1787760815505}}"#).unwrap();
        assert!(matches!(
            &started.facts[0].kind,
            FactKind::Agent { spawned_by, agent_type, parent, .. } if spawned_by.as_deref() == Some("call_s") && agent_type.as_deref() == Some("explore_theme") && parent.as_deref() == Some("main")
        ));
        assert_eq!(started.facts[0].agent.as_deref(), Some("c"));
        let wait = s.push(r#"{"timestamp":"2026-08-26T16:13:58.376Z","ordinal":3,"type":"response_item","payload":{"type":"function_call_output","call_id":"call_w","output":"{\"message\":\"Wait completed.\"}"}}"#).unwrap();
        assert!(
            !wait
                .facts
                .iter()
                .any(|f| matches!(f.kind, FactKind::Ended(_)))
        );
        let done = s.push(r#"{"timestamp":"2026-08-26T16:13:58.381Z","ordinal":4,"type":"event_msg","payload":{"type":"item_completed","thread_id":"a","item":{"type":"SubAgentActivity","id":"call_s","kind":"completed","agent_thread_id":"c","agent_path":"/root/explore_theme"},"started_at_ms":1787760838381,"completed_at_ms":1787760838381}}"#).unwrap();
        assert!(matches!(
            &done.facts[0].kind,
            FactKind::Ended(AgentStatus::Done)
        ));
        assert_eq!(done.facts[0].agent.as_deref(), Some("c"));
    }

    #[test]
    fn program_summary_extracts_the_command_or_the_tool() {
        assert_eq!(
            summarize_program(
                r#"const r = await tools.exec_command({"cmd":"sed -n '1,240p' x.md", "workdir":"/p"});"#
            ),
            "sed -n '1,240p' x.md"
        );
        assert_eq!(
            summarize_program(
                r#"const r = await tools.web__run({search_query:[{q:"site:w3.org WAI ARIA switch"}]});"#
            ),
            "web__run: site:w3.org WAI ARIA switch"
        );
        assert_eq!(summarize_program("const x = 1;\nmore"), "const x = 1;");
        assert_eq!(
            summarize_program(
                r#"const patch = "*** Begin Patch\n*** Update File: /p/app.js\n@@\n-a\n+b\n*** Add File: /p/theme.js\n+x\n*** End Patch"; await tools.apply_patch({patch});"#
            ),
            "apply_patch: app.js, theme.js"
        );
    }
}
