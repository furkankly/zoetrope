//! `SessionModel`: the pure domain model — agents, their statuses, and tool
//! calls, derived solely from parsed transcript updates. No rataflow types
//! here; the graph layer ([`crate::state::graph`]) projects this onto a `Flow`.
//!
//! Node ids are stable strings: `"main"` for the root agent, the 17-hex
//! `agentId` for direct/workflow subagents, and the workflow id for a workflow
//! group node. Spawn order is tracked explicitly so layout and navigation are
//! deterministic.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::tailer::{Source, Update};
use crate::transcript::{
    AgentToolInput, ContentBlock, Entry, LedgerEntry, SubagentMeta, UserContent, UserContentBlock,
};

/// Stable node id of the main (root) agent.
pub const MAIN_ID: &str = "main";

/// Silence window after which an interactive sidechain (fork) is shown as
/// done. Forks are human-paced — lulls while the user types are normal — so
/// this is minutes, not the main agent's seconds-scale file-growth window.
const INTERACTIVE_IDLE_SECS: i64 = 120;

/// The full derived view of a session.
pub struct SessionModel {
    pub session_id: String,
    /// Agents keyed by stable node id (`"main"`, `agentId`, or `wf-id`).
    pub agents: BTreeMap<String, AgentInfo>,
    /// Stable spawn order of node ids (insertion order). Drives layout/nav.
    pub spawn_order: Vec<String>,
    /// Most recent activity timestamp seen across all files.
    pub last_activity: Option<DateTime<Utc>>,
    /// `runId → (workflowName, summary)` from main-transcript workflow launches,
    /// kept as a FACT rather than applied on arrival: the launch and the group's
    /// first subagent meta can fold in either order, so both sides consult this.
    workflow_labels: BTreeMap<String, (Option<String>, Option<String>)>,
    /// Completion facts, kept so model state is a function of the fact SET,
    /// not of arrival order — a completion can arrive before its target
    /// exists (live attach applies the main transcript before directory scans
    /// deliver metas; replay merges many files). `tool_use_id → (is_err, ack_ts)`
    /// for every main-transcript tool_result. The timestamp matters because a
    /// PARALLEL subagent's `Agent` result is an immediate spawn-ack (ms after the
    /// call), NOT its completion — so it only completes the subagent when not
    /// superseded by the subagent's own later activity (see
    /// [`resolve_spawn_status`](Self::resolve_spawn_status)).
    completed_spawns: HashMap<String, (bool, Option<DateTime<Utc>>)>,
    /// Agent ids named done by workflow-journal `result` entries.
    journal_done: HashSet<String>,
    /// Authoritative terminal status per agent id, from an async
    /// `<task-notification>`. Recorded order-independently (the notification may
    /// arrive before the agent exists) and applied by
    /// [`resolve_spawn_status`](Self::resolve_spawn_status), where it OUTRANKS the
    /// spawn-ack and time-derived liveness — the only real completion report the
    /// async format gives us.
    task_terminal: HashMap<String, AgentStatus>,
    /// Provenance facts: why each spawned agent exists, keyed by the spawning
    /// `tool_use_id` (order-independent, like the completion stores). Captured
    /// at the spawning call in the main transcript; joined to the agent via
    /// `spawned_by_tool_use` at render time.
    spawn_context: HashMap<String, SpawnContext>,
    /// Every plain user prompt in the main transcript, in order — the
    /// session's spine. Tool calls and spawns attribute to a prompt era via
    /// [`Self::prompt_for_ts`] (timestamp-derived, order-independent).
    pub prompts: Vec<PromptInfo>,
    /// Excerpt of the most recent assistant text in the main transcript.
    /// One logical turn spans several JSONL lines, so the reasoning for a
    /// spawn usually lives on an EARLIER line than the tool_use — this is the
    /// cross-line fallback for [`SpawnContext::reasoning`].
    last_main_text: Option<String>,
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone, Default)]
pub struct SpawnContext {
    /// Timestamp of the spawning `tool_use` entry.
    pub ts: Option<DateTime<Utc>>,
    /// Excerpt of the assistant's reasoning right before the spawn.
    pub reasoning: Option<String>,
}

/// Distinguishes the three kinds of node the model produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// The root agent (`"main"`).
    Main,
    /// A direct or workflow subagent.
    Subagent,
    /// A group node representing a workflow run.
    WorkflowGroup,
}

/// Lifecycle status of an agent. Drives node color and edge animation.
///
/// The states encode what we can truthfully claim. Spawned agents (direct
/// subagents, workflow subagents, groups) have completion evidence in the
/// format and use `Running`/`Done`/`Failed`. Interactive agents (main, forks)
/// have NO end marker in the format — completion is unknowable — so they use
/// `Running`/`Idle`: "entries within the idle window" vs "silent" — both
/// statements of fact, neither a claim of completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    /// Interactive agent with no recent activity (never claims completion).
    Idle,
    Done,
    Failed,
    /// A background agent the user stopped mid-run — from an async
    /// `<task-notification>` with `<status>stopped</status>`. Terminal, but NOT
    /// a success — kept distinct from `Done` so a reviewer sees it was cut short.
    /// (`meta.stoppedByUser` reports the same outcome but is intentionally inert;
    /// see the NOTE in [`apply_meta`](SessionModel::apply_meta).)
    Stopped,
}

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

