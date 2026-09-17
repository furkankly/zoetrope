//! The Bob provider: rows of Bob's database in, [`Fact`]s out.
//!
//! IBM Bob keeps a session as rows, not files: a `tasks` row per session and
//! per child it spawned, and a `messages` row per turn of a transcript. The
//! row feeder (`db.rs`, native) and `zoe export` present those rows as JSON
//! lines (`wire.rs`), one session per stream, root task first; this stream
//! reads them the same way whether they came from the database or a file.
//!
//! What this module knows that the model must not: that a child's row is
//! born at `created_at` and ended by its `status` at `updated_at`, and that
//! a `subagent` keeps no turns of its own while a `subtask` does; that
//! `spawn_subagent` and `start_subtask` spawn, and that the join from a
//! call to its child is the child's id on the call's result
//! (`_meta.subagentId`), with the child's `title`, which is the call's
//! prompt, as the fallback for a result that lacks it (the k-th call with
//! a prompt to the k-th child carrying it); that a turn's time is `_meta.timestamp` inside
//! its blob and never the row's `created_at`, which Bob rewrites; that a
//! `tool` row carries its verdict as `isError`, and a spawn's carries the
//! child's whole transcript under `messages`, the only record of what the
//! child did, written when the child returned, whose token spend the
//! child's row already totals (the feeder lifts those turns out as the
//! child's own lines, marked `via` the call; a file that still nests them
//! is read the same); that a turn the harness marks `hide` shows
//! its text to nobody but still made and answered its calls, and one it
//! marks `notAi` is narration in the model's voice, not the model; that a
//! `/skill` command is stored as the
//! skill's whole text, with what the person typed kept in `_meta.mask`.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

pub mod discovery;
pub mod wire;

use crate::fact::{AgentKind, AgentStatus, Fact, FactKind, Outcome, Statement};
use crate::provider::summary::{short_path, truncate_summary};
use crate::state::session::MAIN_ID;
use wire::{Data, MessageRow, Record, Seen, TaskRow, ToolCall, parse_line};

/// One session's rows being read.
#[derive(Debug, Clone, Default)]
pub struct Stream {
    /// The session's root task, from its row. Lines before it state nothing.
    root: Option<String>,
    /// The workspace, for relativising paths in summaries.
    cwd: Option<String>,
    /// Spawning calls seen, keyed by the prompt the child will carry as its
    /// title, in order of appearance: the fallback join.
    spawns: HashMap<String, Vec<String>>,
    /// What each spawning call asked for, by call id.
    calls: HashMap<String, Spawned>,
    /// Children seen, keyed by title, in order.
    children: HashMap<String, Vec<Child>>,
    /// When each child was born, by id, for the join stated from a result.
    born: HashMap<String, Option<DateTime<Utc>>>,
    /// Children already joined to their spawning call.
    paired: HashSet<String>,
    /// Results that named no child and whose k-th child by title has not
    /// been read yet, by `(prompt, k)`.
    pending: HashMap<(String, usize), Returned>,
    /// Cumulative output tokens last seen per child, so each observation of
    /// its row states only what it added.
    child_tokens: HashMap<String, u64>,
}

/// A child task as its row first stated it.
#[derive(Debug, Clone)]
struct Child {
    id: String,
}

/// A spawning call as its turn stated it.
#[derive(Debug, Clone)]
struct Spawned {
    prompt: String,
    agent_type: Option<String>,
}

/// A spawn's result, held for the child it will be joined to.
#[derive(Debug, Clone)]
struct Returned {
    call: String,
    agent_type: Option<String>,
    turns: Vec<Data>,
}

impl Stream {
    pub fn new() -> Self {
        Stream::default()
    }

    /// Parse one line and state what it says. `None` for a blank or
    /// unparsable line, a line before the root's row, or one that states
    /// nothing.
    pub fn push(&mut self, line: &str) -> Option<Statement> {
        match parse_line(line)? {
            Record::Task { seen, row } => self.task(seen, &row),
            Record::Message(row) => self.message(&row),
        }
    }

    /// The model's id for a task: the root is `main`.
    fn node_id(&self, task: &str) -> String {
        if self.root.as_deref() == Some(task) {
            MAIN_ID.to_string()
        } else {
            task.to_string()
        }
    }

    fn task(&mut self, seen: Seen, row: &TaskRow) -> Option<Statement> {
        if row.is_root() {
            if self.root.is_none() {
                self.root = Some(row.id.clone());
                self.cwd = row.workspace();
            }
            if self.root.as_deref() != Some(row.id.as_str()) {
                return None;
            }
            return match seen {
                Seen::Created => Some(self.root_created(row)),
                Seen::Updated => self.root_updated(row),
            };
        }
        // A child names its parent by id; only a known root maps to `main`.
        self.root.as_ref()?;
        match seen {
            Seen::Created => Some(self.child_created(row)),
            Seen::Updated => Some(self.child_updated(row)),
        }
    }

