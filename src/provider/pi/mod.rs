//! The omp/pi provider: [`wire::Entry`] in, [`Fact`]s out — one wire format,
//! two providers. `Provider::Omp` and `Provider::Pi` share this module
//! because their records are identical (see `wire.rs`); only where the files
//! live and how `project_key` is spelled differ (`discovery.rs`).
//!
//! What this module knows that the model must not:
//!
//! - A `message` user turn is a human prompt only when `UserMessage::is_human`
//!   (`attribution` absent or `"user"`) — `"agent"` (measured: 1479 of ~75k
//!   `message` records on this box) is the framework re-entering its own
//!   thread as a `user` turn, never a person; folding it as a `Prompt` would
//!   put words in the user's mouth. `steering: true` is still human text (a
//!   mid-turn interjection, always measured paired with `attribution:
//!   "user"`), folded the same as any other prompt. Only the interactive
//!   root's prompts are stated, matching Claude: they are the session's
//!   spine, not a spawned child's.
//! - A `custom_message` (`mid-run-todo-nudge`, `async-result`) is injected
//!   text delivered as a turn but authored by nobody: never a `Prompt`,
//!   whatever its `customType`. It still marks activity — it did happen to
//!   this agent's thread.
//! - `tool_execution_start` duplicates the `toolCall` block that requested it
//!   (same `toolCallId`, confirmed against real sessions: the two always
//!   pair 1:1). It is not a second `ToolStart`, only activity.
//! - `session_exit` is a real end marker, but only for a spawned child: the
//!   interactive root never turns it into `Ended` — an interactive agent's
//!   completion is unclaimable (ARCHITECTURE.md §2.1/§4; the same reason
//!   Claude never emits `meta.stoppedByUser`). `kind: "normal"` is `Done`,
//!   `"signal"` (sigterm/sighup, measured) is `Stopped`, anything else
//!   (`"fatal"`, measured as an unhandled rejection) is `Failed`.
//! - `model_change` only updates this agent's running model when `role` is
//!   absent or `"default"` — `"fallback"`/`"temporary"` (both measured)
//!   describe a one-off override of a different model slot, not a change to
//!   what this agent is running.
//!
//! **omp's subagent fan-out.** A `task` toolCall (`arguments.tasks[]`, a
//! batch: it can name several children in one call) writes each child as a
//! full sibling transcript, `<name>.jsonl`, beside the root file — not to
//! `~/.omp/agent/history.db` as first assumed; that store is unrelated
//! (measured: 212 sibling transcripts across 177 real projects on this box).
//! The join between the spawning call and the child it named is by NAME, not
//! id — the format shares no id between the two sides (unlike Claude's
//! `meta.json`, which names its parent's `tool_use_id` directly). So:
//!
//! - a child's own stream states its `Agent` fact from `session_init`, keyed
//!   by its file stem, with `agent_type` from `session_init.agent` and
//!   `spawned_by: None` (it cannot name the call that made it);
//! - the root's stream, on a `task` toolCall, states an ADDITIONAL `Agent`
//!   fact for each `tasks[].name`, keyed the same way, with `spawned_by:
//!   Some(call)` — [`FactKind::Agent`] is idempotent and fills only empty
//!   fields, so the two merge into one node with the name from the wire and
//!   the type and the spawning call attached.
//!
//! A name reused within the same root stream (measured: happens — a second
//! fan-out round reusing a name from the first) cannot be told apart from the
//! root side, since the wire only ever records the requested name, never the
//! `-2` the runtime appends on collision: the root does not re-register a
//! name it has already claimed once. The second child still becomes a real
//! agent from its own file (under its real, suffixed stem), just without
//! `spawned_by` — declared, not guessed, per `docs/ARCHITECTURE.md` §0. A
//! child whose stem never appears in any `task` call (an auto-named spawn, or
//! an internal sidecar like `__advisor` that carries no `session_init` at
//! all) is still a real agent: it appears from its own file's activity alone,
//! with a generic `parent: main` (every child observed on this box sits
//! directly under the root's own directory; a hypothetical nested spawn is
//! out of scope — see `docs/omp-wire.md`).
//!
//! **pi has no subagents.** `@earendil-works/pi-coding-agent` 0.84.4's
//! `dist/core` and `dist/cli` contain zero references to a `subagent`
//! concept (checked 2026-09-08), and zero sibling transcripts were found
//! under any pi session directory on this box. So a pi session is exactly
//! one file — a fact about the agent, not a gap in this measurement.
//!
//! Out of scope, and why: `<n>.bash.log`/`<n>.bash-original.log` beside a
//! session directory are raw shell command output, not transcripts
//! (confirmed: no `type`/JSONL shape). `<Name>.md` beside a spawned child is
//! a written report artifact, also not a transcript. Neither is a
//! `SessionFile` (`discovery::classify_path` only ever matches `.jsonl`).