/// The English status word, from raw status + interactivity. The localized
/// entry point is [`crate::i18n::Locale::status_word`]; this is its `En` arm,
/// kept here next to the vocabulary it belongs to. Interactive agents say
/// "active" (we know of recent entries, not that a task executes), spawned say
/// "running". A free fn so `AgentNode` (no `AgentInfo`) shares it too.
pub fn status_word_en(status: AgentStatus, interactive: bool) -> &'static str {
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
#[derive(Debug, Clone)]
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
    pub tool_calls: Vec<ToolCallInfo>,
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
            tool_calls: Vec::new(),
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

    /// The status word for display, in `locale` — delegates to
    /// [`crate::i18n::Locale::status_word`] so cards (`AgentNode`, which has
    /// no `AgentInfo`) share the exact wording.
    pub fn status_word(&self, locale: crate::i18n::Locale) -> &'static str {
        locale.status_word(self.status, self.interactive)
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
        let mut agents = BTreeMap::new();
        agents.insert(MAIN_ID.to_string(), AgentInfo::new(AgentKind::Main));
        SessionModel {
            session_id,
            agents,
            spawn_order: vec![MAIN_ID.to_string()],
            last_activity: None,
            workflow_labels: BTreeMap::new(),
            completed_spawns: HashMap::new(),
            task_terminal: HashMap::new(),
            journal_done: HashSet::new(),
            spawn_context: HashMap::new(),
            prompts: Vec::new(),
            last_main_text: None,
        }
    }

    /// Ensure an agent of `kind` exists under `id`, returning whether it was
    /// newly created (a structural change).
    fn ensure_agent(&mut self, id: &str, kind: AgentKind) -> bool {
        if self.agents.contains_key(id) {
            return false;
        }
        let mut info = AgentInfo::new(kind);
        // A journal `result` naming this agent may have arrived first.
        if self.journal_done.contains(id) {
            info.status = AgentStatus::Done;
            info.terminal = true;
        }
        self.agents.insert(id.to_string(), info);
        self.spawn_order.push(id.to_string());
        true
    }

    /// Copy a recorded workflow label onto its group node, if both are known.
    /// Idempotent, and never overwrites a value already set. Returns whether the
    /// title changed (a structural change — the node relabels).
    fn label_workflow_group(&mut self, run_id: &str) -> bool {
        let Some((name, summary)) = self.workflow_labels.get(run_id).cloned() else {
            return false;
        };
        let Some(group) = self.agents.get_mut(run_id) else {
            return false;
        };
        let mut changed = false;
        if group.agent_type.is_none() && name.is_some() {
            group.agent_type = name;
            changed = true;
        }
        if group.description.is_none() && summary.is_some() {
            group.description = summary;
        }
        changed
    }

    /// Record a workflow launch (`toolUseResult.taskType == "local_workflow"`)
    /// and label its group if that node already exists. Only records the fact
    /// when the group is absent — the group is created by the subagent metas
    /// (from the `workflows/<run_id>/` directory), so a launch alone never
    /// fabricates an empty, parentless node.
    fn apply_workflow_launch(&mut self, wf: &crate::transcript::WorkflowLaunch) -> bool {
        self.workflow_labels
            .insert(wf.run_id.clone(), (wf.name.clone(), wf.summary.clone()));
        self.label_workflow_group(&wf.run_id)
    }

    fn note_activity(&mut self, ts: Option<DateTime<Utc>>) {
        if let Some(ts) = ts
            && self.last_activity.is_none_or(|l| ts > l)
        {
            self.last_activity = Some(ts);
        }
    }

    /// Fold a single [`Update`] into the model, mutating agents, statuses, and
    /// tool calls. Defensive: unknown/irrelevant updates are no-ops.
    ///
    /// Returns `true` if this update changed graph *structure* (an agent node
    /// or parent edge appeared), so the caller can mark layout dirty.
    pub fn apply_update(&mut self, update: &Update) -> bool {
        match update {
            Update::Entry { source, entry } => self.apply_entry(source, entry),
            Update::SubagentMeta {
                agent_id,
                workflow,
                meta,
            } => self.apply_meta(agent_id, workflow.as_deref(), meta),
        }
    }

    /// Digest one parsed transcript entry from a specific source file.
    fn apply_entry(&mut self, source: &Source, entry: &Entry) -> bool {
        match entry {
            Entry::Assistant(e) => {
                let target = match source {
                    Source::Main => MAIN_ID.to_string(),
                    Source::Sub(id) => id.clone(),
                    // Journal files carry no assistant turns.
                    Source::Journal(_) => return false,
                };
                self.note_activity(e.envelope.timestamp);
                let mut structural = false;
                // Subagent files reference an agent we may not have meta for
                // yet — make sure the node exists so its activity is visible.
                if let Source::Sub(id) = source {
                    structural |= self.ensure_agent(id, AgentKind::Subagent);
                }
                self.apply_assistant(&target, e);
                // The subagent just produced activity — re-resolve its spawn ack
                // (its `last_ts` may now supersede an immediate spawn-ack).
                if let Source::Sub(id) = source {
                    self.resolve_spawn_status(id);
                }
                structural
            }
            Entry::User(e) => {
                self.note_activity(e.envelope.timestamp);
                // User entries are activity of their owner too (tool results,
                // fork prompts) — without this an agent's `last_ts` lags at
                // its last assistant turn, skewing activity-derived liveness.
                let mut structural = false;
                if let Source::Sub(id) = source {
                    structural |= self.ensure_agent(id, AgentKind::Subagent);
                }
                let owner = match source {
                    Source::Main => Some(MAIN_ID),
                    Source::Sub(id) => Some(id.as_str()),
                    Source::Journal(_) => None,
                };
                if let Some(agent) = owner.and_then(|id| self.agents.get_mut(id)) {
                    agent.touch_ts(e.envelope.timestamp);
                }
                // A workflow launch names its group node (`runId` is the group id).
                if matches!(source, Source::Main)
                    && let Some(wf) = e.workflow_launch()
                {
                    structural |= self.apply_workflow_launch(&wf);
                }
                // Main-thread user strings come in three flavours: an async
                // agent's `<task-notification>` (a terminal report — apply it),
                // other system-injected text (background-stop notices, etc.), and
                // a genuine human prompt. Only the last is an era boundary, gated
                // on `origin.kind == "human"`; the rest stay off the prompt spine.
                if matches!(source, Source::Main)
                    && let Some(text) = e.prompt_text()
                {
                    if let Some(tn) = crate::transcript::parse_task_notification(text) {
                        self.apply_task_notification(&tn);
                    } else if e.is_human_prompt() {
                        let ex = excerpt(text);
                        let ts = e.envelope.timestamp;
                        // Idempotent AND order-independent: a re-applied entry is a
                        // dup wherever it lands, not only when it's the trailing
                        // prompt (the facts layer must hold under out-of-order
                        // replay). A genuine repeat at a *different* ts is kept.
                        let dup = self.prompts.iter().any(|p| p.excerpt == ex && p.ts == ts);
                        if !dup {
                            self.prompts.push(PromptInfo { excerpt: ex, ts });
                        }
                    }
                }
                // tool_result blocks complete tool calls (and, in the main
                // transcript, direct-subagent + workflow nodes).
                if let Some(msg) = &e.message
                    && let Some(UserContent::Blocks(blocks)) = &msg.content
                {
                    for block in blocks {
                        if let UserContentBlock::ToolResult(r) = block
                            && let Some(tid) = &r.tool_use_id
                        {
                            let is_err = r.is_error == Some(true);
                            self.complete_tool(source, tid, is_err, e.envelope.timestamp);
                        }
                    }
                }
                // The subagent just produced activity — re-resolve its spawn ack
                // (its `last_ts` may now supersede an immediate spawn-ack).
                if let Source::Sub(id) = source {
                    self.resolve_spawn_status(id);
                }
                structural
            }
            Entry::Result(ledger) => {
                // In a workflow journal, a `result` entry marks completion of
                // the workflow subagent it names.
                if let Source::Journal(_) = source {
                    self.complete_journal_result(ledger);
                }
                false
            }
            // System, attachment, flat metadata, started, unknown: no graph
            // effect.
            _ => false,
        }
    }

    /// Apply an assistant entry to the agent identified by `target` id.
    fn apply_assistant(&mut self, target: &str, e: &crate::transcript::AssistantEntry) {
        let ts = e.envelope.timestamp;
        let Some(agent) = self.agents.get_mut(target) else {
            return;
        };
        agent.touch_ts(ts);
        if let Some(msg) = &e.message {
            if agent.model.is_none()
                && let Some(model) = &msg.model
            {
                agent.model = Some(model.clone());
            }
            if let Some(usage) = &msg.usage
                && let Some(out) = usage.output_tokens
            {
                // One assistant turn spans multiple lines that each repeat the
                // same cumulative usage; count it once per `requestId`. Lines
                // with no `requestId` can't be deduped, so they sum per line.
                match &e.envelope.request_id {
                    Some(req) if !agent.seen_request_ids.insert(req.clone()) => {}
                    // Saturating: counts come from untrusted transcript
                    // content; overflow must not panic (debug) or wrap.
                    _ => agent.output_tokens = agent.output_tokens.saturating_add(out),
                }
            }
            // Walk blocks in order: the text block nearest above a spawning
            // tool_use is the assistant's stated reason for the spawn.
            let mut last_text: Option<&str> = None;
            let mut spawns: Vec<(String, Option<String>)> = Vec::new();
            for block in &msg.content {
                // Thinking counts as reasoning too — spawns are often preceded
                // only by a thinking block (verified on real transcripts), and
                // the panel label is literally "thought".
                match block {
                    ContentBlock::Text { text } if !text.trim().is_empty() => {
                        last_text = Some(text);
                    }
                    ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                        last_text = Some(thinking);
                    }
                    _ => {}
                }
                if let ContentBlock::ToolUse(tu) = block {
                    let Some(id) = &tu.id else { continue };
                    let name = tu.name.clone().unwrap_or_default();
                    if crate::transcript::is_spawn_tool(&name) {
                        spawns.push((id.clone(), last_text.map(excerpt)));
                    }
                    // Avoid duplicating a tool call already recorded (idempotent
                    // re-application of the same batch).
                    if agent.tool_index.contains_key(id) {
                        continue;
                    }
                    let summary = summarize_tool(&name, &tu.input, e.envelope.cwd.as_deref());
                    agent.tool_index.insert(id.clone(), agent.tool_calls.len());
                    agent.tool_calls.push(ToolCallInfo {
                        id: id.clone(),
                        name,
                        summary,
                        ts,
                        end_ts: None,
                        state: ToolState::Pending,
                    });
                }
            }
            // Record provenance after the agent borrow ends (fact store —
            // idempotent on re-application, order-independent for the join).
            // Reasoning: same-message text wins; else fall back to the last
            // main-transcript assistant text (turns span multiple lines).
            let cross_line = (target == MAIN_ID)
                .then(|| self.last_main_text.clone())
                .flatten();
            for (id, reasoning) in spawns {
                let reasoning = reasoning.or_else(|| cross_line.clone());
                self.spawn_context
                    .entry(id)
                    .or_insert_with(|| SpawnContext { ts, reasoning });
            }
            if target == MAIN_ID
                && let Some(t) = last_text
            {
                self.last_main_text = Some(excerpt(t));
            }
        }
    }

    /// Complete a tool call by id within the agent owning the originating file,
    /// flipping its state and — if the tool spawned a subagent — completing
    /// that subagent too.
    fn complete_tool(
        &mut self,
        source: &Source,
        tool_use_id: &str,
        is_err: bool,
        ack_ts: Option<DateTime<Utc>>,
    ) {
        let owner = match source {
            Source::Main => MAIN_ID.to_string(),
            Source::Sub(id) => id.clone(),
            Source::Journal(_) => return,
        };
        let new_state = if is_err {
            ToolState::Err
        } else {
            ToolState::Ok
        };
        if let Some(agent) = self.agents.get_mut(&owner)
            && let Some(&i) = agent.tool_index.get(tool_use_id)
        {
            agent.tool_calls[i].state = new_state;
            // The result's timestamp is the tool's finish time → its duration.
            agent.tool_calls[i].end_ts = ack_ts;
        }
        // A tool_result in the *main* transcript may complete a subagent spawned
        // by that tool_use id. Record the fact (with its timestamp) first — the
        // spawned agent may not exist yet (arrival order is unguaranteed) — then
        // resolve any agent already present. `resolve_spawn_status` decides
        // whether this ack is a real completion or a premature spawn-ack.
        if let Source::Main = source {
            self.completed_spawns
                .insert(tool_use_id.to_string(), (is_err, ack_ts));
            let ids: Vec<String> = self
                .agents
                .iter()
                .filter(|(_, a)| a.spawned_by_tool_use.as_deref() == Some(tool_use_id))
                .map(|(id, _)| id.clone())
                .collect();
            for id in ids {
                self.resolve_spawn_status(&id);
            }
        }
    }

    /// Record an async agent's terminal report and apply it. Order-independent:
    /// if the agent isn't folded yet, the status is stored and applied when it
    /// appears (its activity re-runs `resolve_spawn_status`).
    fn apply_task_notification(&mut self, tn: &crate::transcript::TaskNotification) {
        use crate::transcript::TaskStatus;
        let status = match tn.status {
            TaskStatus::Completed => AgentStatus::Done,
            TaskStatus::Stopped => AgentStatus::Stopped,
            TaskStatus::Failed => AgentStatus::Failed,
            // Unknown status string — don't override derived liveness.
            TaskStatus::Other => return,
        };
        self.task_terminal.insert(tn.agent_id.clone(), status);
        self.resolve_spawn_status(&tn.agent_id);
    }

    /// Set a direct subagent's status from its spawn ack, honoring the fact that
    /// a PARALLEL subagent's `Agent` result lands milliseconds after the call (a
    /// spawn-ack), while the subagent then runs for minutes. The ack completes it
    /// only when NOT superseded by the subagent's own later activity (its
    /// `last_ts` is after the ack); otherwise it's still `Running` (settled to
    /// `Done` at [`end_of_stream`](Self::end_of_stream)). A pure function of the
    /// folded facts — `last_ts` and the recorded ack — so it is order-invariant.
    fn resolve_spawn_status(&mut self, id: &str) {
        // An async `<task-notification>` is the real terminal report — it
        // outranks the spawn-ack and the time-derived fallback. Applied here so
        // any later activity fold can't revive it.
        if let Some(&status) = self.task_terminal.get(id)
            && let Some(agent) = self.agents.get_mut(id)
        {
            agent.status = status;
            agent.terminal = true;
            return;
        }
        let Some(agent) = self.agents.get(id) else {
            return;
        };
        let Some(tid) = agent.spawned_by_tool_use.clone() else {
            return;
        };
        let Some(&(is_err, ack_ts)) = self.completed_spawns.get(&tid) else {
            return;
        };
        let superseded = matches!((agent.last_ts, ack_ts), (Some(l), Some(a)) if l > a);
        let agent = self.agents.get_mut(id).unwrap();
        if superseded {
            // Async agent: the ack was a spawn handle, not a completion. Hand it
            // to time-derived liveness (not terminal).
            agent.status = AgentStatus::Running;
            agent.terminal = false;
        } else {
            // The ack IS the completion (sync subagent, or no own activity).
            agent.status = if is_err {
                AgentStatus::Failed
            } else {
                AgentStatus::Done
            };
            agent.terminal = true;
        }
    }

    /// Roll workflow group nodes up from their children.
    ///
    /// A `WorkflowGroup` has no direct completion signal in the transcript (its
    /// own `Workflow` tool_use id is not the workflow id used to key the node),
    /// so the DESIGN fallback is "all-children-done": when every subagent under
    /// a workflow is terminal, the group is terminal too — `Failed` if any child
    /// failed, else `Done`. A childless workflow stays `Running`.
    ///
    /// Fully re-derived on every call — deliberately NOT monotonic: children
    /// are discovered incrementally and in no guaranteed order, so a group
    /// that rolled up to `Done` (all known children terminal) must revert to
    /// `Running` when a still-running child is discovered later. Derived state
    /// is a pure function of the current fact set, never of rollup history.
    pub fn recompute_workflow_status(&mut self) {
        let wf_ids: Vec<String> = self
            .agents
            .iter()
            .filter(|(_, a)| a.kind == AgentKind::WorkflowGroup)
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

    /// Complete a workflow subagent named by a journal `result` ledger entry.
    fn complete_journal_result(&mut self, ledger: &LedgerEntry) {
        let Some(agent_id) = &ledger.agent_id else {
            return;
        };
        // Record the fact — the agent may not exist yet (journal entries carry
        // no timestamps, so in replay they can arrive arbitrarily early);
        // `ensure_agent` consults `journal_done` on creation.
        self.journal_done.insert(agent_id.clone());
        if let Some(agent) = self.agents.get_mut(agent_id) {
            // A journal result is a RELIABLE completion (dated to the agent's
            // last entry): mark success and pin it terminal so time-derived
            // liveness won't revive it. Failures still surface via the main
            // transcript tool_result when present.
            if agent.status == AgentStatus::Running {
                agent.status = AgentStatus::Done;
            }
            agent.terminal = true;
        }
    }

    /// Fold a discovered subagent `meta.json` into the model.
    ///
    /// Returns `true` if this introduced a new agent node (structural change).
    pub fn apply_meta(
        &mut self,
        agent_id: &str,
        workflow: Option<&str>,
        meta: &SubagentMeta,
    ) -> bool {
        let mut structural = false;
        // Workflow subagents live under a group node; ensure it exists first.
        let parent = match workflow {
            Some(wf_id) => {
                structural |= self.ensure_agent(wf_id, AgentKind::WorkflowGroup);
                if let Some(group) = self.agents.get_mut(wf_id)
                    && group.parent.is_none()
                {
                    group.parent = Some(MAIN_ID.to_string());
                }
                // The launch may have folded already — take its label now.
                structural |= self.label_workflow_group(wf_id);
                wf_id.to_string()
            }
            None => MAIN_ID.to_string(),
        };

        structural |= self.ensure_agent(agent_id, AgentKind::Subagent);

        if let Some(agent) = self.agents.get_mut(agent_id) {
            if agent.parent.is_none() {
                agent.parent = Some(parent);
            }
            if let Some(t) = &meta.agent_type {
                // Category decided here, structurally — forks are interactive
                // sidechains (verified: no completion marker in the format).
                if t == "fork" {
                    agent.interactive = true;
                }
                agent.agent_type = Some(t.clone());
            }
            if let Some(d) = &meta.description
                && agent.description.is_none()
            {
                agent.description = Some(d.clone());
            }
            if let Some(tid) = &meta.tool_use_id
                && agent.spawned_by_tool_use.is_none()
            {
                agent.spawned_by_tool_use = Some(tid.clone());
            }
        }
        // NOTE: `meta.stopped_by_user` is deliberately NOT applied here. The
        // sidecar records the agent's FINAL outcome, but the meta folds at the
        // agent's FIRST activity (it's dated there) — applying it would mark the
        // agent `Stopped` for the entire replay, before it has done anything. The
        // timestamped `<task-notification>` is the correct terminal signal; a
        // subagent with no notification falls back to time-derived liveness.
        // Order independence: the spawning tool call may have completed before
        // this meta was seen — its completion fact survives in `completed_spawns`
        // regardless of arrival order. Now that the `tool_use_id` link is set,
        // resolve the status from that fact (honoring the immediate-ack rule, and
        // any recorded task-notification, which outranks it).
        self.resolve_spawn_status(agent_id);
        structural
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
        let mut changed = false;
        for agent in self.agents.values_mut() {
            let Some(ts) = agent.last_ts else {
                continue;
            };
            // An unresolved tool_call is direct evidence the agent is still
            // working — stronger than "no transcript line for 120s". Without it,
            // an agent blocked on a long tool (a 2-minute Bash) looks quiet and
            // settles to Done/Idle mid-tool, then snaps back when the result
            // lands. A reliably-terminal agent short-circuits below, so this
            // can't revive a genuinely finished one.
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
                // Async agent still producing activity — and REVERSIBLE: a long
                // gap (e.g. a subagent running `cargo test`) that settled it to
                // Done flips straight back to Running when it resumes.
                AgentStatus::Running
            } else {
                AgentStatus::Done
            };
            changed |= agent.status != next;
            agent.status = next;
        }
        changed
    }

    /// Mark the end of a finite stream (replay finished): every interactive
    /// agent goes `Idle` — the recording is over, nothing is active, and
    /// completion remains unclaimable.
    pub fn end_of_stream(&mut self) {
        for agent in self.agents.values_mut() {
            if agent.is_interactive() {
                // Interactive agents (main/forks) never "complete" — the stream
                // ending just means they went quiet.
                agent.status = AgentStatus::Idle;
            } else if agent.status == AgentStatus::Running {
                // A subagent still Running at the recording's end has finished
                // (its async spawn-ack Done was superseded by its own later
                // activity via `resolve_spawn_status`; now there's no more).
                agent.status = AgentStatus::Done;
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
                if crate::transcript::is_spawn_tool(&tc.name) {
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

/// Derive a short one-line summary from a tool_use input, if a natural field
/// exists for the tool. Defensive: any shape that doesn't match yields `None`.
fn summarize_tool(name: &str, input: &serde_json::Value, cwd: Option<&str>) -> Option<String> {
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
        n if crate::transcript::is_spawn_tool(n) => {
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

/// Upper bound on a stored tool summary. Generous on purpose: the detail panel
/// truncates to its (often wide) width at render time, so this only caps
/// pathological inputs. The node cards don't render summaries, so it is NOT a
/// card-width constraint — capping tighter here just starved the panel.
const SUMMARY_MAX: usize = 200;

/// Collapse whitespace and truncate a summary to [`SUMMARY_MAX`].
fn truncate_summary(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = SUMMARY_MAX;
    if flat.chars().count() > MAX {
        let truncated: String = flat.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    } else {
        flat
    }
}

/// A file path, made readable for the panel: relative to `cwd` when it lives
/// under the project root, and truncated keeping the BASENAME (not the root) if
/// it's still long — `…/state/timeline.rs`, never `/Users/.../src/sta…`.
fn short_path(path: &str, cwd: Option<&str>) -> String {
    let rel = cwd
        .and_then(|c| path.strip_prefix(c).map(|r| (c, r)))
        // Only a match at a path-component boundary counts: without this a
        // SIBLING dir sharing the cwd as a string prefix is mangled
        // (cwd `…/zoetrope` + path `…/zoetrope-web/src/app.rs` → `-web/src/app.rs`).
        .filter(|(c, r)| r.starts_with('/') || c.ends_with('/'))
        .map(|(_, r)| r.trim_start_matches('/'))
        .filter(|r| !r.is_empty())
        .unwrap_or(path);
    const MAX: usize = SUMMARY_MAX;
    let n = rel.chars().count();
    if n <= MAX {
        return rel.to_string();
    }
    // Keep the tail (basename + nearest dirs) with a leading ellipsis.
    let tail: String = rel.chars().skip(n - (MAX - 1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tailer::{Source, Update};
    use crate::transcript::parse_line;

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
        m.prompts.push(PromptInfo {
            excerpt: "review the codebase".into(),
            ts: Some(ts("2026-06-05T10:00:00Z")),
        });
        let main = m.agents.get_mut(MAIN_ID).unwrap();
        main.tool_calls.push(tool(
            "s1",
            "Agent",
            "hunt bugs",
            "2026-06-05T10:05:00Z",
            None,
            ToolState::Ok,
        ));
        // A slow Bash: started 10:10, failed (result) at 10:12.
        main.tool_calls.push(tool(
            "b1",
            "Bash",
            "cargo test",
            "2026-06-05T10:10:00Z",
            Some("2026-06-05T10:12:00Z"),
            ToolState::Err,
        ));
        // A later SUCCESSFUL non-spawn tool is not a log event.
        main.tool_calls.push(tool(
            "r1",
            "Read",
            "src/lib.rs",
            "2026-06-05T10:15:00Z",
            None,
            ToolState::Ok,
        ));

        // No subagent is loaded here, so the `Agent` call ("s1") is the fallback
        // spawn marker (at call time).
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
            .push(ToolCallInfo {
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
        let sub_asst = |ts: &str| Update::Entry {
            source: Source::Sub("sub".into()),
            entry: entry(&format!(
                r#"{{"type":"assistant","uuid":"u","parentUuid":null,"timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"b1","name":"Bash","input":{{}}}}]}}}}"#
            )),
        };
        let sub_result = |ts: &str| Update::Entry {
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
        let sub_asst = |ts: &str| Update::Entry {
            source: Source::Sub("sub".into()),
            entry: entry(&format!(
                r#"{{"type":"assistant","uuid":"u","parentUuid":null,"timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"b1","name":"Bash","input":{{}}}}]}}}}"#
            )),
        };
        let sub_result = |ts: &str| Update::Entry {
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
        let asst = |src: Source, id: &str, name: &str, ts: &str| Update::Entry {
            source: src,
            entry: entry(&format!(
                r#"{{"type":"assistant","uuid":"u","parentUuid":null,"timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}]}}}}"#
            )),
        };
        let result = |src: Source, tid: &str, ts: &str| Update::Entry {
            source: src,
            entry: entry(&format!(
                r#"{{"type":"user","uuid":"r","parentUuid":null,"timestamp":"{ts}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tid}"}}]}}}}"#
            )),
        };
        let meta = |agent: &str, tid: &str| Update::SubagentMeta {
            agent_id: agent.into(),
            workflow: None,
            meta: crate::transcript::SubagentMeta {
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
        let sub_asst = |ts: &str| Update::Entry {
            source: Source::Sub("sub".into()),
            entry: entry(&format!(
                r#"{{"type":"assistant","uuid":"u","parentUuid":null,"timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"b","name":"Bash","input":{{}}}}]}}}}"#
            )),
        };
        let notif = Update::Entry {
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
        let meta = crate::transcript::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag".into()),
            stopped_by_user: Some(true),
        };
        m.apply_meta("sub", None, &meta);
        m.apply_update(&Update::Entry {
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

    fn assistant_tool_use(source: Source, tool_id: &str, name: &str) -> Update {
        let line = format!(
            r#"{{"type":"assistant","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T13:51:15.151Z","message":{{"role":"assistant","model":"claude-opus-4-8","content":[{{"type":"tool_use","id":"{tool_id}","name":"{name}","input":{{"command":"ls -la"}}}}],"usage":{{"output_tokens":42}}}}}}"#
        );
        Update::Entry {
            source,
            entry: entry(&line),
        }
    }

    /// Shuffle invariance: the model's final state must be a pure function of
    /// the SET of observed facts, independent of cross-source arrival order
    /// (within-source order is preserved — that is what reality guarantees:
    /// each file is tailed in file order, but files interleave arbitrarily).
    ///
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

    #[test]
    fn short_path_strips_cwd_only_at_a_component_boundary() {
        assert_eq!(
            short_path(
                "/Users/me/projects/zoetrope/src/a.rs",
                Some("/Users/me/projects/zoetrope")
            ),
            "src/a.rs"
        );
        // A SIBLING dir sharing the cwd as a string prefix must NOT be mangled
        // into a fake relative path ("-web/src/a.rs").
        assert_eq!(
            short_path(
                "/Users/me/projects/zoetrope-web/src/a.rs",
                Some("/Users/me/projects/zoetrope")
            ),
            "/Users/me/projects/zoetrope-web/src/a.rs"
        );
        assert_eq!(short_path("/project/x.rs", Some("/proj")), "/project/x.rs");
        // A trailing-slash cwd still relativizes.
        assert_eq!(short_path("/proj/x.rs", Some("/proj/")), "x.rs");
        // cwd == path falls back to the absolute path (not an empty string).
        assert_eq!(short_path("/proj", Some("/proj")), "/proj");
    }

    /// This is the enforcement test for the order-independence invariant; it
    /// would have caught the lost-completion and journal-before-agent bugs.
    #[test]
    fn final_state_is_arrival_order_invariant() {
        // Per-source streams (internal order preserved by the interleaver).
        fn streams() -> Vec<Vec<Update>> {
            let meta = |agent_id: &str, wf: Option<&str>, tid: Option<&str>| Update::SubagentMeta {
                agent_id: agent_id.into(),
                workflow: wf.map(String::from),
                meta: crate::transcript::SubagentMeta {
                    agent_type: Some("guide".into()),
                    description: None,
                    tool_use_id: tid.map(String::from),
                    stopped_by_user: None,
                },
            };
            let journal_result = |agent_id: &str| Update::Entry {
                source: Source::Journal("wf_1".into()),
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

        // Comparable snapshot of the facts (spawn_order is presentation —
        // creation order legitimately varies — so compare by sorted id).
        fn snapshot(m: &SessionModel) -> Vec<String> {
            m.agents
                .iter()
                .map(|(id, a)| {
                    let tools: Vec<String> = a
                        .tool_calls
                        .iter()
                        .map(|t| format!("{}:{:?}", t.id, t.state))
                        .collect();
                    let prov = m
                        .provenance(a)
                        .map(|c| format!("{:?}/{:?}", m.provenance_prompt(c), c.reasoning))
                        .unwrap_or_default();
                    format!(
                        "{id}|{:?}|{:?}|{:?}|{:?}|{}|{prov}",
                        a.kind,
                        a.status,
                        a.parent,
                        a.spawned_by_tool_use,
                        tools.join(",")
                    )
                })
                .collect()
        }

        // Deterministic LCG so failures are reproducible.
        let interleave = |mut seed: u64| -> SessionModel {
            let mut queues = streams();
            let mut m = SessionModel::new("s".into());
            while queues.iter().any(|q| !q.is_empty()) {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let nonempty: Vec<usize> = (0..queues.len())
                    .filter(|&i| !queues[i].is_empty())
                    .collect();
                let pick = nonempty[(seed >> 33) as usize % nonempty.len()];
                let update = queues[pick].remove(0);
                m.apply_update(&update);
            }
            m.recompute_workflow_status();
            m
        };

        let baseline = snapshot(&interleave(0));
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

        for seed in 1..40u64 {
            let got = snapshot(&interleave(seed));
            assert_eq!(got, baseline, "order-dependent state at seed {seed}");
        }
    }

    #[test]
    fn workflow_rollup_reverts_when_running_child_appears_late() {
        // Children are discovered incrementally and unordered: a group that
        // rolled up to Done from its first (already-finished) child must
        // revert to Running when a still-running sibling is discovered.
        let mut m = SessionModel::new("s1".into());

        // Child A arrives already completed (journal result first).
        m.apply_update(&Update::Entry {
            source: Source::Journal("wf_1".into()),
            entry: entry(r#"{"type":"result","key":"v2:k","agentId":"childA","result":{}}"#),
        });
        let meta_a = crate::transcript::SubagentMeta {
            agent_type: Some("workflow-subagent".into()),
            description: None,
            tool_use_id: None,
            stopped_by_user: None,
        };
        m.apply_meta("childA", Some("wf_1"), &meta_a);
        m.recompute_workflow_status();
        assert_eq!(m.agent("wf_1").unwrap().status, AgentStatus::Done);

        // Child B (still running) is discovered later: the group must revert.
        let meta_b = crate::transcript::SubagentMeta {
            agent_type: Some("workflow-subagent".into()),
            description: None,
            tool_use_id: None,
            stopped_by_user: None,
        };
        m.apply_meta("childB", Some("wf_1"), &meta_b);
        m.recompute_workflow_status();
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
            m.apply_update(&Update::Entry {
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
        let meta = crate::transcript::SubagentMeta {
            agent_type: Some("fork".into()),
            description: Some("yess".into()),
            tool_use_id: None,
            stopped_by_user: None,
        };
        m.apply_meta("ayess-123", None, &meta);

        // The fork acts at 13:00.
        m.apply_update(&Update::Entry {
            source: Source::Sub("ayess-123".into()),
            entry: entry(
                r#"{"type":"assistant","uuid":"f1","parentUuid":null,"timestamp":"2026-06-07T13:00:00.000Z","message":{"role":"assistant","content":[]}}"#,
            ),
        });
        m.recompute_liveness(None);
        assert_eq!(m.agent("ayess-123").unwrap().status, AgentStatus::Running);

        // The session moves on without it (main activity 3.5 min later):
        // the silent fork is shown done.
        m.apply_update(&Update::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"user","uuid":"u9","parentUuid":null,"origin":{"kind":"human"},"timestamp":"2026-06-07T13:03:30.000Z","message":{"role":"user","content":"hi"}}"#,
            ),
        });
        m.recompute_liveness(None);
        assert_eq!(m.agent("ayess-123").unwrap().status, AgentStatus::Idle);

        // The user returns to the fork (a USER entry — exercises the
        // owner-touch fix): it resurrects.
        m.apply_update(&Update::Entry {
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
        let spawned = crate::transcript::SubagentMeta {
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
        let prompt = |uid: &str, ts: &str, text: &str| Update::Entry {
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
        m.apply_update(&Update::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"user","uuid":"u1","parentUuid":null,"origin":{"kind":"human"},"timestamp":"2026-06-07T10:00:00.000Z","message":{"role":"user","content":"please research the flag handling"}}"#,
            ),
        });
        // The assistant explains, then spawns an agent in the same message.
        m.apply_update(&Update::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"assistant","uuid":"u2","parentUuid":"u1","timestamp":"2026-06-07T10:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"I will spawn a guide to research this."},{"type":"tool_use","id":"ag1","name":"Agent","input":{"description":"research","subagent_type":"guide","prompt":"go"}}]}}"#,
            ),
        });
        let meta = crate::transcript::SubagentMeta {
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
        m.apply_update(&Update::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"assistant","uuid":"u3","parentUuid":"u2","timestamp":"2026-06-07T10:01:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Now a second agent for the docs."}]}}"#,
            ),
        });
        m.apply_update(&Update::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"assistant","uuid":"u4","parentUuid":"u3","timestamp":"2026-06-07T10:01:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"ag2","name":"Agent","input":{"description":"docs","subagent_type":"guide","prompt":"go"}}]}}"#,
            ),
        });
        let meta2 = crate::transcript::SubagentMeta {
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
        m.apply_update(&Update::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"assistant","uuid":"t1","parentUuid":null,"timestamp":"2026-06-07T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"The user wants the preferences file located.","signature":"x"}]}}"#,
            ),
        });
        m.apply_update(&Update::Entry {
            source: Source::Main,
            entry: entry(
                r#"{"type":"assistant","uuid":"t2","parentUuid":"t1","timestamp":"2026-06-07T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"agx","name":"Agent","input":{"description":"find prefs","subagent_type":"guide","prompt":"go"}}]}}"#,
            ),
        });
        let meta = crate::transcript::SubagentMeta {
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

        let meta = crate::transcript::SubagentMeta {
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
        let meta_err = crate::transcript::SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag2".into()),
            stopped_by_user: None,
        };
        m.apply_meta("sub2", None, &meta_err);
        assert_eq!(m.agent("sub2").unwrap().status, AgentStatus::Failed);

        // Still-pending spawn stays Running.
        m.apply_update(&assistant_tool_use(Source::Main, "ag3", "Agent"));
        let meta_pending = crate::transcript::SubagentMeta {
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
        let meta = crate::transcript::SubagentMeta {
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

    fn tool_result(source: Source, tool_id: &str, is_error: bool) -> Update {
        let err = if is_error { r#","is_error":true"# } else { "" };
        let line = format!(
            r#"{{"type":"user","uuid":"u2","parentUuid":"u1","timestamp":"2026-06-05T13:51:16.000Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tool_id}","content":"done"{err}}}]}}}}"#
        );
        Update::Entry {
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
        a.apply_update(&Update::Entry {
            source: Source::Main,
            entry: entry(LAUNCH),
        });
        a.apply_meta("wfsub1", Some("wf-99"), &meta);

        // Meta first, then the launch labels the existing group.
        let mut b = SessionModel::new("s1".into());
        b.apply_meta("wfsub1", Some("wf-99"), &meta);
        b.apply_update(&Update::Entry {
            source: Source::Main,
            entry: entry(LAUNCH),
        });

        for (name, m) in [("launch-first", &a), ("meta-first", &b)] {
            let group = m.agent("wf-99").expect("group exists");
            assert_eq!(group.kind, AgentKind::WorkflowGroup, "{name}");
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
        m.apply_update(&Update::Entry {
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
        assert_eq!(group.kind, AgentKind::WorkflowGroup);
        assert_eq!(group.parent.as_deref(), Some("main"));
        // Subagent parented to the group.
        assert_eq!(m.agent("wfsub1").unwrap().parent.as_deref(), Some("wf-99"));
        assert_eq!(m.agent("wfsub1").unwrap().status, AgentStatus::Running);

        // A journal `result` for wfsub1 marks it done.
        let line = r#"{"type":"result","key":"k","agentId":"wfsub1","result":{"ok":true}}"#;
        m.apply_update(&Update::Entry {
            source: Source::Journal("wf-99".into()),
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
        m.recompute_workflow_status();
        assert_eq!(m.agent("wf-1").unwrap().status, AgentStatus::Running);

        // One child done, one still running → group still running.
        m.agents.get_mut("c1").unwrap().status = AgentStatus::Done;
        m.recompute_workflow_status();
        assert_eq!(m.agent("wf-1").unwrap().status, AgentStatus::Running);

        // All children terminal (one failed) → group Failed.
        m.agents.get_mut("c2").unwrap().status = AgentStatus::Failed;
        m.recompute_workflow_status();
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
        m.recompute_workflow_status();
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
    fn assistant_turn(source: Source, request_id: &str, out_tokens: u64) -> Update {
        let line = format!(
            r#"{{"type":"assistant","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T13:51:15.151Z","requestId":"{request_id}","message":{{"role":"assistant","model":"claude-opus-4-8","content":[{{"type":"text","text":"hi"}}],"usage":{{"output_tokens":{out_tokens}}}}}}}"#
        );
        Update::Entry {
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
        let spawned = crate::transcript::SubagentMeta {
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
