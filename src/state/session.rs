//! `SessionModel`: the pure domain model — agents, their statuses, and tool
//! calls, folded solely from `Fact`s. No provider records and no rataflow types
//! here; the graph layer ([`crate::state::graph`]) projects this onto a `Flow`.
//!
//! Node ids are stable strings: `"main"` for the root agent, the 17-hex
//! `agentId` for direct/workflow subagents, and the workflow id for a workflow
//! group node. Spawn order is tracked explicitly so layout and navigation are
//! deterministic.

use imbl::{HashMap, HashSet, OrdMap, Vector};

use chrono::{DateTime, Utc};

use crate::fact::{Fact, FactKind, Outcome};

/// Stable node id of the main (root) agent.
pub const MAIN_ID: &str = "main";

/// Silence window after which an interactive sidechain (fork) is shown as
/// done. Forks are human-paced — lulls while the user types are normal — so
/// this is minutes, not the main agent's seconds-scale file-growth window.
const INTERACTIVE_IDLE_SECS: i64 = 120;

/// The full derived view of a session.
///
/// `Clone` is O(1): every growing collection here is persistent
/// ([`imbl`]), so cloning shares structure rather than copying it. That is what
/// makes a snapshot cheap enough to keep several of.
#[derive(Clone, PartialEq)]
pub struct SessionModel {
    pub session_id: String,
    /// Agents keyed by stable node id (`"main"`, `agentId`, or `wf-id`).
    pub(crate) agents: OrdMap<String, AgentInfo>,
    /// Stable spawn order of node ids (insertion order). Drives layout/nav.
    pub(crate) spawn_order: Vector<String>,
    /// Most recent activity timestamp seen across all files.
    pub last_activity: Option<DateTime<Utc>>,
    /// Names stated for an agent by records other than its own (`Label`
    /// facts: a workflow launch naming its group). Kept as a FACT rather than
    /// applied on arrival: the label and the agent's birth fold in either order.
    labels: OrdMap<String, (Option<String>, Option<String>)>,
    /// Every observed tool end, `call → (is_err, ts)`, kept so model state is a
    /// function of the fact SET, not of arrival order: a spawning call's end
    /// can arrive before the agent it spawned exists (live attach applies the
    /// main transcript before directory scans deliver metas; replay merges many
    /// files). The timestamp matters because a PARALLEL subagent's spawn result
    /// is an immediate acknowledgement (ms after the call), NOT its completion,
    /// so it only completes the subagent when not superseded by the subagent's
    /// own later activity (see [`resolve_spawn_status`](Self::resolve_spawn_status)).
    completed_calls: HashMap<String, (bool, Option<DateTime<Utc>>)>,
    /// Authoritative terminal status per agent, from an `Ended` fact (a
    /// `<task-notification>`, a workflow journal `result`, a Codex lifecycle
    /// record). Recorded order-independently — the fact may arrive before the
    /// agent exists — and applied by
    /// [`resolve_spawn_status`](Self::resolve_spawn_status), where it OUTRANKS
    /// the spawn acknowledgement and time-derived liveness.
    ended: HashMap<String, AgentStatus>,
    /// Provenance facts: why each spawned agent exists, keyed by the spawning
    /// call (order-independent, like the other stores). Captured at the
    /// `Spawn` fact; joined to the agent via `spawned_by_tool_use` at render.
    spawn_context: HashMap<String, SpawnContext>,
    /// Every plain user prompt in the main transcript, in order — the
    /// session's spine. Tool calls and spawns attribute to a prompt era via
    /// [`Self::prompt_for_ts`] (timestamp-derived, order-independent).
    pub(crate) prompts: Vector<PromptInfo>,
    /// Excerpt of each agent's most recent reasoning. One logical turn spans
    /// several records, so the reasoning for a spawn usually lives on an
    /// EARLIER record than the call — this is the cross-record fallback for
    /// [`SpawnContext::reasoning`].
    last_reasoning: HashMap<String, String>,
}

/// A notable timeline event for the scrubber's log line: a prompt (era
/// boundary), an agent spawn, or a tool failure. Surfaced by
/// [`SessionModel::latest_event_at`] and rendered with an icon matching the
/// scrubber's marker glyphs (◆ / ❋ / ✗).
#[derive(Debug, Clone)]
pub struct LogEvent {
    pub ts: DateTime<Utc>,
    pub kind: LogKind,
    pub text: String,
}

/// Which marker a [`LogEvent`] is — selects the caption's icon and colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Prompt,
    Spawn,
    Failure,
}

/// One user prompt in the main transcript — an era boundary on the session's
/// timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptInfo {
    /// One-line excerpt of the prompt text.
    pub excerpt: String,
    /// Recorded timestamp of the prompt entry.
    pub ts: Option<DateTime<Utc>>,
}

/// Why an agent exists: the spawning call's timestamp (the prompt era is
/// DERIVED from it via [`SessionModel::prompt_for_ts`] — order-independent,
/// like all era attribution) and the assistant text immediately preceding the
/// spawning tool call (stored: reasoning is not timestamp-derivable).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpawnContext {
    /// Timestamp of the spawning `tool_use` entry.
    pub ts: Option<DateTime<Utc>>,
    /// Excerpt of the assistant's reasoning right before the spawn.
    pub reasoning: Option<String>,
}

// The vocabulary shared with every provider lives on the boundary;
// re-exported here so `state::session::AgentKind` keeps resolving.
pub use crate::fact::{AgentKind, AgentStatus};

impl AgentStatus {
    /// The status glyph — single source for cards, cells, panel, inspect.
    /// (Color is a theme concern and lives in the ui layer.)
    pub fn glyph(self) -> char {
        match self {
            AgentStatus::Running => '●',
            AgentStatus::Idle => '◌',
            AgentStatus::Done => '✓',
            AgentStatus::Failed => '✗',
            AgentStatus::Stopped => '■',
        }
    }
}

/// The status word for display, from raw status + interactivity. Single source
/// for the wording rule (cards, panel, inspect) — interactive agents say
/// "active" (we know of recent entries, not that a task executes), spawned say
/// "running". A free fn so `AgentNode` (no `AgentInfo`) shares it too.
pub fn status_word(status: AgentStatus, interactive: bool) -> &'static str {
    match status {
        AgentStatus::Running if interactive => "active",
        AgentStatus::Running => "running",
        AgentStatus::Idle => "idle",
        AgentStatus::Done => "done",
        AgentStatus::Failed => "failed",
        AgentStatus::Stopped => "stopped",
    }
}

/// State of a single tool call, paired from `tool_use` + later `tool_result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    /// `tool_use` seen, no matching `tool_result` yet.
    Pending,
    /// Completed successfully (`is_error` absent or false).
    Ok,
    /// Completed with `is_error == true`.
    Err,
}

/// One tool invocation within an agent.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallInfo {
    /// `tool_use.id` — join key for the result.
    pub id: String,
    /// Tool name (e.g. `"Bash"`, `"Read"`, `"Agent"`).
    pub name: String,
    /// Short human summary (e.g. a command or path), if derivable.
    pub summary: Option<String>,
    /// Timestamp of the `tool_use` (when the tool started).
    pub ts: Option<DateTime<Utc>>,
    /// Timestamp of the `tool_result` (when it finished). `None` while pending.
    pub end_ts: Option<DateTime<Utc>>,
    pub state: ToolState,
}

impl ToolCallInfo {
    /// How long this tool ran — or has been running so far, if still pending.
    /// `now` (the timeline's `now_reference`) drives the live tick while pending;
    /// a completed call uses its recorded `end_ts`. `None` if it has no start ts.
    pub fn duration(&self, now: Option<DateTime<Utc>>) -> Option<chrono::Duration> {
        let start = self.ts?;
        let end = self.end_ts.or(now)?;
        Some((end - start).max(chrono::Duration::zero()))
    }
}

/// Everything known about one agent node.
///
/// Cloned whenever the containing map copy-on-writes the node holding it, so
/// its own growing collections are persistent too — a plain `Vec` of tool calls
/// here would make every tool-call append copy the agent's whole history.
#[derive(Clone, PartialEq)]
pub struct AgentInfo {
    pub kind: AgentKind,
    /// Interactive category (main-session semantics: no completion evidence
    /// exists in the format; liveness is activity-inferred and the agent
    /// never claims done/failed). Decided structurally at creation
    /// (`kind == Main`) or when the meta reveals a fork — never re-derived
    /// from strings at call sites.
    interactive: bool,
    /// e.g. `"claude-code-guide"`, `"workflow-subagent"`.
    pub agent_type: Option<String>,
    /// From `meta.description` or the `Agent` tool_use `input.description`.
    pub description: Option<String>,
    /// Node id of the parent (`"main"` or a workflow id).
    pub parent: Option<String>,
    /// The `toolUseId` that spawned this agent — completion join key.
    pub spawned_by_tool_use: Option<String>,
    pub status: AgentStatus,
    /// A RELIABLE completion signal has been recorded (a non-superseded spawn
    /// ack, or a workflow journal `result`) — the status is terminal and must
    /// not be revived by time-derived liveness. `false` for async agents whose
    /// only in-band signal is the spawn ack, which the launch supersedes: those
    /// are time-derived (`Running` while active, `Done` when quiet, reversible).
    pub(crate) terminal: bool,
    pub model: Option<String>,
    /// Tool calls in observed order.
    pub(crate) tool_calls: Vector<ToolCallInfo>,
    /// tool_use id → index into `tool_calls`, so the per-entry dedup check and
    /// per-result completion are O(1) instead of scanning every prior call
    /// (which made folding a tool-heavy agent quadratic).
    tool_index: HashMap<String, usize>,
    /// Summed `usage.output_tokens`, counted once per assistant turn.
    pub output_tokens: u64,
    pub first_ts: Option<DateTime<Utc>>,
    pub last_ts: Option<DateTime<Utc>>,
    /// `requestId`s whose usage has already been counted. Claude Code splits one
    /// logical assistant turn across several JSONL lines that each repeat the
    /// same cumulative `usage.output_tokens`; summing per line inflates the
    /// total (~2.7x on real transcripts), so we add a turn's tokens only the
    /// first time its `requestId` is seen. Lines without a `requestId` fall back
    /// to per-line summation.
    seen_request_ids: HashSet<String>,
}