use std::collections::HashSet;

use chrono::{DateTime, Utc};

pub mod discovery;
pub mod wire;

use crate::fact::{AgentKind, AgentStatus, Fact, FactKind, Outcome, Statement};
use crate::provider::summary::truncate_summary;
use crate::state::session::MAIN_ID;
use wire::{ContentBlock, Entry, MessageBody, UserMessage, is_spawn_tool, parse_line};

/// One session file being read: the interactive root, or a spawned child's
/// own transcript (omp only — see `super`). The only cross-line state this
/// format needs: an inherited timestamp (like Claude, since a `custom`
/// record's own `timestamp` field can be absent), whether this file's own
/// `Agent` fact has been stated yet, and — root only — which spawned names
/// this stream has already claimed.
#[derive(Debug, Clone)]
pub struct Stream {
    /// This file's own node id: `main` for the root, its file stem for a
    /// child — the only name either side of a spawn agrees on (see `super`).
    owner: String,
    interactive: bool,
    /// Root only: `"omp"` or `"pi"`, this build's label for who wrote it.
    agent_type: Option<&'static str>,
    announced: bool,
    last_ts: Option<DateTime<Utc>>,
    /// Root only: task-spawned child names already registered in this
    /// stream, so a same-session name reuse does not misattribute a fresh
    /// spawn's provenance to the agent that first claimed the name.
    seen_spawn_names: HashSet<String>,
}

impl Stream {
    pub fn new_root(agent_type: &'static str) -> Self {
        Stream {
            owner: MAIN_ID.to_string(),
            interactive: true,
            agent_type: Some(agent_type),
            announced: false,
            last_ts: None,
            seen_spawn_names: HashSet::new(),
        }
    }

    pub fn new_child(name: String) -> Self {
        Stream {
            owner: name,
            interactive: false,
            agent_type: None,
            announced: false,
            last_ts: None,
            seen_spawn_names: HashSet::new(),
        }
    }

    /// Parse one line and state what it says. `None` for a blank or
    /// unparsable line, or one that states nothing.
    pub fn push(&mut self, line: &str) -> Option<Statement> {
        let entry = parse_line(line)?;
        self.push_entry(&entry)
    }

    /// State what an already-parsed entry says.
    fn push_entry(&mut self, entry: &Entry) -> Option<Statement> {
        let at = entry_timestamp(entry).or(self.last_ts);
        if at.is_some() {
            self.last_ts = at;
        }
        let mut out = self.facts(entry);
        if out.is_empty() {
            return None;
        }
        for f in &mut out {
            if f.ts.is_none() {
                f.ts = at;
            }
        }
        // This file's own agent, stated once, on the first line that is
        // activity rather than session metadata (mirrors the Claude root:
        // neither format has a dedicated "I am born" record for the root,
        // and a child's own richer announcement from `session_init` — below
        // — merges into whichever of the two fires first).
        if !self.announced && !out.iter().all(Fact::is_session_meta) {
            self.announced = true;
            out.insert(0, self.birth_fact(at));
        }
        Some(Statement { at, facts: out })
    }

