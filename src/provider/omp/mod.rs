//! The OMP (oh-my-pi) / Pi provider: session JSONL lines in, [`Fact`]s out.
//!
//! One file is one thread. The root is `<project>/<timestamp>_<uuid>.jsonl`;
//! every spawned agent has its own file under `<timestamp>_<uuid>/` named by
//! the agent (`Name.jsonl`, or `Parent.Name.jsonl` one level down), whose
//! header names the spawner in `parentSession`. So, like Claude, the *file*
//! decides the owner: `main` for the root, the file stem for a child. The
//! spawner states the same ids from its `task` result (`details.progress[]`),
//! which is what joins the spawning call to the child.
//!
//! What lives here and nowhere else: the first record is often a `title`, not
//! the header; a `task` result is a spawn acknowledgement and never an
//! ending; the one ending the format records is a delivered `async-result`
//! naming the job and its status. Silence is not completion.
//!
//! Branches: the session is a tree (`id`/`parentId`); the current branch is
//! the parent chain from the last entry. [`Stream::push_lines`] states only
//! that chain, and raises [`Stream::take_rebuild`] when appends leave
//! already-stated history, so the feeder re-reads the file afresh.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::Value;

pub mod discovery;
pub mod wire;

use crate::fact::{AgentKind, AgentStatus, Fact, FactKind, Outcome, Statement};
use crate::provider::summary::{short_path, truncate_summary};
use crate::state::session::MAIN_ID;
use wire::{Record, parse_line, task_results};

/// One OMP/Pi session file being read.
#[derive(Debug, Clone)]
pub struct Stream {
    /// `main` for a root file, else the child's file stem.
    owner: String,
    cwd: Option<String>,
    /// Entry id → parent id, every tree entry seen. The session is a tree
    /// whose current branch is the parent chain from the last entry.
    parents: HashMap<String, Option<String>>,
    /// Last tree entry seen: the head.
    tip: Option<String>,
    /// Entries already stated. A new head whose chain misses one of these
    /// means the user branched away from stated history.
    seen: HashSet<String>,
    rebuild: bool,
    /// Child ids each tree entry's `task` result states (`progress[].id`,
    /// `async.jobId`), for every entry read, current branch or not.
    spawns: HashMap<String, Vec<String>>,
    /// The current branch: the parent chain from the tip, as of the last batch.
    chain: HashSet<String>,
    /// Stated history left the current branch as of the last batch.
    stale: bool,
}

/// The tree id of an entry: every record but the header and title line.
fn tree_id(r: &Record) -> Option<&str> {
    match r.kind() {
        "session" | "title" => None,
        _ => r.str("id"),
    }
}