impl AgentInfo {
    /// This agent's tool calls, oldest first. See
    /// [`SessionModel::spawn_order`] for why this is an iterator.
    pub fn tool_calls(&self) -> impl ExactSizeIterator<Item = &ToolCallInfo> {
        self.tool_calls.iter()
    }

    /// A fresh agent record of the given kind, defaulting to
    /// [`AgentStatus::Running`] with no activity yet.
    pub(crate) fn new(kind: AgentKind) -> Self {
        AgentInfo {
            kind,
            interactive: kind == AgentKind::Main,
            agent_type: None,
            description: None,
            parent: None,
            spawned_by_tool_use: None,
            status: AgentStatus::Running,
            terminal: false,
            model: None,
            tool_calls: Vector::new(),
            tool_index: HashMap::new(),
            output_tokens: 0,
            first_ts: None,
            last_ts: None,
            seen_request_ids: HashSet::new(),
        }
    }

    /// Whether this agent has main-session semantics: the format records no
    /// end for it, so liveness must be inferred from activity instead of
    /// completion evidence. True for the main agent and for forked sidechains
    /// (set structurally at creation / meta application).
    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// The status word for display — delegates to the free [`status_word`] so
    /// cards (`AgentNode`, which has no `AgentInfo`) share the exact wording.
    pub fn status_word(&self) -> &'static str {
        status_word(self.status, self.interactive)
    }

    /// Most recent tool call name, if any.
    pub fn last_tool(&self) -> Option<&str> {
        self.tool_calls.last().map(|t| t.name.as_str())
    }

    fn touch_ts(&mut self, ts: Option<DateTime<Utc>>) {
        if let Some(ts) = ts {
            if self.first_ts.is_none_or(|f| ts < f) {
                self.first_ts = Some(ts);
            }
            if self.last_ts.is_none_or(|l| ts > l) {
                self.last_ts = Some(ts);
            }
        }
    }
}

impl SessionModel {
    /// Create an empty model for the given session id, with the `"main"` agent
    /// pre-seeded as [`AgentStatus::Running`].
    pub fn new(session_id: String) -> Self {
        let mut agents = OrdMap::new();
        let mut main = AgentInfo::new(AgentKind::Main);
        // The root agent's display name is provider data, not a constant of the
        // graph projection. Seeded here until the parser layer owns it.
        main.agent_type = Some("claude".to_string());
        agents.insert(MAIN_ID.to_string(), main);
        SessionModel {
            session_id,
            agents,
            spawn_order: Vector::unit(MAIN_ID.to_string()),
            last_activity: None,
            labels: OrdMap::new(),
            completed_calls: HashMap::new(),
            ended: HashMap::new(),
            spawn_context: HashMap::new(),
            prompts: Vector::new(),
            last_reasoning: HashMap::new(),
        }
    }

    /// Ensure an agent of `kind` exists under `id`, returning whether it was
    /// newly created (a structural change).
    fn ensure_agent(&mut self, id: &str, kind: AgentKind) -> bool {
        if self.agents.contains_key(id) {
            return false;
        }
        let mut info = AgentInfo::new(kind);
        // Facts ABOUT this agent may have arrived before it did.
        if let Some(&status) = self.ended.get(id) {
            info.status = status;
            info.terminal = true;
        }
        if let Some((agent_type, description)) = self.labels.get(id) {
            info.agent_type = agent_type.clone();
            info.description = description.clone();
        }
        self.agents.insert(id.to_string(), info);
        self.spawn_order.push_back(id.to_string());
        true
    }

    fn note_activity(&mut self, ts: Option<DateTime<Utc>>) {
        if let Some(ts) = ts
            && self.last_activity.is_none_or(|l| ts > l)
        {
            self.last_activity = Some(ts);
        }
    }

    /// Fold one Claude record through the provider. Test convenience: the
    /// production paths hand facts in directly.
    #[cfg(test)]
    pub(crate) fn apply_update(&mut self, record: &crate::provider::claude::Record) -> bool {
        record
            .facts()
            .iter()
            .fold(false, |s, f| self.apply_fact(f) | s)
    }

    /// Fold one [`Fact`] into the model. The only entry point that mutates
    /// agents, statuses and tool calls; every provider reaches it and nothing
    /// in here knows which one did.
    ///
    /// Idempotent and commutative: re-applying a fact is a no-op, and two facts
    /// reach the same state in either order (ARCHITECTURE.md §1.1). Facts *by*
    /// an agent create it ("an agent exists once it has spoken"); facts *about*
    /// one (`Label`, `Ended`) only record until it appears.
    ///
    /// Returns `true` if graph *structure* changed.
    pub fn apply_fact(&mut self, fact: &Fact) -> bool {
        let mut structural = false;
        self.note_activity(fact.ts);
        if fact.is_session_meta() {
            return false;
        }
        let Some(id) = fact.agent.as_deref() else {
            return false;
        };
        let by_agent = !matches!(fact.kind, FactKind::Label { .. } | FactKind::Ended(_));
        if by_agent {
            match &fact.kind {
                FactKind::Agent { kind, parent, .. } => {
                    // A parent named before it has spoken is a group: the only
                    // node born from its first child rather than its own record.
                    // No format seen so far names a parent that later speaks
                    // for itself (Claude's groups never do); if one appears,
                    // this is where the placeholder's kind would be revisited.
                    if let Some(p) = parent
                        && !self.agents.contains_key(p)
                    {
                        structural |= self.ensure_agent(p, AgentKind::Group);
                        if let Some(group) = self.agents.get_mut(p) {
                            group.parent = Some(MAIN_ID.to_string());
                        }
                    }
                    structural |= self.ensure_agent(id, *kind);
                }
                _ => structural |= self.ensure_agent(id, AgentKind::Subagent),
            }
            if let Some(a) = self.agents.get_mut(id) {
                a.touch_ts(fact.ts);
            }
        }

        let structural = structural | self.fold_kind(id, fact);
        // The agent just produced activity — re-resolve its spawn
        // acknowledgement, since its `last_ts` may now supersede an immediate
        // one. Idempotent; a no-op for agents with no spawning call.
        if by_agent {
            self.resolve_spawn_status(id);
        }
        structural
    }

    /// The per-kind half of [`apply_fact`](Self::apply_fact): the agent (if
    /// any) already exists and has been touched. Returns whether structure
    /// changed.
    fn fold_kind(&mut self, id: &str, fact: &Fact) -> bool {
        let mut structural = false;
        match &fact.kind {
            FactKind::Activity
            | FactKind::Session { .. }
            | FactKind::Tally(_)
            | FactKind::Title(_) => {}
            FactKind::Agent {
                parent,
                agent_type,
                description,
                spawned_by,
                interactive,
                ..
            } => {
                if let Some(a) = self.agents.get_mut(id) {
                    if a.parent.is_none() {
                        a.parent = parent.clone();
                    }
                    if agent_type.is_some() {
                        a.agent_type = agent_type.clone();
                    }
                    if a.description.is_none() {
                        a.description = description.clone();
                    }
                    if a.spawned_by_tool_use.is_none() {
                        a.spawned_by_tool_use = spawned_by.clone();
                    }
                    a.interactive |= *interactive;
                }
                // The spawning call may have ended before this agent was seen —
                // its end survives in `completed_calls` regardless of order. Now
                // that the join key is set, the trailing resolve in `apply_fact`
                // reads it (honouring the immediate-ack rule, and any recorded
                // `Ended`, which outranks it).
            }
            FactKind::Label {
                agent_type,
                description,
            } => {
                self.labels
                    .insert(id.to_string(), (agent_type.clone(), description.clone()));
                if let Some(a) = self.agents.get_mut(id) {
                    if a.agent_type.is_none() && agent_type.is_some() {
                        a.agent_type = agent_type.clone();
                        // The node relabels: a structural change.
                        structural = true;
                    }
                    if a.description.is_none() {
                        a.description = description.clone();
                    }
                }
            }
            FactKind::Model(model) => {
                if let Some(a) = self.agents.get_mut(id)
                    && a.model.is_none()
                {
                    a.model = Some(model.clone());
                }
            }
            FactKind::Tokens { output, dedup } => {
                if let Some(a) = self.agents.get_mut(id) {
                    match dedup {
                        Some(key) if a.seen_request_ids.insert(key.clone()).is_some() => {}
                        // Saturating: counts come from untrusted transcript
                        // content; overflow must not panic (debug) or wrap.
                        _ => a.output_tokens = a.output_tokens.saturating_add(*output),
                    }
                }
            }
            // The session's spine is the root agent's prompts.
            FactKind::Prompt(text) if id == MAIN_ID => {
                let ex = excerpt(text);
                let ts = fact.ts;
                // Idempotent AND order-independent: a re-applied fact is a dup
                // wherever it lands, not only when it's the trailing prompt. A
                // genuine repeat at a *different* ts is kept.
                if !self.prompts.iter().any(|p| p.excerpt == ex && p.ts == ts) {
                    self.prompts.push_back(PromptInfo { excerpt: ex, ts });
                }
            }
            FactKind::Prompt(_) => {}
            FactKind::Reasoning(text) => {
                self.last_reasoning.insert(id.to_string(), excerpt(text));
            }
            FactKind::ToolStart {
                call,
                name,
                summary,
            } => {
                if let Some(a) = self.agents.get_mut(id)
                    && !a.tool_index.contains_key(call)
                {
                    a.tool_index.insert(call.clone(), a.tool_calls.len());
                    a.tool_calls.push_back(ToolCallInfo {
                        id: call.clone(),
                        name: name.clone(),
                        summary: summary.clone(),
                        ts: fact.ts,
                        end_ts: None,
                        state: ToolState::Pending,
                    });
                }
            }
            // Provenance: why the agent this call spawns will exist. The
            // reasoning is the agent's latest text before the call.
            FactKind::Spawn { call } => {
                let reasoning = self.last_reasoning.get(id).cloned();
                self.spawn_context
                    .entry(call.clone())
                    .or_insert_with(|| SpawnContext {
                        ts: fact.ts,
                        reasoning,
                    });
            }
            FactKind::ToolEnd { call, outcome } => {
                self.complete_tool(id, call, *outcome == Outcome::Err, fact.ts);
            }
            FactKind::Ended(status) => {
                self.ended.insert(id.to_string(), *status);
                self.resolve_spawn_status(id);
            }
        }
        structural
    }