    fn birth_fact(&self, at: Option<DateTime<Utc>>) -> Fact {
        Fact {
            agent: Some(self.owner.clone()),
            ts: at,
            kind: FactKind::Agent {
                kind: if self.interactive {
                    AgentKind::Main
                } else {
                    AgentKind::Subagent
                },
                // Every child observed on this box sits directly under the
                // root's own directory — see `super`.
                parent: (!self.interactive).then(|| MAIN_ID.to_string()),
                agent_type: self.agent_type.map(str::to_string),
                description: None,
                spawned_by: None,
                interactive: self.interactive,
            },
        }
    }

    /// Facts stated by one record of this file's own thread.
    fn facts(&mut self, entry: &Entry) -> Vec<Fact> {
        let mut out = Vec::new();
        let owner = self.owner.clone();
        match entry {
            Entry::Title(t) => {
                if let Some(title) = non_empty(&t.title) {
                    out.push(meta(FactKind::Title(title)));
                }
            }
            // Session-wide info: only meaningful once, from the interactive
            // root — a spawned child's own internal session record is a
            // format artefact (its `cwd` can differ from the root's), not
            // something worth surfacing in the session-wide header.
            Entry::Session(s) if self.interactive => {
                if let Some(cwd) = non_empty(&s.cwd) {
                    out.push(session_row("cwd", cwd));
                }
                if let Some(title) = non_empty(&s.title) {
                    out.push(meta(FactKind::Title(title)));
                }
                if let Some(src) = non_empty(&s.title_source) {
                    out.push(session_row("title source", src));
                }
            }
            Entry::Session(_) => {}
            // A child's own root record (never observed on a root stream —
            // gated defensively so a malformed file cannot make "main" its
            // own parent).
            Entry::SessionInit(si) if !self.interactive => {
                out.push(Fact {
                    agent: Some(owner.clone()),
                    ts: None,
                    kind: FactKind::Agent {
                        kind: AgentKind::Subagent,
                        parent: Some(MAIN_ID.to_string()),
                        agent_type: si.agent.clone(),
                        description: None,
                        spawned_by: None,
                        interactive: false,
                    },
                });
            }
            Entry::SessionInit(_) => {}
            Entry::Message(m) => self.message_facts(m, &mut out),
            Entry::Custom(c) => self.custom_facts(c, &mut out),
            Entry::CustomMessage(_) => ensure_activity(&mut out, &owner),
            Entry::ModelChange(mc) => {
                // Absent or "default" is this agent's own running model;
                // "fallback"/"temporary" (measured) are a one-off override
                // of a different slot — see `super`.
                if mc.role.as_deref().is_none_or(|r| r == "default")
                    && let Some(model) = mc.model()
                {
                    out.push(Fact {
                        agent: Some(owner.clone()),
                        ts: None,
                        kind: FactKind::Model(model.to_string()),
                    });
                }
                if mc.resolved_model_is_fallback == Some(true) {
                    out.push(meta(FactKind::Tally("model fallback".into())));
                }
                ensure_activity(&mut out, &owner);
            }
            Entry::ThinkingLevelChange(tl) => {
                if let Some(level) = non_empty(&tl.thinking_level) {
                    out.push(session_row("thinking level", level));
                }
                ensure_activity(&mut out, &owner);
            }
            Entry::TitleChange(tc) => {
                if let Some(title) = non_empty(&tc.title) {
                    out.push(meta(FactKind::Title(title)));
                }
            }
            Entry::CredentialPin(cp) => {
                if let Some(provider) = non_empty(&cp.provider) {
                    out.push(session_row("credential", provider));
                }
            }
            Entry::Unknown => {}
        }
        out
    }