    /// The session itself: the root agent, its title, and what the row says
    /// about where and how it ran.
    fn root_created(&self, row: &TaskRow) -> Statement {
        let ts = row.created();
        let mut facts = vec![Fact {
            agent: Some(MAIN_ID.to_string()),
            ts,
            kind: FactKind::Agent {
                kind: AgentKind::Main,
                parent: None,
                agent_type: Some("bob".into()),
                description: None,
                spawned_by: None,
                interactive: true,
            },
        }];
        if let Some(title) = row.headline() {
            facts.push(meta(FactKind::Title(title.to_string())));
        }
        let session = |label: &str, value: Option<String>| {
            value.map(|v| {
                meta(FactKind::Session {
                    label: label.into(),
                    value: v,
                })
            })
        };
        facts.extend(session("workspace", row.workspace()));
        facts.extend(session(
            "mode",
            row.env.as_ref().and_then(|e| e.mode_id.clone()),
        ));
        facts.extend(session("version", row.version.clone()));
        facts.extend(session("status", row.status.clone()));
        facts.extend(session("cost", cost_of(row)));
        facts.extend(session("error", error_of(row)));
        Statement { at: ts, facts }
    }

    /// A later look at the session's row: the running cost, the status,
    /// the error it stopped on if any, and the title, which the IDE fills
    /// in only once the task has run a while. The root is interactive, so
    /// its status is never an ending: a task that stopped on an error can
    /// be resumed.
    fn root_updated(&self, row: &TaskRow) -> Option<Statement> {
        let mut facts = Vec::new();
        if let Some(title) = row.headline() {
            facts.push(meta(FactKind::Title(title.to_string())));
        }
        let session = |label: &str, value: Option<String>| {
            value.map(|v| {
                meta(FactKind::Session {
                    label: label.into(),
                    value: v,
                })
            })
        };
        facts.extend(session("status", row.status.clone()));
        facts.extend(session("cost", cost_of(row)));
        facts.extend(session("error", error_of(row)));
        (!facts.is_empty()).then_some(Statement { at: None, facts })
    }

    /// A child's row as first seen: its birth. Its spawning call is joined
    /// from the call's result, which names the child; a result read before
    /// this row, and naming no child, waits here by title.
    fn child_created(&mut self, row: &TaskRow) -> Statement {
        let ts = row.created();
        let title = row.title.clone().unwrap_or_default();
        let siblings = self.children.entry(title.clone()).or_default();
        let k = siblings.len();
        siblings.push(Child { id: row.id.clone() });
        self.born.insert(row.id.clone(), ts);
        let returned = self.pending.remove(&(title, k));
        let (spawned_by, agent_type) = match &returned {
            Some(r) => {
                self.paired.insert(row.id.clone());
                (Some(r.call.clone()), r.agent_type.clone())
            }
            None => (None, None),
        };
        let parent = row
            .parent_id
            .as_deref()
            .map(|p| self.node_id(p))
            .unwrap_or_else(|| MAIN_ID.to_string());
        let mut facts = vec![Fact {
            agent: Some(row.id.clone()),
            ts,
            kind: FactKind::Agent {
                kind: AgentKind::Subagent,
                parent: Some(parent),
                agent_type,
                description: row.headline().map(truncate_summary),
                spawned_by,
                interactive: false,
            },
        }];
        if let Some(r) = returned {
            for turn in &r.turns {
                self.embedded_turn(&row.id, turn, &mut facts);
            }
        }
        Statement { at: ts, facts }
    }

    /// A child's row after a change: it was active up to `updated_at`, spent
    /// what its costs grew by, and if its status is terminal, ended then.
    fn child_updated(&mut self, row: &TaskRow) -> Statement {
        let ts = row.updated();
        let by = |kind| Fact {
            agent: Some(row.id.clone()),
            ts,
            kind,
        };
        let mut facts = vec![by(FactKind::Activity)];
        if let Some(total) = row.costs.as_ref().and_then(|c| c.output) {
            let last = self.child_tokens.entry(row.id.clone()).or_insert(0);
            let added = total.saturating_sub(*last);
            *last = (*last).max(total);
            // Keyed by the total observed, so the same observation stated
            // twice adds once.
            if added > 0 {
                facts.push(by(FactKind::Tokens {
                    output: added,
                    dedup: Some(format!("{}@{total}", row.id)),
                }));
            }
        }
        if let Some(status) = row.status.as_deref() {
            match ended_status(status) {
                Some(ended) => facts.push(by(FactKind::Ended(ended))),
                // Still going, or a status this provider has not seen:
                // the latter is counted so it shows rather than silently
                // leaving the child running.
                None if matches!(status, "active" | "running") => {}
                None => facts.push(meta(FactKind::Tally(format!("child status {status}")))),
            }
        }
        Statement { at: ts, facts }
    }