    /// Complete a tool call by id within its owner, flipping its state and
    /// recording the end as a fact, since a spawning call's end is the
    /// acknowledgement for the agent it spawned.
    fn complete_tool(
        &mut self,
        owner: &str,
        call: &str,
        is_err: bool,
        end_ts: Option<DateTime<Utc>>,
    ) {
        let new_state = if is_err {
            ToolState::Err
        } else {
            ToolState::Ok
        };
        if let Some(agent) = self.agents.get_mut(owner)
            && let Some(&i) = agent.tool_index.get(call)
        {
            agent.tool_calls[i].state = new_state;
            // The result's timestamp is the tool's finish time → its duration.
            agent.tool_calls[i].end_ts = end_ts;
        }
        // Record the fact first — the spawned agent may not exist yet (arrival
        // order is unguaranteed) — then resolve any agent already present.
        // `resolve_spawn_status` decides whether this end is a real completion
        // or a premature spawn acknowledgement.
        self.completed_calls
            .insert(call.to_string(), (is_err, end_ts));
        let ids: Vec<String> = self
            .agents
            .iter()
            .filter(|(_, a)| a.spawned_by_tool_use.as_deref() == Some(call))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.resolve_spawn_status(&id);
        }
    }

    /// Derive a spawned agent's status from its recorded facts: an `Ended`
    /// fact if there is one, else the end of the call that spawned it.
    ///
    /// The spawning call's end is an *acknowledgement*, which doubles as the
    /// completion only for a synchronous subagent. The tell is the timestamp:
    /// an agent whose own activity postdates the acknowledgement was launched
    /// asynchronously, and the acknowledgement was a handle, not a result. A
    /// pure function of folded facts — `last_ts` and the recorded end — so it
    /// is order-invariant.
    fn resolve_spawn_status(&mut self, id: &str) {
        // An observed ending is the real terminal report — it outranks the
        // acknowledgement and the time-derived fallback. Applied here so any
        // later activity fold can't revive it.
        if let Some(&status) = self.ended.get(id)
            && let Some(agent) = self.agents.get_mut(id)
        {
            agent.status = status;
            agent.terminal = true;
            return;
        }
        let Some(agent) = self.agents.get(id) else {
            return;
        };
        let Some(call) = agent.spawned_by_tool_use.clone() else {
            return;
        };
        let Some(&(is_err, ack_ts)) = self.completed_calls.get(&call) else {
            return;
        };
        let superseded = matches!((agent.last_ts, ack_ts), (Some(l), Some(a)) if l > a);
        let agent = self.agents.get_mut(id).unwrap();
        if superseded {
            // Async agent: the acknowledgement was a spawn handle, not a
            // completion. Hand it to time-derived liveness (not terminal).
            agent.status = AgentStatus::Running;
            agent.terminal = false;
        } else {
            // The acknowledgement IS the completion (sync subagent, or no own
            // activity).
            agent.status = if is_err {
                AgentStatus::Failed
            } else {
                AgentStatus::Done
            };
            agent.terminal = true;
        }
    }

    /// Roll group nodes up from their children.
    ///
    /// A `Group` is by definition a node with no completion signal of its own
    /// (for Claude workflows, the `Workflow` tool_use id is not the workflow id
    /// the node is keyed by), so its status is "all-children-done": when every subagent under
    /// a workflow is terminal, the group is terminal too — `Failed` if any child
    /// failed, else `Done`. A childless workflow stays `Running`.
    ///
    /// Fully re-derived on every call — deliberately NOT monotonic: children
    /// are discovered incrementally and in no guaranteed order, so a group
    /// that rolled up to `Done` (all known children terminal) must revert to
    /// `Running` when a still-running child is discovered later. Derived state
    /// is a pure function of the current fact set, never of rollup history.
    pub fn recompute_group_status(&mut self) {
        let wf_ids: Vec<String> = self
            .agents
            .iter()
            .filter(|(_, a)| a.kind == AgentKind::Group)
            .map(|(id, _)| id.clone())
            .collect();

        for wf_id in wf_ids {
            let mut any = false;
            let mut all_terminal = true;
            let mut any_failed = false;
            for child in self.agents.values() {
                if child.parent.as_deref() == Some(wf_id.as_str()) {
                    any = true;
                    match child.status {
                        AgentStatus::Running | AgentStatus::Idle => all_terminal = false,
                        AgentStatus::Failed => any_failed = true,
                        // Done / Stopped are terminal but not failures.
                        AgentStatus::Done | AgentStatus::Stopped => {}
                    }
                }
            }
            let derived = if any && all_terminal {
                if any_failed {
                    AgentStatus::Failed
                } else {
                    AgentStatus::Done
                }
            } else {
                AgentStatus::Running
            };
            if let Some(group) = self.agents.get_mut(&wf_id) {
                group.status = derived;
            }
        }
    }

    /// Fold a Claude `meta.json` sidecar through the provider. Test
    /// convenience; returns `true` if it introduced a new agent node.
    #[cfg(test)]
    pub(crate) fn apply_meta(
        &mut self,
        agent_id: &str,
        workflow: Option<&str>,
        meta: &crate::provider::claude::wire::SubagentMeta,
    ) -> bool {
        crate::provider::claude::meta_facts(agent_id, workflow, meta)
            .iter()
            .fold(false, |s, f| self.apply_fact(f) | s)
    }

    /// Re-derive time-based liveness from each agent's own activity.
    ///
    /// Two families, both inferred from `last_ts` vs `reference` (the wall clock
    /// live, or `None` → the session's `last_activity` in replay, keeping replay
    /// deterministic on the recorded timeline):
    ///
    /// - **Interactive** (main/forks): no completion evidence exists in the
    ///   format, so `Running` within `INTERACTIVE_IDLE_SECS` of `reference`,
    ///   else `Idle` — never `Done`/`Failed` (unclaimable). Reversible: new
    ///   activity flips it back to `Running`.
    /// - **Subagents**: they DO terminate, but the `Agent` tool result is only a
    ///   spawn ack ("Async agent launched successfully"), not a completion. So a
    ///   subagent with no reliable completion on record has its liveness derived
    ///   from its OWN activity: `Running` while active, `Done` once quiet. A
    ///   reliable completion already recorded (`Done`/`Failed`/`Stopped` from an
    ///   async `<task-notification>`, a non-superseded ack, or a workflow journal
    ///   `result`) is terminal — only a still-`Running` subagent is refined here.
    ///
    /// "Active" is transcript activity within the window OR an unresolved
    /// (`Pending`) tool_call: a tool in flight is direct proof the agent is
    /// still working, so a long-running tool never settles it to `Done`/`Idle`
    /// mid-tool. Terminal agents short-circuit, so an abandoned pending tool
    /// can't keep a genuinely finished agent alive.
    ///
    /// Returns whether any status changed (lets callers skip graph work on the
    /// common nothing-happened tick).
    pub fn recompute_liveness(&mut self, now: Option<DateTime<Utc>>) -> bool {
        let Some(reference) = now.or(self.last_activity) else {
            return false;
        };
        // Two passes: decide in a read-only walk, then write ONLY the agents
        // whose status actually moved.
        //
        // The obvious single `values_mut()` loop takes a mutable borrow of
        // every agent on every call — and this runs once a second from
        // `status_tick`, plus on every resync, overwhelmingly finding nothing
        // to change. That is wasted work with plain maps and actively harmful
        // with persistent ones, where touching an entry copies it.
        let pending: Vec<(String, AgentStatus)> = self
            .agents
            .iter()
            .filter_map(|(id, agent)| {
                let ts = agent.last_ts?;
                // An unresolved tool_call is direct evidence the agent is still
                // working — stronger than "no transcript line for 120s". Without
                // it, an agent blocked on a long tool (a 2-minute Bash) looks
                // quiet and settles to Done/Idle mid-tool, then snaps back when
                // the result lands. A reliably-terminal agent short-circuits
                // below, so this can't revive a genuinely finished one.
                let active = (reference - ts).num_seconds() <= INTERACTIVE_IDLE_SECS
                    || agent
                        .tool_calls
                        .iter()
                        .any(|c| c.state == ToolState::Pending);
                let next = if agent.is_interactive() {
                    if active {
                        AgentStatus::Running
                    } else {
                        AgentStatus::Idle
                    }
                } else if agent.terminal {
                    // A reliably-completed subagent (sync ack / journal) is terminal.
                    agent.status
                } else if active {
                    // Async agent still producing activity — and REVERSIBLE: a
                    // long gap (e.g. a subagent running `cargo test`) that
                    // settled it to Done flips straight back to Running when it
                    // resumes.
                    AgentStatus::Running
                } else {
                    AgentStatus::Done
                };
                (next != agent.status).then(|| (id.clone(), next))
            })
            .collect();

        let changed = !pending.is_empty();
        for (id, next) in pending {
            if let Some(agent) = self.agents.get_mut(&id) {
                agent.status = next;
            }
        }
        changed
    }

    /// Mark the end of a finite stream (replay finished): every interactive
    /// agent goes `Idle` — the recording is over, nothing is active, and
    /// completion remains unclaimable.
    pub fn end_of_stream(&mut self) {
        // Same two-pass shape as `recompute_liveness`, for the same reason.
        let pending: Vec<(String, AgentStatus)> = self
            .agents
            .iter()
            .filter_map(|(id, agent)| {
                let next = if agent.is_interactive() {
                    // Interactive agents (main/forks) never "complete" — the
                    // stream ending just means they went quiet.
                    AgentStatus::Idle
                } else if agent.status == AgentStatus::Running {
                    // A subagent still Running at the recording's end has
                    // finished (its async spawn-ack Done was superseded by its
                    // own later activity via `resolve_spawn_status`; now there's
                    // no more).
                    AgentStatus::Done
                } else {
                    return None;
                };
                (next != agent.status).then(|| (id.clone(), next))
            })
            .collect();

        for (id, next) in pending {
            if let Some(agent) = self.agents.get_mut(&id) {
                agent.status = next;
            }
        }
    }

    /// Number of agents currently tracked.
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Total tool calls across all agents.
    pub fn tool_count(&self) -> usize {
        self.agents.values().map(|a| a.tool_calls.len()).sum()
    }

    /// Agent ids in spawn order.
    ///
    /// An iterator rather than the collection itself: the backing store is an
    /// `imbl` persistent type, which is an implementation detail of the fold
    /// rather than something the published API should pin down.
    pub fn spawn_order(&self) -> impl ExactSizeIterator<Item = &str> {
        self.spawn_order.iter().map(String::as_str)
    }

    /// Borrow an agent by node id.
    pub fn agent(&self, id: &str) -> Option<&AgentInfo> {
        self.agents.get(id)
    }

    /// Why `agent` exists, if its spawning call was observed: the triggering
    /// user prompt and the assistant reasoning before the spawn.
    pub fn provenance(&self, agent: &AgentInfo) -> Option<&SpawnContext> {
        self.spawn_context
            .get(agent.spawned_by_tool_use.as_deref()?)
    }

    /// The triggering prompt for a spawn, derived from the spawn timestamp's
    /// era — the same order-independent attribution as everything else.
    pub fn provenance_prompt(&self, ctx: &SpawnContext) -> Option<&str> {
        let idx = self.prompt_for_ts(ctx.ts)?;
        Some(self.prompts.get(idx)?.excerpt.as_str())
    }

    /// The most recent notable timeline event at or before `cursor` — what the
    /// scrubber narrates as a log line, updating as the playhead crosses each
    /// marker. Spans the three marker kinds so the line reads like "what's
    /// happening now": a human prompt (◆), an agent spawn (❋), or a tool failure
    /// (✗). All are events on the timeline, never stitched onto an agent. Ties
    /// keep the earlier-considered event; `None` before the first event.
    ///
    /// A spawn is timed by the agent's **birth** (when it starts to exist and its
    /// node appears), not the parent's spawn *call* — mirroring the strip's meta
    /// ❋. `meta_tool_use_ids` (the spawn `tool_use_id`s that have a discovered
    /// subagent) lets a call act only as a fallback for spawns whose subagent
    /// isn't loaded, matching the strip exactly.
    pub fn latest_event_at(
        &self,
        cursor: Option<DateTime<Utc>>,
        meta_tool_use_ids: &std::collections::BTreeSet<String>,
    ) -> Option<LogEvent> {
        let cursor = cursor?;
        let mut best: Option<LogEvent> = None;
        let mut consider = |ts: Option<DateTime<Utc>>, kind: LogKind, text: String| {
            if let Some(ts) = ts
                && ts <= cursor
                && best.as_ref().is_none_or(|b| ts > b.ts)
            {
                best = Some(LogEvent { ts, kind, text });
            }
        };
        for p in &self.prompts {
            consider(p.ts, LogKind::Prompt, p.excerpt.clone());
        }
        for agent in self.agents.values() {
            // A subagent's spawn = its birth: mark it when the agent starts to
            // exist (`first_ts`), where the node appears and the strip's meta ❋ sits.
            if matches!(agent.kind, AgentKind::Subagent) {
                let text = agent
                    .description
                    .clone()
                    .or_else(|| agent.agent_type.clone())
                    .unwrap_or_else(|| "subagent".to_string());
                consider(agent.first_ts, LogKind::Spawn, text);
            }
            for tc in &agent.tool_calls {
                if self.spawn_context.contains_key(&tc.id) {
                    // Fallback: a spawn whose subagent isn't loaded (no meta) is
                    // marked at the call — matching the strip's tool_use fallback.
                    if !meta_tool_use_ids.contains(&tc.id) {
                        let text = tc
                            .summary
                            .clone()
                            .unwrap_or_else(|| format!("spawned {}", tc.name));
                        consider(tc.ts, LogKind::Spawn, text);
                    }
                } else if tc.state == ToolState::Err {
                    let text = match &tc.summary {
                        Some(s) => format!("{} failed · {s}", tc.name),
                        None => format!("{} failed", tc.name),
                    };
                    // A failure "happens" when the error result returns (`end_ts`),
                    // not when the tool started — this is also where the scrubber's
                    // ✗ marker sits, so the log and the strip agree.
                    consider(tc.end_ts.or(tc.ts), LogKind::Failure, text);
                }
            }
        }
        best
    }

    /// The prompt era a timestamp falls in: index of the last prompt at or
    /// before `ts`. Derived (never stored on tool calls) so attribution is a
    /// pure function of recorded timestamps — cross-source arrival order
    /// cannot skew it. `None` when `ts` precedes every prompt or is absent.
    pub fn prompt_for_ts(&self, ts: Option<DateTime<Utc>>) -> Option<usize> {
        let ts = ts?;
        // Prompts are pushed in main-file order; timestamps are monotonic
        // within one file, so a reverse scan finds the era. (Linear is fine:
        // prompt counts are tens, not thousands.)
        self.prompts
            .iter()
            .rposition(|p| p.ts.is_some_and(|pt| pt <= ts))
    }

    /// The most recently active agent: latest `last_ts`, ties (and the
    /// no-timestamps case) broken toward the most recently spawned. `None`
    /// only when there are no agents.
    pub fn last_active_agent_id(&self) -> Option<String> {
        let mut best: Option<(&str, Option<DateTime<Utc>>)> = None;
        for id in &self.spawn_order {
            let Some(info) = self.agents.get(id) else {
                continue;
            };
            // `Option` orders `None < Some`; `>=` lets a later spawn win ties.
            if best.as_ref().is_none_or(|(_, ts)| info.last_ts >= *ts) {
                best = Some((id, info.last_ts));
            }
        }
        best.map(|(id, _)| id.to_string())
    }
}