    fn message_facts(&mut self, m: &wire::MessageEntry, out: &mut Vec<Fact>) {
        let owner = self.owner.clone();
        let Some(body) = &m.message else { return };
        match body {
            MessageBody::User(u) => {
                // Only the interactive root's prompts are the session's
                // spine (matches Claude); a spawned child's first "user"
                // turn is its task assignment, not a person speaking, and is
                // excluded anyway by `is_human` (measured: always
                // `attribution: "agent"`).
                if self.interactive
                    && u.is_human()
                    && let Some(text) = user_text(u)
                {
                    out.push(Fact {
                        agent: Some(owner.clone()),
                        ts: None,
                        kind: FactKind::Prompt(text),
                    });
                }
                ensure_activity(out, &owner);
            }
            MessageBody::Assistant(a) => {
                if let Some(model) = &a.model {
                    out.push(Fact {
                        agent: Some(owner.clone()),
                        ts: None,
                        kind: FactKind::Model(model.clone()),
                    });
                }
                if let Some(usage) = &a.usage
                    && let Some(output) = usage.output
                {
                    out.push(Fact {
                        agent: Some(owner.clone()),
                        ts: None,
                        kind: FactKind::Tokens {
                            output,
                            dedup: a.response_id.clone(),
                        },
                    });
                }
                // Blocks in order: a spawn carries the text nearest above it
                // as its stated reason, which the fold reads off the last
                // Reasoning (same as Claude/Codex).
                for block in &a.content {
                    match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            out.push(Fact {
                                agent: Some(owner.clone()),
                                ts: None,
                                kind: FactKind::Reasoning(text.clone()),
                            });
                        }
                        ContentBlock::Thinking { thinking } if !thinking.trim().is_empty() => {
                            out.push(Fact {
                                agent: Some(owner.clone()),
                                ts: None,
                                kind: FactKind::Reasoning(thinking.clone()),
                            });
                        }
                        ContentBlock::ToolCall(tc) => {
                            let Some(call) = &tc.id else { continue };
                            let name = tc.name.clone().unwrap_or_default();
                            // `intent` is the agent's own one-line reason for
                            // the call — used directly, never reconstructed
                            // from `arguments` (see `super`).
                            let summary = tc
                                .intent
                                .as_deref()
                                .map(truncate_summary)
                                .filter(|s| !s.is_empty());
                            out.push(Fact {
                                agent: Some(owner.clone()),
                                ts: None,
                                kind: FactKind::ToolStart {
                                    call: call.clone(),
                                    name: name.clone(),
                                    summary,
                                },
                            });
                            if is_spawn_tool(&name) {
                                out.push(Fact {
                                    agent: Some(owner.clone()),
                                    ts: None,
                                    kind: FactKind::Spawn { call: call.clone() },
                                });
                                self.spawn_facts(call, &tc.arguments, out);
                            }
                        }
                        _ => {}
                    }
                }
                ensure_activity(out, &owner);
            }
            MessageBody::ToolResult(r) => {
                if let Some(call) = &r.tool_call_id {
                    let outcome = if r.is_error == Some(true) {
                        Outcome::Err
                    } else {
                        Outcome::Ok
                    };
                    out.push(Fact {
                        agent: Some(owner.clone()),
                        ts: None,
                        kind: FactKind::ToolEnd {
                            call: call.clone(),
                            outcome,
                        },
                    });
                }
                ensure_activity(out, &owner);
            }
            MessageBody::Unknown => {}
        }
    }

    /// The additional `Agent` fact a `task` toolCall states for each child
    /// it names, keyed by the SAME file-stem name the child's own stream
    /// uses — see `super` for why this is a stated join, not a guess, and
    /// for the collision rule `seen_spawn_names` enforces.
    fn spawn_facts(&mut self, call: &str, arguments: &serde_json::Value, out: &mut Vec<Fact>) {
        let Ok(parsed) = serde_json::from_value::<wire::TaskArguments>(arguments.clone()) else {
            return;
        };
        for item in parsed.tasks {
            let Some(name) = non_empty(&item.name) else {
                continue;
            };
            if !self.seen_spawn_names.insert(name.clone()) {
                continue;
            }
            out.push(Fact {
                agent: Some(name),
                ts: None,
                kind: FactKind::Agent {
                    kind: AgentKind::Subagent,
                    parent: Some(self.owner.clone()),
                    agent_type: None,
                    description: None,
                    spawned_by: Some(call.to_string()),
                    interactive: false,
                },
            });
        }
    }

    fn custom_facts(&mut self, c: &wire::CustomEntry, out: &mut Vec<Fact>) {
        let owner = self.owner.clone();
        match c.custom_type.as_deref() {
            // Duplicates the toolCall that requested it — see `super`.
            Some("tool_execution_start") => {}
            Some("session_exit") => {
                // An interactive root's completion is unclaimable — see
                // `super`. Only a spawned child's own exit is ground truth.
                if !self.interactive
                    && let Ok(exit) = serde_json::from_value::<wire::SessionExit>(c.data.clone())
                    && let Some(status) = exit_status(exit.kind.as_deref())
                {
                    out.push(Fact {
                        agent: Some(owner.clone()),
                        ts: None,
                        kind: FactKind::Ended(status),
                    });
                }
            }
            _ => {}
        }
        ensure_activity(out, &owner);
    }
}