    fn message(&mut self, row: &MessageRow) -> Option<Statement> {
        self.root.as_ref()?;
        let owner = self.node_id(&row.task_id);
        let data = row.data.as_ref()?;
        let role = row.role()?;
        // The system prompt is the harness, not the agent.
        if role == "system" {
            return None;
        }
        let ts = row.timestamp();
        let mut facts = Vec::new();
        // A turn lifted out of a spawn's result is the child's, with its
        // tokens already in the child's row.
        self.turn_facts(
            &owner,
            role,
            data,
            ts,
            &row.id,
            row.via.is_none(),
            &mut facts,
        );
        // A turn is its owner's activity even when it states nothing else.
        if !facts.iter().any(|f| f.agent.as_deref() == Some(&owner)) {
            facts.push(Fact {
                agent: Some(owner.clone()),
                ts,
                kind: FactKind::Activity,
            });
        }
        Some(Statement { at: ts, facts })
    }

    /// What one turn by `owner` states, whether it is a row of its own or
    /// embedded in a spawn's result. `id` is the turn's own id, the key
    /// under which its tokens are counted once; `tokens` is whether they
    /// are counted at all (a child's come from its row instead). A turn the
    /// harness marks `hide` shows its text to nobody, but the calls it makes
    /// and answers happened all the same.
    #[allow(clippy::too_many_arguments)]
    fn turn_facts(
        &mut self,
        owner: &str,
        role: &str,
        data: &Data,
        ts: Option<DateTime<Utc>>,
        id: &str,
        tokens: bool,
        facts: &mut Vec<Fact>,
    ) {
        let by = |kind| Fact {
            agent: Some(owner.to_string()),
            ts,
            kind,
        };
        let spoken = !data.hidden() && !data.not_ai();
        match role {
            "user" => {
                // A child's user turn is its spawner's instruction, not a
                // person's prompt: only the root's are prompts.
                let text = data.prompt_text();
                if owner == MAIN_ID && spoken && !text.trim().is_empty() {
                    facts.push(by(FactKind::Prompt(text.trim_end().to_string())));
                }
            }
            "assistant" => {
                let text = data.text();
                if spoken && !text.trim().is_empty() {
                    facts.push(by(FactKind::Reasoning(text)));
                }
                if tokens
                    && let Some(output) = data.output_tokens()
                    && output > 0
                {
                    facts.push(by(FactKind::Tokens {
                        output,
                        dedup: Some(id.to_string()),
                    }));
                }
                for call in &data.tool_calls {
                    let Some(call_id) = &call.id else { continue };
                    let name = call.name.clone().unwrap_or_default();
                    facts.push(by(FactKind::ToolStart {
                        call: call_id.clone(),
                        name: name.clone(),
                        summary: summarize(&name, call, self.cwd.as_deref()),
                    }));
                    if let Some(prompt) = spawn_prompt(&name, call) {
                        facts.push(by(FactKind::Spawn {
                            call: call_id.clone(),
                        }));
                        self.spawns
                            .entry(prompt.clone())
                            .or_default()
                            .push(call_id.clone());
                        self.calls.insert(
                            call_id.clone(),
                            Spawned {
                                prompt,
                                agent_type: spawn_type(&name, call),
                            },
                        );
                    }
                }
            }
            "tool" => {
                if let Some(call) = data.call_id() {
                    let outcome = match data.is_error() {
                        Some(true) => Outcome::Err,
                        _ => Outcome::Ok,
                    };
                    facts.push(by(FactKind::ToolEnd {
                        call: call.to_string(),
                        outcome,
                    }));
                    if let Some(spawned) = self.calls.get(call).cloned() {
                        self.spawn_result(owner, call, &spawned, data, facts);
                    }
                }
            }
            _ => {}
        }
    }

    /// A spawn's result: the join to its child, and the child's transcript.
    /// The result names the child (`_meta.subagentId`), which is the join.
    /// A result that does not (an older Bob) is joined by prompt: the k-th
    /// call with a prompt to the k-th child carrying it as a title. A
    /// result that errored spawned nobody and joins nothing.
    fn spawn_result(
        &mut self,
        owner: &str,
        call: &str,
        spawned: &Spawned,
        data: &Data,
        facts: &mut Vec<Fact>,
    ) {
        let agent_type = data
            .meta
            .as_ref()
            .and_then(|m| m.agent_type.clone())
            .or_else(|| spawned.agent_type.clone());
        let child = match data.meta.as_ref().and_then(|m| m.subagent_id.clone()) {
            Some(id) => id,
            None => {
                if data.is_error() == Some(true) {
                    // Spawned nobody: it no longer counts among the calls
                    // the children of this prompt are matched to.
                    if let Some(calls) = self.spawns.get_mut(&spawned.prompt) {
                        calls.retain(|c| c != call);
                    }
                    return;
                }
                let k = self
                    .spawns
                    .get(&spawned.prompt)
                    .and_then(|calls| calls.iter().position(|c| c == call))
                    .unwrap_or(0);
                let known = self
                    .children
                    .get(&spawned.prompt)
                    .and_then(|c| c.get(k))
                    .map(|c| c.id.clone());
                match known {
                    Some(id) => id,
                    None => {
                        self.pending.insert(
                            (spawned.prompt.clone(), k),
                            Returned {
                                call: call.to_string(),
                                agent_type,
                                turns: data.messages.clone(),
                            },
                        );
                        return;
                    }
                }
            }
        };
        self.paired.insert(child.clone());
        facts.push(Fact {
            agent: Some(child.clone()),
            ts: self.born.get(&child).copied().flatten(),
            kind: FactKind::Agent {
                kind: AgentKind::Subagent,
                parent: Some(owner.to_string()),
                agent_type,
                description: None,
                spawned_by: Some(call.to_string()),
                interactive: false,
            },
        });
        for turn in &data.messages {
            self.embedded_turn(&child, turn, facts);
        }
    }