/// One-line excerpt for provenance display: whitespace collapsed, hard cap so
/// adversarial 38KB lines can't bloat the model.
fn excerpt(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 240 {
        let mut t: String = one.chars().take(239).collect();
        t.push('…');
        t
    } else {
        one
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::claude::wire::{Entry, SubagentMeta, parse_line};
    use crate::provider::claude::{Record, Source};

    /// Build an `Entry` from a JSONL line, panicking in tests only if the
    /// fixture itself is malformed (parser returns `None`).
    fn entry(line: &str) -> Entry {
        parse_line(line).expect("test fixture must parse")
    }

    #[test]
    fn latest_event_at_narrates_across_marker_kinds() {
        let ts = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let tool =
            |id: &str, name: &str, summary: &str, t: &str, end: Option<&str>, state: ToolState| {
                ToolCallInfo {
                    id: id.into(),
                    name: name.into(),
                    summary: Some(summary.into()),
                    ts: Some(ts(t)),
                    end_ts: end.map(&ts),
                    state,
                }
            };
        let mut m = SessionModel::new("s".into());
        m.prompts.push_back(PromptInfo {
            excerpt: "review the codebase".into(),
            ts: Some(ts("2026-06-05T10:00:00Z")),
        });
        let main = m.agents.get_mut(MAIN_ID).unwrap();
        main.tool_calls.push_back(tool(
            "s1",
            "Agent",
            "hunt bugs",
            "2026-06-05T10:05:00Z",
            None,
            ToolState::Ok,
        ));
        // A slow Bash: started 10:10, failed (result) at 10:12.
        main.tool_calls.push_back(tool(
            "b1",
            "Bash",
            "cargo test",
            "2026-06-05T10:10:00Z",
            Some("2026-06-05T10:12:00Z"),
            ToolState::Err,
        ));
        // A later SUCCESSFUL non-spawn tool is not a log event.
        main.tool_calls.push_back(tool(
            "r1",
            "Read",
            "src/lib.rs",
            "2026-06-05T10:15:00Z",
            None,
            ToolState::Ok,
        ));

        // Which calls spawn is provenance the provider states, not a tool name
        // the model recognises: record it as the fold would.
        m.apply_fact(&Fact {
            agent: Some(MAIN_ID.to_string()),
            ts: Some(ts("2026-06-05T10:05:00Z")),
            kind: FactKind::Spawn { call: "s1".into() },
        });

        // No subagent is loaded here, so the spawning call ("s1") is the
        // fallback spawn marker (at call time).
        let none = std::collections::BTreeSet::new();

        // Before the first event, and with no playhead → nothing.
        assert!(
            m.latest_event_at(Some(ts("2026-06-05T09:00:00Z")), &none)
                .is_none()
        );
        assert!(m.latest_event_at(None, &none).is_none());

        let at = |t: &str| m.latest_event_at(Some(ts(t)), &none).expect("an event");
        // Prompt → spawn (its summary) → failure. The later successful Read is
        // skipped, so at 10:20 the failure is still the latest event.
        let p = at("2026-06-05T10:02:00Z");
        assert_eq!(p.kind, LogKind::Prompt);
        assert_eq!(p.text, "review the codebase");
        let s = at("2026-06-05T10:07:00Z");
        assert_eq!(s.kind, LogKind::Spawn);
        assert_eq!(s.text, "hunt bugs");
        // The failure is timed by its result (`end_ts` = 10:12), not its start
        // (10:10): at 10:11 it hasn't happened yet, so the spawn still stands.
        let mid = at("2026-06-05T10:11:00Z");
        assert_eq!(mid.kind, LogKind::Spawn);
        let f = at("2026-06-05T10:13:00Z");
        assert_eq!(f.kind, LogKind::Failure);
        assert_eq!(f.text, "Bash failed · cargo test");
        assert_eq!(f.ts, ts("2026-06-05T10:12:00Z"));
    }

    #[test]
    fn spawn_event_is_timed_by_birth_not_the_call() {
        let ts = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let mut m = SessionModel::new("s".into());
        // Main calls `Agent` at 10:00 (tool_use id "call1").
        m.agents
            .get_mut(MAIN_ID)
            .unwrap()
            .tool_calls
            .push_back(ToolCallInfo {
                id: "call1".into(),
                name: "Agent".into(),
                summary: Some("hunt bugs".into()),
                ts: Some(ts("2026-06-05T10:00:00Z")),
                end_ts: None,
                state: ToolState::Ok,
            });
        // The subagent it spawned is born (first activity) at 10:02.
        let mut sub = AgentInfo::new(AgentKind::Subagent);
        sub.description = Some("hunt bugs".into());
        sub.first_ts = Some(ts("2026-06-05T10:02:00Z"));
        m.agents.insert("a1000000000000001".into(), sub);
        // Its meta joins the call, so the call is NOT a fallback.
        let metas = std::collections::BTreeSet::from(["call1".to_string()]);

        // Between the call (10:00) and the birth (10:02): the call is suppressed
        // (its subagent has a meta) and the birth hasn't happened → no spawn yet.
        assert!(
            m.latest_event_at(Some(ts("2026-06-05T10:01:00Z")), &metas)
                .is_none()
        );
        // After the birth: the spawn shows, timed by birth (10:02), not the call.
        let e = m
            .latest_event_at(Some(ts("2026-06-05T10:03:00Z")), &metas)
            .expect("a spawn");
        assert_eq!(e.kind, LogKind::Spawn);
        assert_eq!(e.text, "hunt bugs");
        assert_eq!(e.ts, ts("2026-06-05T10:02:00Z"));
    }

    #[test]
    fn subagent_liveness_is_time_derived_running_then_done() {
        let sub_asst = |ts: &str| Record::Entry {
            source: Source::Sub("sub".into()),
            entry: entry(&format!(
                r#"{{"type":"assistant","uuid":"u","parentUuid":null,"timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"b1","name":"Bash","input":{{}}}}]}}}}"#
            )),
        };
        let sub_result = |ts: &str| Record::Entry {
            source: Source::Sub("sub".into()),
            entry: entry(&format!(
                r#"{{"type":"user","uuid":"r","parentUuid":null,"timestamp":"{ts}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"b1"}}]}}}}"#
            )),
        };
        let mut m = SessionModel::new("s".into());
        m.apply_update(&sub_asst("2026-06-07T13:00:00.000Z"));
        // Resolve the tool so liveness is driven by quiet-time, not the pending
        // tool (an unresolved tool holds it Running — see the next test).
        m.apply_update(&sub_result("2026-06-07T13:00:01.000Z"));
        assert_eq!(m.agent("sub").unwrap().status, AgentStatus::Running);

        // Reference just after its activity → still Running (within the window).
        m.recompute_liveness(Some("2026-06-07T13:01:00.000Z".parse().unwrap()));
        assert_eq!(m.agent("sub").unwrap().status, AgentStatus::Running);

        // Reference well past the idle window → the subagent has terminated
        // (no reliable in-band completion for async agents; quiet ⇒ Done).
        m.recompute_liveness(Some("2026-06-07T13:30:00.000Z".parse().unwrap()));
        assert_eq!(m.agent("sub").unwrap().status, AgentStatus::Done);

        // Fresh activity revives it — liveness is derived and reversible.
        m.apply_update(&sub_asst("2026-06-07T13:30:05.000Z"));
        m.recompute_liveness(Some("2026-06-07T13:30:06.000Z".parse().unwrap()));
        assert_eq!(m.agent("sub").unwrap().status, AgentStatus::Running);
    }

    #[test]
    fn an_unresolved_tool_keeps_a_quiet_subagent_running() {
        // A subagent blocked on a long tool produces no transcript output for
        // minutes, but its pending tool_call is direct proof it's still working.
        // It must NOT settle to Done mid-tool (which dropped its in-flight chip
        // and hid the tool's eventual result — a real error even went unshown).
        let sub_asst = |ts: &str| Record::Entry {
            source: Source::Sub("sub".into()),
            entry: entry(&format!(
                r#"{{"type":"assistant","uuid":"u","parentUuid":null,"timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"b1","name":"Bash","input":{{}}}}]}}}}"#
            )),
        };
        let sub_result = |ts: &str| Record::Entry {
            source: Source::Sub("sub".into()),
            entry: entry(&format!(
                r#"{{"type":"user","uuid":"r","parentUuid":null,"timestamp":"{ts}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"b1"}}]}}}}"#
            )),
        };
        let mut m = SessionModel::new("s".into());
        m.apply_update(&sub_asst("2026-06-07T13:00:00.000Z"));

        // Far past the idle window, but the Bash is still pending → stays Running.
        m.recompute_liveness(Some("2026-06-07T13:30:00.000Z".parse().unwrap()));
        assert_eq!(
            m.agent("sub").unwrap().status,
            AgentStatus::Running,
            "a pending tool keeps the agent Running past the quiet window"
        );

        // The tool resolves; now genuine quiet settles it to Done.
        m.apply_update(&sub_result("2026-06-07T13:30:01.000Z"));
        m.recompute_liveness(Some("2026-06-07T14:00:00.000Z".parse().unwrap()));
        assert_eq!(m.agent("sub").unwrap().status, AgentStatus::Done);
    }

    #[test]
    fn a_parallel_subagent_stays_running_past_its_immediate_spawn_ack() {
        let asst = |src: Source, id: &str, name: &str, ts: &str| Record::Entry {
            source: src,
            entry: entry(&format!(
                r#"{{"type":"assistant","uuid":"u","parentUuid":null,"timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}]}}}}"#
            )),
        };
        let result = |src: Source, tid: &str, ts: &str| Record::Entry {
            source: src,
            entry: entry(&format!(
                r#"{{"type":"user","uuid":"r","parentUuid":null,"timestamp":"{ts}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tid}"}}]}}}}"#
            )),
        };
        let meta = |agent: &str, tid: &str| Record::Meta {
            agent_id: agent.into(),
            workflow: None,
            meta: crate::provider::claude::wire::SubagentMeta {
                agent_type: Some("guide".into()),
                description: None,
                tool_use_id: Some(tid.into()),
                stopped_by_user: None,
            },
        };

        let mut m = SessionModel::new("s".into());
        // Main spawns the agent; its `Agent` result is an IMMEDIATE ack (5ms).
        m.apply_update(&asst(
            Source::Main,
            "ag",
            "Agent",
            "2026-06-05T10:00:00.000Z",
        ));
        m.apply_update(&result(Source::Main, "ag", "2026-06-05T10:00:00.005Z"));
        m.apply_update(&meta("sub", "ag"));
        // No own activity yet → the ack is its only signal → reads Done.
        assert_eq!(m.agent("sub").unwrap().status, AgentStatus::Done);

        // Then the subagent works for minutes: its own activity supersedes the
        // immediate ack, so it must read Running (else its chips are pruned as
        // orphans of a "finished" agent — the flicker bug).
        m.apply_update(&asst(
            Source::Sub("sub".into()),
            "b1",
            "Bash",
            "2026-06-05T10:03:00.000Z",
        ));
        assert_eq!(m.agent("sub").unwrap().status, AgentStatus::Running);

        // The recording ends → it settles back to Done.
        m.end_of_stream();
        assert_eq!(m.agent("sub").unwrap().status, AgentStatus::Done);
    }

    #[test]
    fn a_task_notification_marks_the_agent_stopped_and_is_not_a_prompt() {
        let sub_asst = |ts: &str| Record::Entry {
            source: Source::Sub("sub".into()),
            entry: entry(&format!(
                r#"{{"type":"assistant","uuid":"u","parentUuid":null,"timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"b","name":"Bash","input":{{}}}}]}}}}"#
            )),
        };
        let notif = Record::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"user","uuid":"n","parentUuid":null,"timestamp":"2026-06-05T10:05:00.000Z","message":{"role":"user","content":"<task-notification>\n<task-id>sub</task-id>\n<status>stopped</status>\n<summary>x</summary>\n</task-notification>"}}"#,
            ),
        };

        let mut m = SessionModel::new("s".into());
        m.apply_update(&sub_asst("2026-06-05T10:00:00.000Z"));
        assert_eq!(m.agent("sub").unwrap().status, AgentStatus::Running);

        // The `<task-notification>` is the authoritative terminal report.
        m.apply_update(&notif);
        assert_eq!(m.agent("sub").unwrap().status, AgentStatus::Stopped);

        // Terminal — even later activity (an out-of-order fold) can't revive it.
        m.apply_update(&sub_asst("2026-06-05T10:06:00.000Z"));
        assert_eq!(m.agent("sub").unwrap().status, AgentStatus::Stopped);

        // And it is NOT a user prompt — the era spine stays clean.
        assert!(
            m.prompts.is_empty(),
            "a task-notification must not pollute the prompt spine"
        );
    }

    #[test]
    fn meta_stopped_by_user_does_not_terminate_an_active_agent() {
        // `stoppedByUser` is a FINAL-outcome flag, but the meta folds at the
        // agent's FIRST activity — applying it there would strand the agent
        // `Stopped` for the whole replay (and prune its chips). Only the
        // timestamped `<task-notification>` terminates it.
        let mut m = SessionModel::new("s".into());
        let meta = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag".into()),
            stopped_by_user: Some(true),
        };
        m.apply_meta("sub", None, &meta);
        m.apply_update(&Record::Entry {
            source: Source::Sub("sub".into()),
            entry: entry(
                r#"{"type":"assistant","uuid":"u","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"b","name":"Bash","input":{}}]}}"#,
            ),
        });
        assert_eq!(
            m.agent("sub").unwrap().status,
            AgentStatus::Running,
            "an active agent must not be Stopped by the static meta flag"
        );
    }

    fn assistant_tool_use(source: Source, tool_id: &str, name: &str) -> Record {
        let line = format!(
            r#"{{"type":"assistant","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T13:51:15.151Z","message":{{"role":"assistant","model":"claude-opus-4-8","content":[{{"type":"tool_use","id":"{tool_id}","name":"{name}","input":{{"command":"ls -la"}}}}],"usage":{{"output_tokens":42}}}}}}"#
        );
        Record::Entry {
            source,
            entry: entry(&line),
        }
    }

    /// Shuffle invariance: the model's final state must be a pure function of
    /// the SET of observed facts, independent of cross-source arrival order
    /// (within-source order is preserved — that is what reality guarantees:
    /// each file is tailed in file order, but files interleave arbitrarily).
    ///
    /// This is the enforcement test for the order-independence invariant; it
    /// would have caught the lost-completion and journal-before-agent bugs.
    ///
    /// It shuffles *facts*, not Claude records: the streams below are what the
    /// Claude provider states for its files, and the fold under test is the one
    /// every provider reaches. A second provider proves itself the same way, by
    /// supplying its own streams to this interleaver.
    #[test]
    fn final_state_is_arrival_order_invariant() {
        // Per-source streams of facts (internal order preserved by the
        // interleaver), as the provider states them for each file.
        fn streams() -> Vec<Vec<Fact>> {
            records()
                .into_iter()
                .map(|recs| recs.iter().flat_map(Record::facts).collect())
                .collect()
        }

        fn records() -> Vec<Vec<Record>> {
            let meta = |agent_id: &str, wf: Option<&str>, tid: Option<&str>| Record::Meta {
                agent_id: agent_id.into(),
                workflow: wf.map(String::from),
                meta: crate::provider::claude::wire::SubagentMeta {
                    agent_type: Some("guide".into()),
                    description: None,
                    tool_use_id: tid.map(String::from),
                    stopped_by_user: None,
                },
            };
            let journal_result = |agent_id: &str| Record::Entry {
                source: Source::Ledger("wf_1".into()),
                entry: entry(&format!(
                    r#"{{"type":"result","key":"v2:k","agentId":"{agent_id}","result":{{"ok":true}}}}"#
                )),
            };
            vec![
                // Main transcript: spawn two agents, one completes ok, one err.
                vec![
                    assistant_tool_use(Source::Main, "ag_ok", "Agent"),
                    assistant_tool_use(Source::Main, "ag_err", "Agent"),
                    tool_result(Source::Main, "ag_ok", false),
                    tool_result(Source::Main, "ag_err", true),
                ],
                // Each meta is its own arrival (dir scans are independent).
                vec![meta("sub_ok", None, Some("ag_ok"))],
                vec![meta("sub_err", None, Some("ag_err"))],
                vec![meta("wfsub", Some("wf_1"), None)],
                // The workflow subagent's own activity.
                vec![
                    assistant_tool_use(Source::Sub("wfsub".into()), "w1", "Bash"),
                    tool_result(Source::Sub("wfsub".into()), "w1", false),
                ],
                // The journal completing it.
                vec![journal_result("wfsub")],
            ]
        }

        let baseline = crate::provider::harness::assert_order_invariant(streams);
        // Semantic anchors: every completion must have landed.
        assert!(
            baseline
                .iter()
                .any(|s| s.starts_with("sub_ok|") && s.contains("Done"))
        );
        assert!(
            baseline
                .iter()
                .any(|s| s.starts_with("sub_err|") && s.contains("Failed"))
        );
        assert!(
            baseline
                .iter()
                .any(|s| s.starts_with("wfsub|") && s.contains("Done"))
        );
        assert!(
            baseline
                .iter()
                .any(|s| s.starts_with("wf_1|") && s.contains("Done"))
        );
    }

    #[test]
    fn workflow_rollup_reverts_when_running_child_appears_late() {
        // Children are discovered incrementally and unordered: a group that
        // rolled up to Done from its first (already-finished) child must
        // revert to Running when a still-running sibling is discovered.
        let mut m = SessionModel::new("s1".into());

        // Child A arrives already completed (journal result first).
        m.apply_update(&Record::Entry {
            source: Source::Ledger("wf_1".into()),
            entry: entry(r#"{"type":"result","key":"v2:k","agentId":"childA","result":{}}"#),
        });
        let meta_a = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("workflow-subagent".into()),
            description: None,
            tool_use_id: None,
            stopped_by_user: None,
        };
        m.apply_meta("childA", Some("wf_1"), &meta_a);
        m.recompute_group_status();
        assert_eq!(m.agent("wf_1").unwrap().status, AgentStatus::Done);

        // Child B (still running) is discovered later: the group must revert.
        let meta_b = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("workflow-subagent".into()),
            description: None,
            tool_use_id: None,
            stopped_by_user: None,
        };
        m.apply_meta("childB", Some("wf_1"), &meta_b);
        m.recompute_group_status();
        assert_eq!(
            m.agent("wf_1").unwrap().status,
            AgentStatus::Running,
            "premature rollup must revert for a late-discovered running child"
        );
    }

    #[test]
    fn token_sum_saturates_instead_of_overflowing() {
        let mut m = SessionModel::new("s1".into());
        // Two turns with distinct requestIds, each claiming u64::MAX tokens.
        for (req, uid) in [("r1", "u1"), ("r2", "u2")] {
            let line = format!(
                r#"{{"type":"assistant","uuid":"{uid}","parentUuid":null,"requestId":"{req}","message":{{"role":"assistant","content":[],"usage":{{"output_tokens":{}}}}}}}"#,
                u64::MAX
            );
            m.apply_update(&Record::Entry {
                source: Source::Main,
                entry: entry(&line),
            });
        }
        assert_eq!(m.agent(MAIN_ID).unwrap().output_tokens, u64::MAX);
    }

    #[test]
    fn fork_liveness_is_activity_derived() {
        let mut m = SessionModel::new("s1".into());
        // A fork sidechain: agentType "fork", no toolUseId, no journal.
        let meta = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("fork".into()),
            description: Some("yess".into()),
            tool_use_id: None,
            stopped_by_user: None,
        };
        m.apply_meta("ayess-123", None, &meta);

        // The fork acts at 13:00.
        m.apply_update(&Record::Entry {
            source: Source::Sub("ayess-123".into()),
            entry: entry(
                r#"{"type":"assistant","uuid":"f1","parentUuid":null,"timestamp":"2026-06-07T13:00:00.000Z","message":{"role":"assistant","content":[]}}"#,
            ),
        });
        m.recompute_liveness(None);
        assert_eq!(m.agent("ayess-123").unwrap().status, AgentStatus::Running);

        // The session moves on without it (main activity 3.5 min later):
        // the silent fork is shown done.
        m.apply_update(&Record::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"user","uuid":"u9","parentUuid":null,"origin":{"kind":"human"},"timestamp":"2026-06-07T13:03:30.000Z","message":{"role":"user","content":"hi"}}"#,
            ),
        });
        m.recompute_liveness(None);
        assert_eq!(m.agent("ayess-123").unwrap().status, AgentStatus::Idle);

        // The user returns to the fork (a USER entry — exercises the
        // owner-touch fix): it resurrects.
        m.apply_update(&Record::Entry {
            source: Source::Sub("ayess-123".into()),
            entry: entry(
                r#"{"type":"user","uuid":"f2","parentUuid":"f1","timestamp":"2026-06-07T13:04:00.000Z","message":{"role":"user","content":"more"}}"#,
            ),
        });
        m.recompute_liveness(None);
        assert_eq!(m.agent("ayess-123").unwrap().status, AgentStatus::Running);

        // Live mode passes the wall clock: long-quiet fork goes idle.
        m.recompute_liveness(Some("2026-06-07T13:30:00.000Z".parse().unwrap()));
        assert_eq!(m.agent("ayess-123").unwrap().status, AgentStatus::Idle);

        // Spawned (non-interactive) subagents are untouched by the derivation.
        let spawned = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("t1".into()),
            stopped_by_user: None,
        };
        m.apply_meta("sub1", None, &spawned);
        m.recompute_liveness(Some("2026-06-07T14:00:00.000Z".parse().unwrap()));
        assert_eq!(
            m.agent("sub1").unwrap().status,
            AgentStatus::Running,
            "evidence-based agents must not be idled by silence"
        );
    }

    #[test]
    fn prompt_log_and_era_attribution() {
        let mut m = SessionModel::new("s1".into());
        let prompt = |uid: &str, ts: &str, text: &str| Record::Entry {
            source: Source::Main,
            entry: entry(&format!(
                r#"{{"type":"user","uuid":"{uid}","parentUuid":null,"origin":{{"kind":"human"}},"timestamp":"{ts}","message":{{"role":"user","content":"{text}"}}}}"#
            )),
        };
        m.apply_update(&prompt("p1", "2026-06-07T10:00:00.000Z", "first task"));
        m.apply_update(&prompt("p2", "2026-06-07T11:00:00.000Z", "second task"));
        assert_eq!(m.prompts.len(), 2);
        assert_eq!(m.prompts[0].excerpt, "first task");

        // Idempotent: re-applying the same entry doesn't duplicate.
        m.apply_update(&prompt("p2", "2026-06-07T11:00:00.000Z", "second task"));
        assert_eq!(m.prompts.len(), 2);

        // Order-independent: re-applying an EARLIER (non-trailing) entry is still
        // a dup — the facts layer must hold under out-of-order replay.
        m.apply_update(&prompt("p1", "2026-06-07T10:00:00.000Z", "first task"));
        assert_eq!(
            m.prompts.len(),
            2,
            "non-trailing re-apply must not duplicate"
        );

        // Era derivation is a pure function of timestamps — works for any
        // source (main or subagent tool calls alike).
        let ts = |s: &str| Some(s.parse().unwrap());
        assert_eq!(m.prompt_for_ts(ts("2026-06-07T10:30:00.000Z")), Some(0));
        assert_eq!(m.prompt_for_ts(ts("2026-06-07T12:00:00.000Z")), Some(1));
        assert_eq!(m.prompt_for_ts(ts("2026-06-07T09:00:00.000Z")), None);
        assert_eq!(m.prompt_for_ts(None), None);
    }

    #[test]
    fn provenance_links_spawn_to_prompt_and_reasoning() {
        let mut m = SessionModel::new("s1".into());

        // The human asks for something.
        m.apply_update(&Record::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"user","uuid":"u1","parentUuid":null,"origin":{"kind":"human"},"timestamp":"2026-06-07T10:00:00.000Z","message":{"role":"user","content":"please research the flag handling"}}"#,
            ),
        });
        // The assistant explains, then spawns an agent in the same message.
        m.apply_update(&Record::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"assistant","uuid":"u2","parentUuid":"u1","timestamp":"2026-06-07T10:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"I will spawn a guide to research this."},{"type":"tool_use","id":"ag1","name":"Agent","input":{"description":"research","subagent_type":"guide","prompt":"go"}}]}}"#,
            ),
        });
        let meta = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag1".into()),
            stopped_by_user: None,
        };
        m.apply_meta("sub1", None, &meta);

        let ctx = m
            .provenance(m.agent("sub1").unwrap())
            .expect("spawned agent has provenance");
        assert_eq!(
            m.provenance_prompt(ctx),
            Some("please research the flag handling")
        );
        assert_eq!(
            ctx.reasoning.as_deref(),
            Some("I will spawn a guide to research this.")
        );

        // Non-spawning tools record nothing; agents without a spawn link have
        // no provenance.
        assert!(m.provenance(m.agent(MAIN_ID).unwrap()).is_none());

        // Cross-line reasoning: text on an EARLIER line (turns span multiple
        // JSONL lines), spawn on a later line with no text of its own.
        m.apply_update(&Record::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"assistant","uuid":"u3","parentUuid":"u2","timestamp":"2026-06-07T10:01:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Now a second agent for the docs."}]}}"#,
            ),
        });
        m.apply_update(&Record::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"assistant","uuid":"u4","parentUuid":"u3","timestamp":"2026-06-07T10:01:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"ag2","name":"Agent","input":{"description":"docs","subagent_type":"guide","prompt":"go"}}]}}"#,
            ),
        });
        let meta2 = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag2".into()),
            stopped_by_user: None,
        };
        m.apply_meta("sub2", None, &meta2);
        assert_eq!(
            m.provenance(m.agent("sub2").unwrap())
                .unwrap()
                .reasoning
                .as_deref(),
            Some("Now a second agent for the docs.")
        );
    }

    #[test]
    fn provenance_reasoning_from_prior_thinking_line() {
        let mut m = SessionModel::new("s1".into());
        m.apply_update(&Record::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"assistant","uuid":"t1","parentUuid":null,"timestamp":"2026-06-07T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"The user wants the preferences file located.","signature":"x"}]}}"#,
            ),
        });
        m.apply_update(&Record::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"assistant","uuid":"t2","parentUuid":"t1","timestamp":"2026-06-07T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"agx","name":"Agent","input":{"description":"find prefs","subagent_type":"guide","prompt":"go"}}]}}"#,
            ),
        });
        let meta = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("agx".into()),
            stopped_by_user: None,
        };
        m.apply_meta("subx", None, &meta);
        assert_eq!(
            m.provenance(m.agent("subx").unwrap())
                .unwrap()
                .reasoning
                .as_deref(),
            Some("The user wants the preferences file located.")
        );
    }

    #[test]
    fn meta_after_completion_marks_subagent_done() {
        // Live attach order: the WHOLE main transcript (spawn + completion)
        // applies before the directory scan delivers the subagent meta.
        let mut m = SessionModel::new("s1".into());
        m.apply_update(&assistant_tool_use(Source::Main, "ag1", "Agent"));
        m.apply_update(&tool_result(Source::Main, "ag1", false));

        let meta = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag1".into()),
            stopped_by_user: None,
        };
        m.apply_meta("sub1", None, &meta);
        assert_eq!(
            m.agent("sub1").unwrap().status,
            AgentStatus::Done,
            "completion that predates the meta must not be lost"
        );

        // Failed variant.
        m.apply_update(&assistant_tool_use(Source::Main, "ag2", "Agent"));
        m.apply_update(&tool_result(Source::Main, "ag2", true));
        let meta_err = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag2".into()),
            stopped_by_user: None,
        };
        m.apply_meta("sub2", None, &meta_err);
        assert_eq!(m.agent("sub2").unwrap().status, AgentStatus::Failed);

        // Still-pending spawn stays Running.
        m.apply_update(&assistant_tool_use(Source::Main, "ag3", "Agent"));
        let meta_pending = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag3".into()),
            stopped_by_user: None,
        };
        m.apply_meta("sub3", None, &meta_pending);
        assert_eq!(m.agent("sub3").unwrap().status, AgentStatus::Running);
    }

    #[test]
    fn last_active_agent_follows_latest_timestamp() {
        let mut m = SessionModel::new("s".into());
        // Main is seeded; give it activity at T1.
        m.apply_update(&assistant_tool_use(Source::Main, "t1", "Bash"));

        // A subagent spawns but has no timestamped activity yet: the
        // timestamped main still wins (None < Some).
        let meta = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("t1".into()),
            stopped_by_user: None,
        };
        m.apply_meta("sub1", None, &meta);
        assert_eq!(m.last_active_agent_id().as_deref(), Some(MAIN_ID));

        // The subagent acts later: it becomes the active one.
        if let Some(a) = m.agents.get_mut("sub1") {
            a.last_ts = Some("2026-06-05T13:52:00.000Z".parse().unwrap());
        }
        assert_eq!(m.last_active_agent_id().as_deref(), Some("sub1"));

        // Main acts again, later still.
        if let Some(a) = m.agents.get_mut(MAIN_ID) {
            a.last_ts = Some("2026-06-05T13:53:00.000Z".parse().unwrap());
        }
        assert_eq!(m.last_active_agent_id().as_deref(), Some(MAIN_ID));
    }

    fn tool_result(source: Source, tool_id: &str, is_error: bool) -> Record {
        let err = if is_error { r#","is_error":true"# } else { "" };
        let line = format!(
            r#"{{"type":"user","uuid":"u2","parentUuid":"u1","timestamp":"2026-06-05T13:51:16.000Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tool_id}","content":"done"{err}}}]}}}}"#
        );
        Record::Entry {
            source,
            entry: entry(&line),
        }
    }

    #[test]
    fn main_seeded_running() {
        let m = SessionModel::new("s1".into());
        assert_eq!(m.agent_count(), 1);
        assert_eq!(m.agent(MAIN_ID).unwrap().status, AgentStatus::Running);
        assert_eq!(m.agent(MAIN_ID).unwrap().kind, AgentKind::Main);
    }

    #[test]
    fn tool_pairing_pending_then_ok() {
        let mut m = SessionModel::new("s1".into());
        m.apply_update(&assistant_tool_use(Source::Main, "t1", "Bash"));
        let main = m.agent(MAIN_ID).unwrap();
        assert_eq!(main.tool_calls.len(), 1);
        assert_eq!(main.tool_calls[0].state, ToolState::Pending);
        assert_eq!(main.tool_calls[0].summary.as_deref(), Some("ls -la"));
        assert_eq!(main.output_tokens, 42);
        assert_eq!(main.model.as_deref(), Some("claude-opus-4-8"));

        m.apply_update(&tool_result(Source::Main, "t1", false));
        let tc = &m.agent(MAIN_ID).unwrap().tool_calls[0];
        assert_eq!(tc.state, ToolState::Ok);
        // The result's timestamp is recorded as the finish time → duration
        // (`None` for `now` proves a completed tool uses `end_ts`, not the clock).
        assert_eq!(
            tc.duration(None).map(|d| d.num_milliseconds()),
            Some(849),
            "duration = result ts − use ts (13:51:16.000 − 13:51:15.151)"
        );
    }

    #[test]
    fn tool_duration_ticks_live_while_pending() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let pending = ToolCallInfo {
            id: "b".into(),
            name: "Bash".into(),
            summary: None,
            ts: Some(t("2026-06-05T10:00:00.000Z")),
            end_ts: None,
            state: ToolState::Pending,
        };
        // Pending → measured against `now` (the live tick).
        assert_eq!(
            pending
                .duration(Some(t("2026-06-05T10:00:05.000Z")))
                .map(|d| d.num_seconds()),
            Some(5)
        );
        // Pending with no playhead yet → None; no start ts → None.
        assert!(pending.duration(None).is_none());
        let no_start = ToolCallInfo {
            id: "c".into(),
            name: "x".into(),
            summary: None,
            ts: None,
            end_ts: None,
            state: ToolState::Pending,
        };
        assert!(
            no_start
                .duration(Some(t("2026-06-05T10:00:00.000Z")))
                .is_none()
        );
    }

    #[test]
    fn tool_pairing_error() {
        let mut m = SessionModel::new("s1".into());
        m.apply_update(&assistant_tool_use(Source::Main, "t1", "Bash"));
        m.apply_update(&tool_result(Source::Main, "t1", true));
        assert_eq!(
            m.agent(MAIN_ID).unwrap().tool_calls[0].state,
            ToolState::Err
        );
    }

    #[test]
    fn direct_subagent_spawn_running_done() {
        let mut m = SessionModel::new("s1".into());
        // meta introduces the subagent (structural), parented to main, spawned
        // by tool use "ag1".
        let meta = SubagentMeta {
            agent_type: Some("guide".into()),
            description: Some("do research".into()),
            tool_use_id: Some("ag1".into()),
            stopped_by_user: None,
        };
        let structural = m.apply_meta("abc123", None, &meta);
        assert!(structural);
        let a = m.agent("abc123").unwrap();
        assert_eq!(a.kind, AgentKind::Subagent);
        assert_eq!(a.parent.as_deref(), Some("main"));
        assert_eq!(a.status, AgentStatus::Running);
        assert_eq!(a.spawned_by_tool_use.as_deref(), Some("ag1"));

        // Re-applying the same meta is NOT structural.
        assert!(!m.apply_meta("abc123", None, &meta));

        // The main transcript's tool_result for ag1 completes the subagent.
        m.apply_update(&tool_result(Source::Main, "ag1", false));
        assert_eq!(m.agent("abc123").unwrap().status, AgentStatus::Done);
    }

    #[test]
    fn direct_subagent_failed() {
        let mut m = SessionModel::new("s1".into());
        let meta = SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag1".into()),
            stopped_by_user: None,
        };
        m.apply_meta("abc123", None, &meta);
        m.apply_update(&tool_result(Source::Main, "ag1", true));
        assert_eq!(m.agent("abc123").unwrap().status, AgentStatus::Failed);
    }

    /// The workflow group takes its name from the launch's `toolUseResult`
    /// (`runId` == the group id), whichever order the two facts arrive in —
    /// the launch and the group's first subagent meta can fold either way round.
    #[test]
    fn workflow_group_takes_its_name_from_the_launch_in_either_order() {
        const LAUNCH: &str = r#"{"type":"user","uuid":"u","timestamp":"2026-06-05T10:00:00.000Z","toolUseResult":{"status":"async_launched","taskType":"local_workflow","workflowName":"code-review","runId":"wf-99","summary":"one finder per angle"}}"#;
        let meta = SubagentMeta {
            agent_type: Some("workflow-subagent".into()),
            description: None,
            tool_use_id: None,
            stopped_by_user: None,
        };

        // Launch first, then the subagent meta creates the group.
        let mut a = SessionModel::new("s1".into());
        a.apply_update(&Record::Entry {
            source: Source::Main,
            entry: entry(LAUNCH),
        });
        a.apply_meta("wfsub1", Some("wf-99"), &meta);

        // Meta first, then the launch labels the existing group.
        let mut b = SessionModel::new("s1".into());
        b.apply_meta("wfsub1", Some("wf-99"), &meta);
        b.apply_update(&Record::Entry {
            source: Source::Main,
            entry: entry(LAUNCH),
        });

        for (name, m) in [("launch-first", &a), ("meta-first", &b)] {
            let group = m.agent("wf-99").expect("group exists");
            assert_eq!(group.kind, AgentKind::Group, "{name}");
            assert_eq!(
                group.agent_type.as_deref(),
                Some("code-review"),
                "{name}: group is labelled with the workflow name, not the fallback"
            );
            assert_eq!(
                group.description.as_deref(),
                Some("one finder per angle"),
                "{name}"
            );
            // The subagent keeps its own type — the label is the group's alone.
            assert_eq!(
                m.agent("wfsub1").unwrap().agent_type.as_deref(),
                Some("workflow-subagent"),
                "{name}"
            );
        }
    }

    /// A launch alone must not fabricate a group: the node is created by the
    /// subagent metas (i.e. by the `workflows/<id>/` directory actually existing),
    /// so an unrelated workflow ack never adds an empty, parentless node.
    #[test]
    fn workflow_launch_alone_creates_no_group_node() {
        let mut m = SessionModel::new("s1".into());
        m.apply_update(&Record::Entry {
            source: Source::Main,
            entry: entry(r#"{"type":"user","uuid":"u","timestamp":"2026-06-05T10:00:00.000Z","toolUseResult":{"taskType":"local_workflow","workflowName":"code-review","runId":"wf-99"}}"#),
        });
        assert!(m.agent("wf-99").is_none(), "no group without its directory");
    }

    #[test]
    fn workflow_group_and_journal_completion() {
        let mut m = SessionModel::new("s1".into());
        let meta = SubagentMeta {
            agent_type: Some("workflow-subagent".into()),
            description: None,
            tool_use_id: None,
            stopped_by_user: None,
        };
        let structural = m.apply_meta("wfsub1", Some("wf-99"), &meta);
        assert!(structural);
        // Group node created, parented to main.
        let group = m.agent("wf-99").unwrap();
        assert_eq!(group.kind, AgentKind::Group);
        assert_eq!(group.parent.as_deref(), Some("main"));
        // Subagent parented to the group.
        assert_eq!(m.agent("wfsub1").unwrap().parent.as_deref(), Some("wf-99"));
        assert_eq!(m.agent("wfsub1").unwrap().status, AgentStatus::Running);

        // A journal `result` for wfsub1 marks it done.
        let line = r#"{"type":"result","key":"k","agentId":"wfsub1","result":{"ok":true}}"#;
        m.apply_update(&Record::Entry {
            source: Source::Ledger("wf-99".into()),
            entry: entry(line),
        });
        assert_eq!(m.agent("wfsub1").unwrap().status, AgentStatus::Done);
    }

    #[test]
    fn workflow_group_rolls_up_from_children() {
        let mut m = SessionModel::new("s1".into());
        let meta = SubagentMeta {
            agent_type: Some("workflow-subagent".into()),
            description: None,
            tool_use_id: None,
            stopped_by_user: None,
        };
        m.apply_meta("c1", Some("wf-1"), &meta);
        m.apply_meta("c2", Some("wf-1"), &meta);

        // Both children running → group stays running.
        m.recompute_group_status();
        assert_eq!(m.agent("wf-1").unwrap().status, AgentStatus::Running);

        // One child done, one still running → group still running.
        m.agents.get_mut("c1").unwrap().status = AgentStatus::Done;
        m.recompute_group_status();
        assert_eq!(m.agent("wf-1").unwrap().status, AgentStatus::Running);

        // All children terminal (one failed) → group Failed.
        m.agents.get_mut("c2").unwrap().status = AgentStatus::Failed;
        m.recompute_group_status();
        assert_eq!(m.agent("wf-1").unwrap().status, AgentStatus::Failed);
    }

    #[test]
    fn workflow_group_all_done_is_done() {
        let mut m = SessionModel::new("s1".into());
        let meta = SubagentMeta {
            agent_type: Some("workflow-subagent".into()),
            description: None,
            tool_use_id: None,
            stopped_by_user: None,
        };
        m.apply_meta("c1", Some("wf-1"), &meta);
        m.agents.get_mut("c1").unwrap().status = AgentStatus::Done;
        m.recompute_group_status();
        assert_eq!(m.agent("wf-1").unwrap().status, AgentStatus::Done);
    }

    #[test]
    fn subagent_file_activity_creates_node() {
        let mut m = SessionModel::new("s1".into());
        // An assistant turn arrives in a subagent file before its meta.
        let structural =
            m.apply_update(&assistant_tool_use(Source::Sub("zz9".into()), "t1", "Read"));
        assert!(structural);
        let a = m.agent("zz9").unwrap();
        assert_eq!(a.kind, AgentKind::Subagent);
        assert_eq!(a.tool_calls.len(), 1);
    }

    /// An assistant turn line carrying a `requestId` and a fixed
    /// `usage.output_tokens` (no tool_use), to model a multi-line turn.
    fn assistant_turn(source: Source, request_id: &str, out_tokens: u64) -> Record {
        let line = format!(
            r#"{{"type":"assistant","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T13:51:15.151Z","requestId":"{request_id}","message":{{"role":"assistant","model":"claude-opus-4-8","content":[{{"type":"text","text":"hi"}}],"usage":{{"output_tokens":{out_tokens}}}}}}}"#
        );
        Record::Entry {
            source,
            entry: entry(&line),
        }
    }

    #[test]
    fn output_tokens_counted_once_per_request_id() {
        // Claude Code emits the same requestId across multiple lines of one
        // turn, each repeating the cumulative usage. We must count it once.
        let mut m = SessionModel::new("s1".into());
        m.apply_update(&assistant_turn(Source::Main, "req_A", 418));
        m.apply_update(&assistant_turn(Source::Main, "req_A", 418));
        m.apply_update(&assistant_turn(Source::Main, "req_A", 418));
        assert_eq!(m.agent(MAIN_ID).unwrap().output_tokens, 418);

        // A new turn (different requestId) adds its own tokens.
        m.apply_update(&assistant_turn(Source::Main, "req_B", 100));
        m.apply_update(&assistant_turn(Source::Main, "req_B", 100));
        assert_eq!(m.agent(MAIN_ID).unwrap().output_tokens, 518);
    }

    #[test]
    fn output_tokens_without_request_id_sum_per_line() {
        // Defensive fallback: lines lacking a requestId can't be deduped.
        let mut m = SessionModel::new("s1".into());
        m.apply_update(&assistant_tool_use(Source::Main, "t1", "Bash")); // 42, no requestId
        m.apply_update(&assistant_tool_use(Source::Main, "t2", "Bash")); // 42, no requestId
        assert_eq!(m.agent(MAIN_ID).unwrap().output_tokens, 84);
    }

    #[test]
    fn duplicate_tool_use_not_double_counted() {
        let mut m = SessionModel::new("s1".into());
        let u = assistant_tool_use(Source::Main, "t1", "Bash");
        m.apply_update(&u);
        m.apply_update(&u);
        assert_eq!(m.agent(MAIN_ID).unwrap().tool_calls.len(), 1);
    }

    #[test]
    fn end_of_stream_idles_interactive_agents_only() {
        let mut m = SessionModel::new("s1".into());
        let spawned = crate::provider::claude::wire::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("t1".into()),
            stopped_by_user: None,
        };
        m.apply_meta("sub1", None, &spawned);
        m.end_of_stream();
        // Interactive agents can't complete — they just go quiet (Idle).
        assert_eq!(m.agent(MAIN_ID).unwrap().status, AgentStatus::Idle);
        // A subagent still Running when the recording ends has finished (its
        // async spawn-ack was superseded by its own activity, or it never got a
        // reliable completion) — settle it to Done rather than leave it "live".
        assert_eq!(m.agent("sub1").unwrap().status, AgentStatus::Done);
    }
}