/// `session_exit.kind` to a lifecycle status: `"normal"` is a clean finish,
/// `"signal"` (sigterm/sighup, measured) is cut short externally rather than
/// finished cleanly, anything else (`"fatal"`, measured as an unhandled
/// rejection) is a crash. `None` claims nothing.
fn exit_status(kind: Option<&str>) -> Option<AgentStatus> {
    match kind? {
        "normal" => Some(AgentStatus::Done),
        "signal" => Some(AgentStatus::Stopped),
        _ => Some(AgentStatus::Failed),
    }
}

/// The plain text of a user turn's `text` blocks, joined. `None` when there
/// is none (an all-non-text turn, or an empty one).
fn user_text(u: &UserMessage) -> Option<String> {
    let parts: Vec<&str> = u
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    non_empty(&Some(parts.join("\n")))
}

/// A record's own time, for entries whose only timestamp field is not named
/// `timestamp` (the `title` header) or that carry none at all (`Unknown`).
fn entry_timestamp(entry: &Entry) -> Option<DateTime<Utc>> {
    match entry {
        Entry::Title(t) => t.updated_at,
        Entry::Session(s) => s.timestamp,
        Entry::SessionInit(si) => si.timestamp,
        Entry::Message(m) => m.timestamp,
        Entry::Custom(c) => c.timestamp,
        Entry::CustomMessage(cm) => cm.timestamp,
        Entry::ModelChange(mc) => mc.timestamp,
        Entry::ThinkingLevelChange(tl) => tl.timestamp,
        Entry::TitleChange(tc) => tc.timestamp,
        Entry::CredentialPin(cp) => cp.timestamp,
        Entry::Unknown => None,
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

/// Untimed session metadata, off the timeline.
fn meta(kind: FactKind) -> Fact {
    Fact {
        agent: None,
        ts: None,
        kind,
    }
}

fn session_row(label: &str, value: String) -> Fact {
    meta(FactKind::Session {
        label: label.to_string(),
        value,
    })
}

/// A wire string field, trimmed of the "present but blank" case the format
/// writes for an unset title (`{"title": ""}` on a fresh session).
fn non_empty(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_all(stream: &mut Stream, lines: &[&str]) -> Vec<Statement> {
        lines.iter().filter_map(|l| stream.push(l)).collect()
    }

    /// The shipped fixture, discovered the way a live omp session is: a
    /// root plus two spawned children found through `related_paths`, run
    /// through the whole conformance check. Proves the name-join and the
    /// `-name`-shaped sibling discrimination together, over real discovery,
    /// not just the in-memory `Stream` the unit tests above drive directly.
    #[test]
    fn omp_demo_conforms() {
        use crate::provider::Provider;
        let Some(dir) = crate::provider::harness::fixture_dir("omp") else {
            return;
        };
        let root_path =
            dir.join("2026-01-01T00-00-00-000Z_00000000-0000-7000-0000-000000000001.jsonl");
        let streams = move || -> Vec<Vec<Statement>> {
            let root_file = discovery::session_file(Provider::Omp, &root_path).unwrap();
            discovery::related_paths(&root_file)
                .into_iter()
                .map(|path| {
                    let file = discovery::session_file(Provider::Omp, &path).unwrap();
                    let text = std::fs::read_to_string(&path).unwrap();
                    let mut stream = discovery::stream_for(&file);
                    text.lines().filter_map(|l| stream.push(l)).collect()
                })
                .collect()
        };
        crate::provider::harness::conform("omp", "demo", streams);
    }

    /// The shipped pi fixture: one file, no children — pi has none to find
    /// (see the module doc above).
    #[test]
    fn pi_demo_conforms() {
        use crate::provider::Provider;
        let Some(dir) = crate::provider::harness::fixture_dir("pi") else {
            return;
        };
        let root_path =
            dir.join("2026-01-01T00-00-00-000Z_00000000-0000-7c00-0000-000000000002.jsonl");
        let streams = move || -> Vec<Vec<Statement>> {
            let file = discovery::session_file(Provider::Pi, &root_path).unwrap();
            let text = std::fs::read_to_string(&root_path).unwrap();
            let mut stream = discovery::stream_for(&file);
            vec![text.lines().filter_map(|l| stream.push(l)).collect()]
        };
        crate::provider::harness::conform("pi", "demo", streams);
    }

    #[test]
    fn root_announces_main_as_interactive_with_its_provider_label() {
        let mut s = Stream::new_root("omp");
        let stmts = push_all(
            &mut s,
            &[r#"{"type":"title","title":"","updatedAt":"2026-09-01T00:00:00Z","pad":""}"#,
              r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-09-01T00:00:01Z","cwd":"/tmp"}"#,
              r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-09-01T00:00:02Z","message":{"role":"user","content":[{"type":"text","text":"hello"}],"attribution":"user"}}"#],
        );
        let facts: Vec<&Fact> = stmts.iter().flat_map(|s| &s.facts).collect();
        let agent = facts
            .iter()
            .find(|f| matches!(f.kind, FactKind::Agent { .. }))
            .expect("main announced");
        assert_eq!(agent.agent.as_deref(), Some(MAIN_ID));
        match &agent.kind {
            FactKind::Agent {
                kind,
                interactive,
                agent_type,
                ..
            } => {
                assert_eq!(*kind, AgentKind::Main);
                assert!(*interactive);
                assert_eq!(agent_type.as_deref(), Some("omp"));
            }
            _ => unreachable!(),
        }
        assert!(
            facts
                .iter()
                .any(|f| f.kind == FactKind::Prompt("hello".into()))
        );
    }

    #[test]
    fn agent_attributed_user_turn_is_never_a_prompt() {
        let mut s = Stream::new_root("omp");
        let stmts = push_all(
            &mut s,
            &[
                r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-09-01T00:00:00Z","message":{"role":"user","content":[{"type":"text","text":"synthetic nudge"}],"attribution":"agent"}}"#,
            ],
        );
        let facts: Vec<&Fact> = stmts.iter().flat_map(|s| &s.facts).collect();
        assert!(!facts.iter().any(|f| matches!(f.kind, FactKind::Prompt(_))));
        // Still activity: the thread did produce a line.
        assert!(facts.iter().any(|f| f.kind == FactKind::Activity));
    }

    #[test]
    fn custom_message_is_never_a_prompt_either() {
        let mut s = Stream::new_root("omp");
        let stmts = push_all(
            &mut s,
            &[r#"{"type":"custom_message","id":"a","parentId":null,"timestamp":"2026-09-01T00:00:00Z","customType":"async-result","content":"result text","attribution":"agent"}"#],
        );
        let facts: Vec<&Fact> = stmts.iter().flat_map(|s| &s.facts).collect();
        assert!(!facts.iter().any(|f| matches!(f.kind, FactKind::Prompt(_))));
        assert!(facts.iter().any(|f| f.kind == FactKind::Activity));
    }

    #[test]
    fn tool_call_pairs_with_its_result_and_uses_intent_as_summary() {
        let mut s = Stream::new_root("omp");
        let stmts = push_all(
            &mut s,
            &[
                r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-09-01T00:00:00Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"t1","name":"bash","arguments":{"command":"ls"},"intent":"list files"}]}}"#,
                r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-09-01T00:00:01Z","message":{"role":"toolResult","toolCallId":"t1","toolName":"bash","content":[{"type":"text","text":"out"}],"isError":false}}"#,
            ],
        );
        let facts: Vec<&Fact> = stmts.iter().flat_map(|s| &s.facts).collect();
        assert!(facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::ToolStart { call, summary, .. }
                if call == "t1" && summary.as_deref() == Some("list files")
        )));
        assert!(facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "t1"
        )));
    }

    #[test]
    fn task_spawn_registers_a_named_child_and_a_repeat_name_is_not_reclaimed() {
        let mut s = Stream::new_root("omp");
        let call = r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-09-01T00:00:00Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call1","name":"task","arguments":{"tasks":[{"name":"TickReviewer"}]},"intent":"fan out"}]}}"#;
        let stmts = push_all(&mut s, &[call]);
        let facts: Vec<&Fact> = stmts.iter().flat_map(|s| &s.facts).collect();
        assert!(facts.iter().any(|f| f.agent.as_deref() == Some("TickReviewer")
            && matches!(&f.kind, FactKind::Agent{spawned_by: Some(c), parent, ..} if c == "call1" && parent.as_deref() == Some(MAIN_ID))));
        assert!(
            facts
                .iter()
                .any(|f| matches!(&f.kind, FactKind::Spawn { call } if call == "call1"))
        );

        // A second round reusing the same name must not re-register it —
        // the wire cannot tell the provider whether the runtime wrote
        // `TickReviewer.jsonl` again or `TickReviewer-2.jsonl`.
        let call2 = r#"{"type":"message","id":"c","parentId":"a","timestamp":"2026-09-01T00:01:00Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call2","name":"task","arguments":{"tasks":[{"name":"TickReviewer"}]},"intent":"fan out again"}]}}"#;
        let stmts2 = push_all(&mut s, &[call2]);
        let facts2: Vec<&Fact> = stmts2.iter().flat_map(|s| &s.facts).collect();
        assert!(!facts2.iter().any(|f| matches!(f.kind, FactKind::Agent { .. })));
        // The Spawn fact itself is still stated — it is call-scoped, not
        // name-scoped, and always true regardless of the collision.
        assert!(
            facts2
                .iter()
                .any(|f| matches!(&f.kind, FactKind::Spawn { call } if call == "call2"))
        );
    }

    #[test]
    fn child_stream_states_itself_from_session_init_and_ends_from_session_exit() {
        let mut s = Stream::new_child("TickReviewer".to_string());
        let stmts = push_all(
            &mut s,
            &[
                r#"{"type":"session_init","id":"i","parentId":null,"timestamp":"2026-09-01T00:00:00Z","agent":"reviewer","resolvedModel":"anthropic/claude-opus-5","task":"look for bugs","systemPrompt":"...","readOnly":false,"spawns":"","tools":6}"#,
                r#"{"type":"custom","id":"e","parentId":"i","timestamp":"2026-09-01T00:05:00Z","customType":"session_exit","data":{"reason":"dispose","kind":"normal","recordedAt":"2026-09-01T00:05:00Z"}}"#,
            ],
        );
        let facts: Vec<&Fact> = stmts.iter().flat_map(|s| &s.facts).collect();
        let agent_facts: Vec<&Fact> = facts
            .iter()
            .filter(|f| matches!(f.kind, FactKind::Agent { .. }) && f.agent.as_deref() == Some("TickReviewer"))
            .copied()
            .collect();
        // Two Agent facts are stated for this one child — the generic birth
        // (structural: any child sits under `main`) and `session_init`'s own
        // richer one (its `agent_type`). `SessionModel::apply_fact` merges
        // them by filling only empty fields; this asserts the union either
        // one alone would leave incomplete.
        assert_eq!(agent_facts.len(), 2, "birth + session_init enrichment");
        assert!(agent_facts.iter().all(|f| matches!(&f.kind,
            FactKind::Agent { kind: AgentKind::Subagent, parent: Some(p), interactive: false, .. }
                if p == MAIN_ID
        )));
        assert!(agent_facts.iter().any(|f| matches!(&f.kind,
            FactKind::Agent { agent_type: Some(t), .. } if t == "reviewer"
        )));
        assert!(facts.iter().any(|f| f.agent.as_deref() == Some("TickReviewer")
            && f.kind == FactKind::Ended(AgentStatus::Done)));
    }

    #[test]
    fn child_session_exit_by_signal_is_stopped_not_done() {
        let mut s = Stream::new_child("Orphan".to_string());
        let stmts = push_all(
            &mut s,
            &[r#"{"type":"custom","id":"e","parentId":null,"timestamp":"2026-09-01T00:05:00Z","customType":"session_exit","data":{"reason":"sigterm","kind":"signal","recordedAt":"2026-09-01T00:05:00Z"}}"#],
        );
        let facts: Vec<&Fact> = stmts.iter().flat_map(|s| &s.facts).collect();
        assert!(facts.iter().any(|f| f.kind == FactKind::Ended(AgentStatus::Stopped)));
    }

    #[test]
    fn interactive_root_never_claims_completion_from_session_exit() {
        let mut s = Stream::new_root("omp");
        let stmts = push_all(
            &mut s,
            &[r#"{"type":"custom","id":"e","parentId":null,"timestamp":"2026-09-01T00:05:00Z","customType":"session_exit","data":{"reason":"dispose","kind":"normal","recordedAt":"2026-09-01T00:05:00Z"}}"#],
        );
        let facts: Vec<&Fact> = stmts.iter().flat_map(|s| &s.facts).collect();
        assert!(!facts.iter().any(|f| matches!(f.kind, FactKind::Ended(_))));
    }

    #[test]
    fn model_change_role_gates_which_slot_updates_the_agents_model() {
        let mut s = Stream::new_root("omp");
        let stmts = push_all(
            &mut s,
            &[
                r#"{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-09-01T00:00:00Z","model":"anthropic/claude-opus-5","resolvedModelIsFallback":false}"#,
                r#"{"type":"model_change","id":"m2","parentId":"m1","timestamp":"2026-09-01T00:00:01Z","model":"anthropic/claude-sonnet-5","role":"temporary","resolvedModelIsFallback":false}"#,
            ],
        );
        let facts: Vec<&Fact> = stmts.iter().flat_map(|s| &s.facts).collect();
        let models: Vec<&str> = facts
            .iter()
            .filter_map(|f| match &f.kind {
                FactKind::Model(m) => Some(m.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(models, vec!["anthropic/claude-opus-5"]);
    }

    #[test]
    fn pi_model_change_reads_model_id_field() {
        let mut s = Stream::new_root("pi");
        let stmts = push_all(
            &mut s,
            &[r#"{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-09-01T00:00:00Z","provider":"anthropic","modelId":"claude-opus-5"}"#],
        );
        let facts: Vec<&Fact> = stmts.iter().flat_map(|s| &s.facts).collect();
        assert!(facts.iter().any(|f| f.kind == FactKind::Model("claude-opus-5".into())));
    }
}