    /// One turn of an embedded transcript. Its tokens are not counted: the
    /// child's row already carries its total, and arrives while it runs.
    fn embedded_turn(&mut self, child: &str, turn: &Data, facts: &mut Vec<Fact>) {
        let Some(role) = turn.role.as_deref() else {
            return;
        };
        if role == "system" {
            return;
        }
        let ts = turn.timestamp();
        let id = turn.id.clone().unwrap_or_default();
        let before = facts.len();
        self.turn_facts(child, role, turn, ts, &id, false, facts);
        if facts.len() == before {
            facts.push(Fact {
                agent: Some(child.to_string()),
                ts,
                kind: FactKind::Activity,
            });
        }
    }
}

/// Untimed session-level metadata.
fn meta(kind: FactKind) -> Fact {
    Fact {
        agent: None,
        ts: None,
        kind,
    }
}

/// The running cost, as the info overlay shows it.
fn cost_of(row: &TaskRow) -> Option<String> {
    let cost = row.costs.as_ref()?.cost?;
    Some(format!("${cost:.2}"))
}

/// The error a task stopped on, one line.
fn error_of(row: &TaskRow) -> Option<String> {
    row.last_error
        .as_ref()
        .and_then(|e| e.headline())
        .map(|e| truncate_summary(&e))
}

/// What a child's `status` says once it stops changing. `active` and
/// `running` are not endings.
fn ended_status(status: &str) -> Option<AgentStatus> {
    match status {
        "completed" => Some(AgentStatus::Done),
        "error" | "failed" => Some(AgentStatus::Failed),
        "cancelled" | "canceled" | "stopped" | "aborted" => Some(AgentStatus::Stopped),
        _ => None,
    }
}

/// The prompt a spawning call hands its child, which the child's row carries
/// as its `title`. `None` for a call that spawns nobody.
fn spawn_prompt(name: &str, call: &ToolCall) -> Option<String> {
    match name {
        "spawn_subagent" => call.argument("description"),
        "start_subtask" => call.argument("title"),
        _ => None,
    }
    .map(str::to_string)
}