/// Full child ids a `task` result states: `progress[].id`, plus `async.jobId`.
fn spawned_ids(msg: &Value) -> Vec<String> {
    let details = msg.get("details");
    let mut ids: Vec<String> = details
        .and_then(|d| d.get("progress"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    if let Some(job) = details
        .and_then(|d| d.get("async"))
        .filter(|a| a.get("type").and_then(Value::as_str) == Some("task"))
        .and_then(|a| a.get("jobId"))
        .and_then(Value::as_str)
        && !ids.iter().any(|i| i == job)
    {
        ids.push(job.to_string());
    }
    ids
}

/// Whether the thread named `node` (a file stem) was spawned on a discarded
/// branch, or nested below an agent that was.
pub fn is_pruned(node: &str, discarded: &HashSet<String>) -> bool {
    discarded.iter().any(|d| {
        node == d
            || node
                .strip_prefix(d.as_str())
                .is_some_and(|r| r.starts_with('.'))
    })
}

impl Stream {
    pub fn new(owner: Option<String>) -> Self {
        Stream {
            owner: owner.unwrap_or_else(|| MAIN_ID.to_string()),
            cwd: None,
            parents: HashMap::new(),
            tip: None,
            seen: HashSet::new(),
            rebuild: false,
            spawns: HashMap::new(),
            chain: HashSet::new(),
            stale: false,
        }
    }

    /// Parse one line and state what it says, ignoring branches. Feeders use
    /// [`push_lines`](Self::push_lines).
    pub fn push(&mut self, line: &str) -> Option<Statement> {
        let rec = parse_line(line)?;
        self.state(&rec)
    }

    /// Whether the last batch showed that already-stated history left the
    /// current branch (the user branched to an earlier entry). The caller
    /// must re-read the whole file through a fresh stream; the flag clears.
    pub fn take_rebuild(&mut self) -> bool {
        std::mem::take(&mut self.rebuild)
    }

    /// Spawn ids this file's `task` results state, split by branch:
    /// `(active, discarded)`. Active ids sit on the current branch; discarded
    /// ones were stated only on a branch since abandoned. The one extraction
    /// every feeder reuses (same ids the facts use).
    pub fn spawn_sets(&self) -> (HashSet<String>, HashSet<String>) {
        let mut active = HashSet::new();
        let mut off = HashSet::new();
        for (entry, ids) in &self.spawns {
            let set = if self.chain.contains(entry) {
                &mut active
            } else {
                &mut off
            };
            set.extend(ids.iter().cloned());
        }
        off.retain(|i| !active.contains(i));
        (active, off)
    }

    /// State a batch of lines, only those on the current branch: the parent
    /// chain from the last tree entry. A whole file read in one batch never
    /// states a discarded branch; appends that branch away from stated
    /// history raise [`take_rebuild`](Self::take_rebuild) instead.
    pub fn push_lines<'a>(&mut self, lines: impl IntoIterator<Item = &'a str>) -> Vec<Statement> {
        let recs: Vec<Record> = lines.into_iter().filter_map(parse_line).collect();
        // Nothing new and history still on the branch: O(1), no effect.
        if recs.is_empty() && !self.stale {
            return Vec::new();
        }
        let linear = self.extends_tip(&recs);
        for r in &recs {
            if let Some(id) = tree_id(r) {
                self.parents
                    .insert(id.to_string(), r.str("parentId").map(str::to_owned));
                self.tip = Some(id.to_string());
                if linear {
                    self.chain.insert(id.to_string());
                }
                if let Some(msg) = r.message().filter(|m| {
                    r.role() == Some("toolResult")
                        && m.get("toolName").and_then(Value::as_str) == Some("task")
                }) {
                    let ids = spawned_ids(msg);
                    if !ids.is_empty() {
                        self.spawns.insert(id.to_string(), ids);
                    }
                }
            }
        }
        // A proven linear append keeps `seen ⊆ chain` and cannot form a cycle;
        // anything else (divergence, duplicate id, missing parent, cycle, first
        // batch) walks the parent chain afresh.
        if self.stale || (!linear && recs.iter().any(|r| tree_id(r).is_some())) {
            let mut chain: HashSet<String> = HashSet::new();
            let mut at = self.tip.clone();
            while let Some(id) = at {
                if !chain.insert(id.clone()) {
                    break;
                }
                at = self.parents.get(&id).cloned().flatten();
            }
            self.chain = chain;
            self.stale = self.seen.iter().any(|id| !self.chain.contains(id));
            if self.stale {
                self.rebuild = true;
                return Vec::new();
            }
        }
        let chain = std::mem::take(&mut self.chain);
        let mut out = Vec::new();
        for r in &recs {
            if let Some(id) = tree_id(r) {
                if !chain.contains(id) {
                    continue;
                }
                self.seen.insert(id.to_string());
            }
            out.extend(self.state(r));
        }
        self.chain = chain;
        out
    }

    /// Whether the batch's tree entries are a proven linear append to the tip:
    /// first parent is the tip, each next parent is the previous entry, and
    /// no id was seen before (or repeats in the batch). Then the new chain is
    /// the old one plus the batch.
    fn extends_tip(&self, recs: &[Record]) -> bool {
        let Some(mut prev) = self.tip.as_deref().filter(|_| !self.stale) else {
            return false;
        };
        let mut ids: HashSet<&str> = HashSet::new();
        for r in recs {
            let Some(id) = tree_id(r) else { continue };
            if r.str("parentId") != Some(prev) || self.parents.contains_key(id) || !ids.insert(id) {
                return false;
            }
            prev = id;
        }
        true
    }

    fn state(&mut self, rec: &Record) -> Option<Statement> {
        let at = rec.time();
        let mut facts = self.facts(rec, at);
        if facts.is_empty() {
            return None;
        }
        for f in &mut facts {
            if f.ts.is_none() && !f.is_session_meta() {
                f.ts = at;
            }
        }
        Some(Statement { at, facts })
    }

    fn by(&self, ts: Option<DateTime<Utc>>, kind: FactKind) -> Fact {
        Fact {
            agent: Some(self.owner.clone()),
            ts,
            kind,
        }
    }

    fn session(label: &str, value: &str) -> Fact {
        Fact {
            agent: None,
            ts: None,
            kind: FactKind::Session {
                label: label.into(),
                value: value.into(),
            },
        }
    }

    fn facts(&mut self, rec: &Record, at: Option<DateTime<Utc>>) -> Vec<Fact> {
        let mut out = Vec::new();
        match rec.kind() {
            "title" | "title_change" => {
                if let Some(t) = rec.str("title").map(str::trim).filter(|t| !t.is_empty())
                    && self.owner == MAIN_ID
                {
                    out.push(Fact {
                        agent: None,
                        ts: None,
                        kind: FactKind::Title(t.to_string()),
                    });
                }
            }
            "session" => {
                self.cwd = rec.str("cwd").map(str::to_owned);
                if self.owner == MAIN_ID {
                    out.push(self.by(
                        at,
                        FactKind::Agent {
                            kind: AgentKind::Main,
                            parent: None,
                            agent_type: Some("omp".into()),
                            description: None,
                            spawned_by: None,
                            interactive: true,
                        },
                    ));
                    out.push(Self::session("app", "omp"));
                    if let Some(c) = &self.cwd {
                        out.push(Self::session("cwd", c));
                    }
                    if let Some(v) = rec.v.get("version").and_then(Value::as_u64) {
                        out.push(Self::session("version", &v.to_string()));
                    }
                } else {
                    let parent = rec
                        .str("parentSession")
                        .map(discovery::node_of_path)
                        .or_else(|| self.owner.rsplit_once('.').map(|(p, _)| p.to_string()))
                        .unwrap_or_else(|| MAIN_ID.to_string());
                    // agent_type comes from the spawner's progress[].agent, not the file name.
                    out.push(self.by(
                        at,
                        FactKind::Agent {
                            kind: AgentKind::Subagent,
                            parent: Some(parent),
                            agent_type: None,
                            description: None,
                            spawned_by: None,
                            interactive: false,
                        },
                    ));
                }
            }
            // `model_change` is not stated: it can pin a model no reply used;
            // assistant messages carry the model that actually generated.
            "custom_message" if rec.str("customType") == Some("async-result") => {
                let text = rec.str("content").unwrap_or("");
                for (job, status) in task_results(text) {
                    let ended = if status == "completed" {
                        AgentStatus::Done
                    } else if ["cancelled", "canceled", "aborted", "stopped"]
                        .iter()
                        .any(|s| status.starts_with(s))
                    {
                        AgentStatus::Stopped
                    } else if status.contains("failed") || status.starts_with("error") {
                        // Native: `failed`, `failed (exit N)`, `merge failed`.
                        AgentStatus::Failed
                    } else {
                        continue;
                    };
                    out.push(Fact {
                        agent: Some(job),
                        ts: at,
                        kind: FactKind::Ended(ended),
                    });
                }
                self.activity(&mut out);
            }
            "message" => {
                self.message(rec, at, &mut out);
                self.activity(&mut out);
            }
            _ => {}
        }
        out
    }

    fn activity(&self, out: &mut Vec<Fact>) {
        if !out.iter().any(|f| f.agent.as_deref() == Some(&self.owner)) {
            out.push(self.by(None, FactKind::Activity));
        }
    }

    fn message(&self, rec: &Record, at: Option<DateTime<Utc>>, out: &mut Vec<Fact>) {
        match rec.role() {
            Some("user") => {
                // A child's first user message is its spawner's assignment,
                // and agent-attributed text is injected, not typed.
                let typed = rec
                    .message()
                    .and_then(|m| m.get("attribution"))
                    .and_then(Value::as_str)
                    .is_none_or(|a| a == "user");
                let text = rec.plain_text();
                if self.owner == MAIN_ID && typed && !text.trim().is_empty() {
                    out.push(self.by(at, FactKind::Prompt(text.trim_end().to_string())));
                }
            }
            Some("assistant") => {
                if let Some(m) = rec
                    .message()
                    .and_then(|m| m.get("model"))
                    .and_then(Value::as_str)
                {
                    out.push(self.by(at, FactKind::Model(m.to_string())));
                }
                if let Some(n) = rec.output_tokens().filter(|n| *n > 0) {
                    let dedup = rec
                        .message()
                        .and_then(|m| m.get("responseId"))
                        .and_then(Value::as_str)
                        .or_else(|| rec.str("id"))
                        .map(str::to_owned);
                    out.push(self.by(at, FactKind::Tokens { output: n, dedup }));
                }
                for part in rec.parts() {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") | Some("thinking") => {
                            let t = part
                                .get("text")
                                .or_else(|| part.get("thinking"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            if !t.trim().is_empty() {
                                out.push(self.by(at, FactKind::Reasoning(t.to_string())));
                            }
                        }
                        Some("toolCall") => {
                            let Some(call) = part.get("id").and_then(Value::as_str) else {
                                continue;
                            };
                            let name = part
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            out.push(self.by(
                                at,
                                FactKind::ToolStart {
                                    call: call.to_string(),
                                    name: name.clone(),
                                    summary: self.summarize(part),
                                },
                            ));
                            if name == "task" {
                                out.push(self.by(
                                    at,
                                    FactKind::Spawn {
                                        call: call.to_string(),
                                    },
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("toolResult") => {
                let Some(msg) = rec.message() else { return };
                let Some(call) = msg.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                let outcome = if msg.get("isError").and_then(Value::as_bool) == Some(true) {
                    Outcome::Err
                } else {
                    Outcome::Ok
                };
                // Children first: the model joins end and children by call.
                if msg.get("toolName").and_then(Value::as_str) == Some("task") {
                    let progress = msg
                        .get("details")
                        .and_then(|d| d.get("progress"))
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    for p in progress {
                        let Some(id) = p.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        // Native ids are already fully prefixed (`Parent.Child`).
                        out.push(Fact {
                            agent: Some(id.to_string()),
                            ts: at,
                            kind: FactKind::Agent {
                                kind: AgentKind::Subagent,
                                parent: Some(self.owner.clone()),
                                agent_type: p
                                    .get("agent")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                                description: ["assignment", "task"]
                                    .iter()
                                    .filter_map(|k| p.get(*k).and_then(Value::as_str))
                                    .map(str::trim)
                                    .find(|s| !s.is_empty())
                                    .map(truncate_summary),
                                spawned_by: Some(call.to_string()),
                                interactive: false,
                            },
                        });
                    }
                    // `async.jobId` is the same full id; state it when progress lacks it.
                    if let Some(job) = msg
                        .get("details")
                        .and_then(|d| d.get("async"))
                        .filter(|a| a.get("type").and_then(Value::as_str) == Some("task"))
                        .and_then(|a| a.get("jobId"))
                        .and_then(Value::as_str)
                        .filter(|j| {
                            !progress
                                .iter()
                                .any(|p| p.get("id").and_then(Value::as_str) == Some(j))
                        })
                    {
                        out.push(Fact {
                            agent: Some(job.to_string()),
                            ts: at,
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
                out.push(self.by(
                    at,
                    FactKind::ToolEnd {
                        call: call.to_string(),
                        outcome,
                    },
                ));
            }
            _ => {}
        }
    }

    /// One line for a tool call: its most telling argument, else its intent.
    fn summarize(&self, part: &Value) -> Option<String> {
        let args = part.get("arguments");
        for key in ["path", "command", "cmd", "pattern", "query", "url"] {
            if let Some(s) = args.and_then(|a| a.get(key)).and_then(Value::as_str) {
                return Some(if key == "path" {
                    short_path(s, self.cwd.as_deref())
                } else {
                    truncate_summary(s)
                });
            }
        }
        part.get("intent")
            .and_then(Value::as_str)
            .map(truncate_summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(owner: Option<&str>, lines: &[&str]) -> Vec<Fact> {
        let mut s = Stream::new(owner.map(str::to_owned));
        lines
            .iter()
            .filter_map(|l| s.push(l))
            .flat_map(|s| s.facts)
            .collect()
    }

    const ROOT: &[&str] = &[
        r#"{"type":"title","v":1,"title":"T","source":"auto","updatedAt":"x","pad":"  "}"#,
        r#"{"type":"session","version":3,"id":"u","timestamp":"2026-10-11T01:42:10.041Z","cwd":"/p"}"#,
        r#"this is not json"#,
        r#"{"type":"message","id":"a","timestamp":"2026-10-11T01:42:11.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
        r#"{"type":"message","id":"b","timestamp":"2026-10-11T01:42:12.000Z","message":{"role":"assistant","model":"m","usage":{"output":7},"responseId":"r1","content":[{"type":"toolCall","id":"c1","name":"task","arguments":{},"intent":"go"},{"type":"toolCall","id":"c2","name":"read","arguments":{"path":"/p/a.rs"}}]}}"#,
        r#"{"type":"message","id":"c","timestamp":"2026-10-11T01:42:13.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"task","isError":false,"content":[],"details":{"progress":[{"id":"A","agent":"scout"},{"id":"B","agent":"scout"}]}}}"#,
        r#"{"type":"message","id":"d","timestamp":"2026-10-11T01:42:14.000Z","message":{"role":"toolResult","toolCallId":"c2","toolName":"read","isError":true,"content":[]}}"#,
        r#"{"type":"custom_message","customType":"async-result","timestamp":"2026-10-11T01:43:00.000Z","content":"<task-result id=\"A\" agent=\"scout\" status=\"completed\"> <task-result id=\"B\" status=\"failed\">"}"#,
    ];

    fn count(f: &[Fact], pred: impl Fn(&FactKind) -> bool) -> usize {
        f.iter().filter(|f| pred(&f.kind)).count()
    }

    #[test]
    fn root_session_states_tools_children_and_endings() {
        let f = run(None, ROOT);
        assert!(
            f.iter()
                .any(|f| matches!(&f.kind, FactKind::Title(t) if t == "T"))
        );
        assert_eq!(count(&f, |k| matches!(k, FactKind::ToolStart { .. })), 2);
        assert_eq!(count(&f, |k| matches!(k, FactKind::Spawn { .. })), 1);
        assert_eq!(count(&f, |k| matches!(k, FactKind::Prompt(_))), 1);
        let kids: Vec<_> = f
            .iter()
            .filter(|f| matches!(&f.kind, FactKind::Agent { kind: AgentKind::Subagent, spawned_by: Some(c), .. } if c == "c1"))
            .map(|f| f.agent.clone().unwrap())
            .collect();
        assert_eq!(kids, ["A", "B"]);
        assert!(f.iter().any(
            |f| f.agent.as_deref() == Some("A") && f.kind == FactKind::Ended(AgentStatus::Done)
        ));
        assert!(
            f.iter().any(|f| f.agent.as_deref() == Some("B")
                && f.kind == FactKind::Ended(AgentStatus::Failed))
        );
        // The read errored; the task ack carries no ending.
        assert!(f.iter().any(
            |f| matches!(&f.kind, FactKind::ToolEnd { call, outcome: Outcome::Err } if call == "c2")
        ));
        assert!(f.iter().any(
            |f| matches!(&f.kind, FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "c1")
        ));
    }

    #[test]
    fn child_file_names_its_parent_and_never_ends_itself() {
        let f = run(
            Some("A"),
            &[
                r#"{"type":"title","v":1,"title":"","updatedAt":"x"}"#,
                r#"{"type":"session","version":3,"id":"k","timestamp":"2026-10-11T01:47:18.944Z","cwd":"/p","parentSession":"/h/-p/2026-10-11T01-42-10-041Z_01a1289f-e579-722c-bfe1-cded8884534d.jsonl"}"#,
            ],
        );
        assert!(matches!(
            &f[0].kind,
            FactKind::Agent { parent: Some(p), kind: AgentKind::Subagent, .. } if p == "main"
        ));
        assert_eq!(count(&f, |k| matches!(k, FactKind::Ended(_))), 0);
    }

    #[test]
    fn nested_progress_id_is_used_verbatim_with_assignment() {
        let f = run(
            Some("A"),
            &[
                r#"{"type":"message","timestamp":"2026-10-11T01:42:13.000Z","message":{"role":"toolResult","toolCallId":"x","toolName":"task","content":[],"details":{"progress":[{"id":"A.C","agent":"task","assignment":"fix the bug"}]}}}"#,
                r#"{"type":"custom_message","customType":"async-result","timestamp":"2026-10-11T01:43:00.000Z","content":"<task-result id=\"A.C\" status=\"failed (exit 1)\"> <task-result id=\"A.D\" status=\"merge failed\">"}"#,
            ],
        );
        assert!(f.iter().any(|f| f.agent.as_deref() == Some("A.C")
            && matches!(&f.kind, FactKind::Agent { description: Some(d), .. } if d == "fix the bug")));
        assert!(!f.iter().any(|f| f.agent.as_deref() == Some("A.A.C")));
        for id in ["A.C", "A.D"] {
            assert!(f.iter().any(|f| f.agent.as_deref() == Some(id)
                && f.kind == FactKind::Ended(AgentStatus::Failed)));
        }
    }

    #[test]
    fn async_job_id_states_child_and_header_keeps_spawned_type() {
        let f = run(
            Some("A"),
            &[
                r#"{"type":"message","timestamp":"2026-10-11T01:42:13.000Z","message":{"role":"toolResult","toolCallId":"x","toolName":"task","content":[],"details":{"progress":[],"async":{"state":"running","jobId":"A.C","type":"task"}}}}"#,
                r#"{"type":"session","version":3,"id":"k","timestamp":"2026-10-11T01:47:18.944Z","cwd":"/p"}"#,
            ],
        );
        assert!(f.iter().any(|f| f.agent.as_deref() == Some("A.C")
            && matches!(&f.kind, FactKind::Agent { spawned_by: Some(c), .. } if c == "x")));
        assert!(!f.iter().any(|f| f.agent.as_deref() == Some("A.A.C")));
        assert!(f.iter().any(|f| f.agent.as_deref() == Some("A")
            && matches!(
                &f.kind,
                FactKind::Agent {
                    agent_type: None,
                    ..
                }
            )));
    }

    #[test]
    fn older_nested_header_derives_parent_and_model_change_is_ignored() {
        let f = run(
            Some("A.C"),
            &[
                r#"{"type":"model_change","model":"terra"}"#,
                r#"{"type":"session","version":3,"id":"k","timestamp":"2026-10-11T01:47:18.944Z","cwd":"/p"}"#,
                r#"{"type":"message","timestamp":"2026-10-11T01:47:19.000Z","message":{"role":"assistant","provider":"openai","model":"sol","content":[]}}"#,
            ],
        );
        assert!(matches!(
            &f[0].kind,
            FactKind::Agent { parent: Some(p), .. } if p == "A"
        ));
        let models: Vec<_> = f
            .iter()
            .filter_map(|f| match &f.kind {
                FactKind::Model(m) => Some(m.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(models, ["sol"]);
    }

    fn msg(id: &str, parent: &str, text: &str) -> String {
        let p = if parent.is_empty() {
            "null".into()
        } else {
            format!("\"{parent}\"")
        };
        format!(
            r#"{{"type":"message","id":"{id}","parentId":{p},"timestamp":"2026-10-11T01:42:11.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    fn prompts(s: &[Statement]) -> Vec<String> {
        s.iter()
            .flat_map(|s| s.facts.iter())
            .filter_map(|f| match &f.kind {
                FactKind::Prompt(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn whole_file_states_only_the_current_branch() {
        let lines = [
            msg("a", "", "one"),
            msg("b", "a", "dead"),
            msg("c", "a", "two"),
        ];
        let mut s = Stream::new(None);
        let out = s.push_lines(lines.iter().map(String::as_str));
        assert_eq!(prompts(&out), ["one", "two"]);
        assert!(!s.take_rebuild());
    }

    #[test]
    fn append_that_leaves_stated_history_asks_for_a_rebuild() {
        let mut s = Stream::new(None);
        let first = [msg("a", "", "one"), msg("b", "a", "dead")];
        assert_eq!(
            prompts(&s.push_lines(first.iter().map(String::as_str))).len(),
            2
        );
        let more = [msg("c", "a", "two")];
        assert!(s.push_lines(more.iter().map(String::as_str)).is_empty());
        assert!(s.take_rebuild());
        // A fresh stream over the whole file drops the dead branch.
        let all: Vec<String> = first.iter().chain(more.iter()).cloned().collect();
        let mut fresh = Stream::new(None);
        assert_eq!(
            prompts(&fresh.push_lines(all.iter().map(String::as_str))),
            ["one", "two"]
        );
    }

    fn push(s: &mut Stream, lines: &[String]) -> Vec<Statement> {
        s.push_lines(lines.iter().map(String::as_str))
    }

    #[test]
    fn linear_multi_entry_appends_extend_incrementally() {
        let mut s = Stream::new(None);
        assert_eq!(prompts(&push(&mut s, &[msg("a", "", "one")])), ["one"]);
        let out = push(&mut s, &[msg("b", "a", "two"), msg("c", "b", "three")]);
        assert_eq!(prompts(&out), ["two", "three"]);
        assert!(!s.take_rebuild());
        assert_eq!(s.chain.len(), 3);
        assert_eq!(prompts(&push(&mut s, &[msg("d", "c", "four")])), ["four"]);
        assert!(!s.take_rebuild());
    }

    #[test]
    fn empty_batch_has_no_effect() {
        let mut s = Stream::new(None);
        push(&mut s, &[msg("a", "", "one")]);
        assert!(push(&mut s, &[]).is_empty());
        assert!(!s.take_rebuild());
        assert_eq!(s.chain.len(), 1);
    }

    #[test]
    fn divergence_rebuilds_and_returning_to_stated_tip_recovers() {
        let mut s = Stream::new(None);
        push(&mut s, &[msg("a", "", "one"), msg("b", "a", "dead")]);
        assert!(push(&mut s, &[msg("c", "a", "two")]).is_empty());
        assert!(s.take_rebuild());
        let out = push(&mut s, &[msg("d", "b", "back")]);
        assert_eq!(prompts(&out), ["back"]);
        assert!(!s.take_rebuild());
    }

    #[test]
    fn duplicate_id_falls_back_to_full_walk() {
        let mut s = Stream::new(None);
        push(&mut s, &[msg("a", "", "one"), msg("b", "a", "two")]);
        // Duplicate "b": not a proven append, full walk keeps history on-branch.
        let out = push(&mut s, &[msg("b", "a", "again")]);
        assert!(!s.take_rebuild());
        assert_eq!(prompts(&out), ["again"]);
        assert_eq!(s.chain.len(), 2);
    }

    #[test]
    fn cycle_terminates_and_states_both() {
        let mut s = Stream::new(None);
        let out = push(&mut s, &[msg("x", "y", "one"), msg("y", "x", "two")]);
        assert!(!s.take_rebuild());
        assert_eq!(prompts(&out), ["one", "two"]);
        assert_eq!(s.chain.len(), 2);
    }

    #[test]
    fn missing_parent_asks_for_rebuild() {
        let mut s = Stream::new(None);
        push(&mut s, &[msg("a", "", "one")]);
        assert!(push(&mut s, &[msg("z", "ghost", "two")]).is_empty());
        assert!(s.take_rebuild());
    }

    #[test]
    fn header_only_batch_still_states_meta() {
        let mut s = Stream::new(None);
        push(&mut s, &[msg("a", "", "one")]);
        let hdr = r#"{"type":"session","version":3,"id":"k","timestamp":"2026-10-11T01:47:18.944Z","cwd":"/p"}"#;
        let out = push(&mut s, &[hdr.to_string()]);
        assert!(!out.is_empty());
        assert!(!s.take_rebuild());
        assert_eq!(s.chain.len(), 1);
    }
}