/// What kind of child a spawning call asks for.
fn spawn_type(name: &str, call: &ToolCall) -> Option<String> {
    match name {
        "spawn_subagent" => call.argument("name").map(str::to_string),
        "start_subtask" => Some("subtask".into()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tool summaries: the Bob tool vocabulary and what makes a good one-liner
// ---------------------------------------------------------------------------

/// One line for a tool call: the command, the path, the pattern, the prompt.
fn summarize(name: &str, call: &ToolCall, cwd: Option<&str>) -> Option<String> {
    let arg = |k: &str| call.argument(k).map(str::to_string);
    let path = |k: &str| call.argument(k).map(|p| short_path(p, cwd));
    let s = match name {
        "execute_command" => arg("command"),
        "read_file" | "write_file" | "apply_diff" | "search_and_replace" | "insert_content"
        | "list_files" => path("path")
            .or_else(|| path("target_file"))
            .or_else(|| first_arg_path(call, cwd)),
        "glob" | "grep" => arg("pattern").or_else(|| arg("query")),
        "use_skill" => arg("skill_name"),
        "switch_mode" => arg("mode_slug").or_else(|| arg("mode")),
        "ask_followup_question" => arg("question"),
        "search_bob_docs" | "search_ibm_docs" => arg("query"),
        "web_fetch" => arg("url"),
        "spawn_subagent" => {
            let prompt = call
                .argument("description")
                .and_then(|d| d.lines().find(|l| !l.trim().is_empty()));
            match (call.argument("name"), prompt) {
                (Some(kind), Some(p)) => Some(format!("{kind}: {p}")),
                (kind, p) => kind.or(p).map(str::to_string),
            }
        }
        "start_subtask" | "create_html_artifact" => arg("title"),
        "FindSymbol" => arg("symbol").or_else(|| arg("name")),
        "update_todo_list" => None,
        _ => call.first_string().map(str::to_string),
    };
    s.map(|s| truncate_summary(&s))
}

/// `read_file` may take its files as `args: [{path, …}]`: the first one.
fn first_arg_path(call: &ToolCall, cwd: Option<&str>) -> Option<String> {
    call.arguments
        .get("args")?
        .as_array()?
        .first()?
        .get("path")?
        .as_str()
        .map(|p| short_path(p, cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Every export under `assets/bob/`, each a session as Bob stored it,
    /// run through the whole conformance check.
    #[test]
    fn captures_conform() {
        let Some(dir) = crate::provider::harness::fixture_dir("bob") else {
            return;
        };
        let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        fixtures.sort();
        assert!(!fixtures.is_empty(), "no fixtures under {}", dir.display());
        for fixture in fixtures {
            let name = fixture.file_stem().unwrap().to_str().unwrap().to_string();
            let path = fixture.clone();
            let streams = move || -> Vec<Vec<Statement>> {
                let text = std::fs::read_to_string(&path).unwrap();
                let mut stream = Stream::new();
                vec![text.lines().filter_map(|l| stream.push(l)).collect()]
            };
            crate::provider::harness::conform("bob", &name, streams);
        }
    }

    /// The stream's joins must not depend on when a task's rows land
    /// relative to the turns: a child's row before or after the call that
    /// spawned it, before or after the result carrying its transcript, and
    /// two children in either order. Turns keep their own time order, which
    /// is what the feeder guarantees (a call always precedes its result),
    /// and the root's row stays first, since lines before it state nothing
    /// by design.
    #[test]
    fn fixture_folds_the_same_however_task_rows_interleave() {
        use crate::state::session::SessionModel;
        let Some(dir) = crate::provider::harness::fixture_dir("bob") else {
            return;
        };
        let text = std::fs::read_to_string(dir.join("shell-2.0.3.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // One queue per task (its `created` then `updated`), one for the turns.
        let mut queues: Vec<Vec<&str>> = Vec::new();
        let mut turns = Vec::new();
        for line in &lines[1..] {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if v["table"] == "tasks" {
                let id = v["row"]["id"].as_str().unwrap();
                match queues.iter_mut().find(|q| q[0].contains(id)) {
                    Some(q) => q.push(line),
                    None => queues.push(vec![line]),
                }
            } else {
                turns.push(*line);
            }
        }
        assert!(
            queues.len() >= 3 && turns.len() > 5,
            "the fixture has children and turns"
        );
        queues.push(turns);
        let fold = |order: &[&str]| {
            let mut stream = Stream::new();
            let mut model = SessionModel::new("s".into());
            for line in std::iter::once(&lines[0]).chain(order) {
                if let Some(st) = stream.push(line) {
                    for f in &st.facts {
                        model.apply_fact(f);
                    }
                }
            }
            model.recompute_group_status();
            crate::provider::harness::snapshot(&model)
        };
        let baseline = fold(&queues.concat());
        let mut seed = 7u64;
        for _ in 0..40 {
            let mut qs = queues.clone();
            let mut order = Vec::new();
            while qs.iter().any(|q| !q.is_empty()) {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let nonempty: Vec<usize> = (0..qs.len()).filter(|&i| !qs[i].is_empty()).collect();
                let pick = nonempty[(seed >> 33) as usize % nonempty.len()];
                order.push(qs[pick].remove(0));
            }
            assert_eq!(fold(&order), baseline, "order-dependent fold");
        }
    }

    const ROOT: &str = r#"{"table":"tasks","seen":"created","row":{"id":"r","parent_id":null,"project_id":"file:/p","title":"do the thing\nmore","status":"active","task_type":"normal","env":{"workspace":"/p","modeId":"agent"},"costs":{"output":10,"cost":0.05},"created_at":1789649352547,"updated_at":1789649372774}}"#;

    #[test]
    fn root_row_states_main_and_session_rows() {
        let mut s = Stream::new();
        let st = s.push(ROOT).unwrap();
        assert_eq!(st.at, wire::millis(1789649352547));
        assert!(matches!(
            &st.facts[0].kind,
            FactKind::Agent { kind: AgentKind::Main, interactive: true, agent_type, .. } if agent_type.as_deref() == Some("bob")
        ));
        assert_eq!(st.facts[0].agent.as_deref(), Some("main"));
        assert!(
            st.facts
                .iter()
                .any(|f| matches!(&f.kind, FactKind::Title(t) if t == "do the thing"))
        );
        let labels: Vec<&str> = st
            .facts
            .iter()
            .filter_map(|f| match &f.kind {
                FactKind::Session { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["workspace", "mode", "status", "cost"]);
        // A later look at the row restates the title and the cost: both
        // are latest-wins metadata, and the IDE titles a task late.
        let up = s
            .push(&ROOT.replace(r#""seen":"created""#, r#""seen":"updated""#))
            .unwrap();
        assert!(up.is_session_meta());
        assert!(
            up.facts
                .iter()
                .any(|f| matches!(&f.kind, FactKind::Title(t) if t == "do the thing"))
        );
        // Lines before the root state nothing.
        let mut fresh = Stream::new();
        assert!(
            fresh
                .push(r#"{"table":"messages","row":{"id":"m","task_id":"r","role":"user","data":{"content":"x","_meta":{"timestamp":1}}}}"#)
                .is_none()
        );
    }

    const CHILD: &str = r#"{"table":"tasks","seen":"created","row":{"id":"c","parent_id":"r","title":"count the lines","status":"running","task_type":"subagent","costs":null,"created_at":1789649747462,"updated_at":1789649747462}}"#;
    const DONE: &str = r#"{"table":"tasks","seen":"updated","row":{"id":"c","parent_id":"r","title":"count the lines","status":"completed","task_type":"subagent","costs":{"output":40,"cost":0.01},"created_at":1789649747462,"updated_at":1789649750614}}"#;
    const SPAWN: &str = r#"{"table":"messages","row":{"id":"a1","task_id":"r","role":"assistant","data":{"role":"assistant","content":"spawning","_meta":{"timestamp":1789649745117,"spend":{"output":9}},"toolCalls":[{"id":"call_s","name":"spawn_subagent","arguments":{"name":"general","description":"count the lines"}}]}}}"#;
    /// The call's result, naming its child and carrying its transcript.
    const RESULT: &str = r#"{"table":"messages","row":{"id":"t1","task_id":"r","role":"tool","data":{"role":"tool","content":"<task_result>2</task_result>","toolUsage":{"signature":{"id":"call_s","isError":false}},"_meta":{"timestamp":1789649750615,"subagentId":"c","agentType":"general"},"messages":[
        {"role":"system","content":"You are a subagent.","_meta":{"timestamp":1789649747462},"id":"s"},
        {"role":"user","content":"count the lines","_meta":{"timestamp":1789649747462},"id":"u"},
        {"role":"assistant","content":"","_meta":{"timestamp":1789649749143,"spend":{"output":30}},"toolCalls":[{"id":"call_r","name":"read_file","arguments":{"path":"main.rs"}}],"id":"a"},
        {"role":"tool","content":"1 | fn main() {}","toolUsage":{"signature":{"id":"call_r","isError":false}},"_meta":{"timestamp":1789649749171},"id":"t"},
        {"role":"assistant","content":"2 lines.","_meta":{"timestamp":1789649750383,"spend":{"output":9}},"stop":true,"id":"z"}
    ]}}}"#;

    fn facts_by<'a>(st: &'a Statement, agent: &str) -> Vec<&'a FactKind> {
        st.facts
            .iter()
            .filter(|f| f.agent.as_deref() == Some(agent))
            .map(|f| &f.kind)
            .collect()
    }

    fn names(kinds: &[&FactKind]) -> Vec<&'static str> {
        kinds.iter().map(|k| k.name()).collect()
    }

    /// The child is born from its row and ended by its later look; the
    /// join to the call, and the child's transcript, come from the call's
    /// result, which names the child. Either may be read first.
    #[test]
    fn a_result_joins_its_child_by_id_and_states_its_transcript() {
        // Row first, then the call and its result.
        let mut s = Stream::new();
        s.push(ROOT);
        let born = s.push(CHILD).unwrap();
        assert!(matches!(
            &born.facts[0].kind,
            FactKind::Agent { kind: AgentKind::Subagent, parent: Some(p), spawned_by: None, description: Some(d), .. } if p == "main" && d == "count the lines"
        ));
        let call = s.push(SPAWN).unwrap();
        assert!(
            call.facts
                .iter()
                .any(|f| matches!(&f.kind, FactKind::Spawn { call } if call == "call_s"))
        );
        assert!(
            facts_by(&call, "c").is_empty(),
            "the call alone names no child"
        );
        let st = s.push(RESULT).unwrap();
        assert_eq!(st.at, wire::millis(1789649750615));
        let by_c = facts_by(&st, "c");
        assert_eq!(
            names(&by_c),
            ["Agent", "Activity", "ToolStart", "ToolEnd", "Reasoning"],
            "the join, then the transcript: the user turn is activity, the system turn nothing, no tokens (the row totals them)"
        );
        assert!(matches!(
            by_c[0],
            FactKind::Agent { spawned_by: Some(c), agent_type: Some(t), parent: Some(p), .. } if c == "call_s" && t == "general" && p == "main"
        ));
        let read = st
            .facts
            .iter()
            .find(|f| {
                matches!(f.kind, FactKind::ToolStart { .. }) && f.agent.as_deref() == Some("c")
            })
            .unwrap();
        assert_eq!(read.ts, wire::millis(1789649749143), "the turn's own time");
        assert!(
            matches!(&read.kind, FactKind::ToolStart { call, summary, .. } if call == "call_r" && summary.as_deref() == Some("main.rs"))
        );
        assert!(
            facts_by(&st, "main")
                .iter()
                .any(|k| matches!(k, FactKind::ToolEnd { .. })),
            "the spawn's own verdict is the parent's"
        );
        let ended = s.push(DONE).unwrap();
        assert_eq!(ended.at, wire::millis(1789649750614));
        assert!(
            ended
                .facts
                .iter()
                .any(|f| matches!(f.kind, FactKind::Ended(AgentStatus::Done)))
        );
        assert!(ended.facts.iter().any(
            |f| matches!(&f.kind, FactKind::Tokens { output: 40, dedup: Some(d) } if d == "c@40")
        ));
        // The same look again adds nothing.
        assert!(
            !s.push(DONE)
                .unwrap()
                .facts
                .iter()
                .any(|f| matches!(f.kind, FactKind::Tokens { .. }))
        );

        // The result first: the child is stated from it, and its row
        // later only fills in the description.
        let mut s = Stream::new();
        s.push(ROOT);
        s.push(SPAWN);
        let st = s.push(RESULT).unwrap();
        assert_eq!(
            names(&facts_by(&st, "c")),
            ["Agent", "Activity", "ToolStart", "ToolEnd", "Reasoning"]
        );
        let born = s.push(CHILD).unwrap();
        assert!(matches!(
            &born.facts[0].kind,
            FactKind::Agent {
                spawned_by: None,
                description: Some(_),
                ..
            }
        ));
    }

    /// A result that names no child (an older Bob) is joined by prompt:
    /// the k-th call with a prompt to the k-th child carrying it as a title,
    /// whichever is read first. A result that errored joins nothing, and
    /// does not take the child of the call after it.
    #[test]
    fn a_result_naming_no_child_joins_by_prompt_unless_it_errored() {
        let unnamed = RESULT.replace(r#""subagentId":"c","agentType":"general""#, r#""x":1"#);
        // Row first.
        let mut s = Stream::new();
        s.push(ROOT);
        s.push(CHILD);
        s.push(SPAWN);
        let st = s.push(&unnamed).unwrap();
        assert!(matches!(
            facts_by(&st, "c")[0],
            FactKind::Agent { spawned_by: Some(c), agent_type: Some(t), .. } if c == "call_s" && t == "general"
        ));
        // Result first: held for the row.
        let mut s = Stream::new();
        s.push(ROOT);
        s.push(SPAWN);
        let st = s.push(&unnamed).unwrap();
        assert!(facts_by(&st, "c").is_empty(), "no child to name yet");
        let born = s.push(CHILD).unwrap();
        assert_eq!(
            names(&facts_by(&born, "c")),
            ["Agent", "Activity", "ToolStart", "ToolEnd", "Reasoning"]
        );
        assert!(matches!(
            &born.facts[0].kind,
            FactKind::Agent { spawned_by: Some(c), .. } if c == "call_s"
        ));
        // A failed spawn with the same prompt, before the one that worked.
        let failed_call = SPAWN
            .replace("call_s", "call_x")
            .replace(r#""id":"a1""#, r#""id":"a0""#);
        let failed = r#"{"table":"messages","row":{"id":"t0","task_id":"r","role":"tool","data":{"role":"tool","content":"Error: no capacity","toolUsage":{"signature":{"id":"call_x","isError":true}},"_meta":{"timestamp":1789649745200}}}}"#;
        let mut s = Stream::new();
        s.push(ROOT);
        s.push(CHILD);
        s.push(&failed_call);
        let st = s.push(failed).unwrap();
        assert!(
            facts_by(&st, "c").is_empty(),
            "an errored spawn names nobody"
        );
        s.push(SPAWN);
        let st = s.push(&unnamed).unwrap();
        assert!(matches!(
            facts_by(&st, "c")[0],
            FactKind::Agent { spawned_by: Some(c), .. } if c == "call_s"
        ));
    }

    #[test]
    fn turns_state_prompts_reasoning_tools_and_verdicts() {
        let mut s = Stream::new();
        s.push(ROOT);
        let user = s.push(r#"{"table":"messages","row":{"id":"u1","task_id":"r","role":"user","data":{"role":"user","content":"do it","_meta":{"timestamp":1789649352560}}}}"#).unwrap();
        assert!(matches!(&user.facts[0].kind, FactKind::Prompt(t) if t == "do it"));
        // A skill launched by command: the content is the skill's text, the
        // prompt is what was typed.
        let skill = s.push(r#"{"table":"messages","row":{"id":"u1b","task_id":"r","role":"user","data":{"role":"user","content":"---\nname: pr-review\ndescription: ...\n---\n\n2331","_meta":{"timestamp":1789649352561,"mask":"/pr-review 2331"}}}}"#).unwrap();
        assert!(matches!(&skill.facts[0].kind, FactKind::Prompt(t) if t == "/pr-review 2331"));
        let asst = s.push(r#"{"table":"messages","row":{"id":"a1","task_id":"r","role":"assistant","data":{"role":"assistant","content":"running ls","_meta":{"timestamp":1789649365796,"spend":{"output":64}},"toolCalls":[{"id":"call_1","name":"execute_command","arguments":{"command":"ls -la"}}]}}}"#).unwrap();
        assert!(matches!(&asst.facts[0].kind, FactKind::Reasoning(t) if t == "running ls"));
        assert!(asst.facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::Tokens { output: 64, dedup: Some(d) } if d == "a1"
        )));
        assert!(asst.facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::ToolStart { call, name, summary } if call == "call_1" && name == "execute_command" && summary.as_deref() == Some("ls -la")
        )));
        let ok = s.push(r#"{"table":"messages","row":{"id":"t1","task_id":"r","role":"tool","data":{"role":"tool","content":"total 16","toolUsage":{"signature":{"id":"call_1","name":"execute_command","isError":false}},"_meta":{"timestamp":1789649366551}}}}"#).unwrap();
        assert!(
            matches!(&ok.facts[0].kind, FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "call_1")
        );
        let err = s.push(r#"{"table":"messages","row":{"id":"t2","task_id":"r","role":"tool","data":{"role":"tool","content":"Error: no","toolUsage":{"signature":{"id":"call_2","isError":true}},"_meta":{"timestamp":1789649366600}}}}"#).unwrap();
        assert!(matches!(
            &err.facts[0].kind,
            FactKind::ToolEnd {
                outcome: Outcome::Err,
                ..
            }
        ));
        // The system prompt states nothing; a hidden turn's text is nobody's,
        // but the calls it answers still end; narration in the model's voice
        // is activity only.
        assert!(s.push(r#"{"table":"messages","row":{"id":"s1","task_id":"r","role":"system","data":{"role":"system","content":"You are Bob"}}}"#).is_none());
        let hidden = s.push(r#"{"table":"messages","row":{"id":"h1","task_id":"r","role":"assistant","data":{"role":"assistant","content":"Step marked completed.","_meta":{"hide":true,"notAi":true,"timestamp":1}}}}"#).unwrap();
        assert!(matches!(hidden.facts[0].kind, FactKind::Activity));
        let hidden_end = s.push(r#"{"table":"messages","row":{"id":"h2","task_id":"r","role":"tool","data":{"role":"tool","content":"x","toolUsage":{"signature":{"id":"call_2","isError":false}},"_meta":{"hide":true,"timestamp":1789649366700}}}}"#).unwrap();
        assert!(
            matches!(&hidden_end.facts[0].kind, FactKind::ToolEnd { call, .. } if call == "call_2")
        );
        let narration = s.push(r#"{"table":"messages","row":{"id":"n1","task_id":"r","role":"assistant","data":{"role":"assistant","content":"Comparing branches.","_meta":{"notAi":true,"timestamp":2}}}}"#).unwrap();
        assert!(matches!(narration.facts[0].kind, FactKind::Activity));
        // A child's user turn is not a prompt.
        s.push(r#"{"table":"tasks","seen":"created","row":{"id":"c","parent_id":"r","title":"t","task_type":"subtask","created_at":3}}"#);
        let sub = s.push(r#"{"table":"messages","row":{"id":"u2","task_id":"c","role":"user","data":{"role":"user","content":"instruction","_meta":{"timestamp":4}}}}"#).unwrap();
        assert_eq!(sub.facts[0].agent.as_deref(), Some("c"));
        assert!(matches!(sub.facts[0].kind, FactKind::Activity));
    }

    #[test]
    fn summaries_follow_the_tool_vocabulary() {
        let call = |json: &str| -> ToolCall { serde_json::from_str(json).unwrap() };
        let cwd = Some("/p");
        assert_eq!(
            summarize(
                "read_file",
                &call(r#"{"arguments":{"path":"/p/src/a.rs"}}"#),
                cwd
            )
            .as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(
            summarize(
                "read_file",
                &call(r#"{"arguments":{"args":[{"path":"b.rs"}]}}"#),
                cwd
            )
            .as_deref(),
            Some("b.rs")
        );
        assert_eq!(
            summarize(
                "spawn_subagent",
                &call(r#"{"arguments":{"name":"general","description":"Read x.\nUNIT=1"}}"#),
                cwd
            )
            .as_deref(),
            Some("general: Read x.")
        );
        assert_eq!(
            summarize(
                "use_skill",
                &call(r#"{"arguments":{"skill_name":"probe"}}"#),
                cwd
            )
            .as_deref(),
            Some("probe")
        );
        assert_eq!(
            summarize(
                "mcp__x__y",
                &call(r#"{"arguments":{"n":1,"q":"first"}}"#),
                cwd
            )
            .as_deref(),
            Some("first")
        );
        assert_eq!(
            summarize(
                "update_todo_list",
                &call(r#"{"arguments":{"todos":"[ ] a"}}"#),
                cwd
            ),
            None
        );
    }
}
