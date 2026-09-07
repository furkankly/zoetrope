//! The provider boundary: what a transcript *says*, stripped of how it said it.
//!
//! A provider turns its own wire format (Claude's JSONL entries, Codex's
//! rollout records) into a stream of [`Fact`]s. [`SessionModel`] folds facts and
//! knows nothing about either format. The rule that keeps this honest, and the
//! reason the vocabulary is this small:
//!
//! > A provider states what its records contain. The core decides what it means
//! > when nothing was recorded.
//!
//! So a provider never concludes. It does not decide an agent is done because a
//! tool result came back (that is a [`FactKind::ToolEnd`]; whether it doubles
//! as a spawn acknowledgement is a join the core makes, and the core knows an
//! acknowledgement is not a completion); it does not invent an outcome for a
//! record that carries none (it emits no `ToolEnd`, and the call stays pending,
//! which is ground truth that the agent is working). Reasoning about absence
//! lives in one place, `SessionModel`, and survives both formats.
//!
//! Every fact carries the same envelope: which agent it is about and when. That
//! is what lets the fold track activity once instead of per record kind, and
//! what lets the timeline date undated facts (see `tailer::item::Timing`) without
//! knowing any format. Facts travel in [`Statement`]s, one per record read:
//! the record's own time places it on the timeline, each fact's time feeds
//! the fold.
//!
//! [`SessionModel`]: crate::state::session::SessionModel

use chrono::{DateTime, Utc};

/// Stable node id: `"main"` for the root, else whatever the provider keys its
/// agents by (Claude's 17-hex `agentId`, Codex's thread id).
pub type AgentId = String;

/// A tool invocation id, the join key between its start and its end.
pub type CallId = String;

/// One statement about the session, with the envelope every fact shares.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    /// The agent this fact is about. `None` only for session-level metadata.
    pub agent: Option<AgentId>,
    /// When it happened. `None` for records the format leaves undated; the
    /// timeline dates those relative to their agent's dated neighbours.
    pub ts: Option<DateTime<Utc>>,
    pub kind: FactKind,
}

/// What a fact says. Named after what the record *is*, not what it implies.
#[derive(Debug, Clone, PartialEq)]
pub enum FactKind {
    /// An agent exists. Creates the node. Idempotent: a second `Agent` for the
    /// same id only fills fields that are still empty.
    Agent {
        kind: AgentKind,
        parent: Option<AgentId>,
        agent_type: Option<String>,
        description: Option<String>,
        /// The tool call that spawned it, when the format records that join.
        spawned_by: Option<CallId>,
        /// Human-paced, with no completion signal anywhere in the format (the
        /// root agent, Claude's forks). Selects the `Running`/`Idle` liveness
        /// branch instead of `Running`/`Done`.
        interactive: bool,
    },
    /// The agent produced a record that says nothing else. Exists so activity
    /// tracking does not depend on which other facts a line happened to yield.
    Activity,
    /// Naming for an agent that may not exist yet. Decorates, never creates:
    /// a group is born from its first child, not from the record that names it.
    Label {
        agent_type: Option<String>,
        description: Option<String>,
    },
    /// The model the agent was observed running.
    Model(String),
    /// Output tokens. `dedup` is the key under which the format repeats one
    /// turn's usage across several records (Claude's `requestId`); `None` sums
    /// every fact as its own delta.
    Tokens { output: u64, dedup: Option<String> },
    /// A human prompt on this agent's thread. An era boundary. Providers emit it
    /// only for text a person typed; injected text is not a prompt.
    Prompt(String),
    /// Assistant text or thinking. The core keeps the latest per agent so a
    /// following spawn can carry its stated reason.
    Reasoning(String),
    /// A tool call began. `summary` is the provider's one-line rendering of the
    /// input (a command, a path), because what makes a good summary is a
    /// property of the tool vocabulary, which only the provider knows.
    ToolStart {
        call: CallId,
        name: String,
        summary: Option<String>,
    },
    /// A tool call ended with an observed outcome. Emitted only from a record
    /// that carries the outcome; never synthesised from silence.
    ToolEnd { call: CallId, outcome: Outcome },
    /// This tool call spawns an agent. Lets the core record provenance for the
    /// child (which prompt era, which reasoning) before the child appears.
    Spawn { call: CallId },
    /// The agent's end was observed in the format. Authoritative; pins the
    /// agent against time-derived revival.
    Ended(AgentStatus),
    /// Untimed session metadata, rendered as a labelled row. Latest wins.
    Session { label: String, value: String },
    /// Untimed session metadata that counts occurrences.
    Tally(String),
    /// The session's display title.
    Title(String),
}

/// What one record stated, and when the record itself was written.
///
/// The record's time is where it sits on the timeline: the moment the file
/// said it, which is the only moment a live viewer could have known it. Each
/// fact keeps its own time for the fold, which may differ: a completion record
/// can also say when the call started, and the tool's duration must be right
/// while the timeline still shows the record where it landed.
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub at: Option<DateTime<Utc>>,
    pub facts: Vec<Fact>,
}

impl Statement {
    /// Whether this is session-level metadata in its entirety, and so belongs
    /// off the timeline.
    pub fn is_session_meta(&self) -> bool {
        !self.facts.is_empty() && self.facts.iter().all(Fact::is_session_meta)
    }

    /// Take the session-level metadata out, leaving the activity. Metadata
    /// belongs to the session whichever record carried it, and a record may
    /// carry both (a Codex root names itself and its app on one line).
    pub fn take_session_meta(&mut self) -> Vec<Fact> {
        let (meta, rest): (Vec<Fact>, Vec<Fact>) = std::mem::take(&mut self.facts)
            .into_iter()
            .partition(Fact::is_session_meta);
        self.facts = rest;
        meta
    }
}

impl From<Fact> for Statement {
    /// A single fact, stated at its own time.
    fn from(fact: Fact) -> Self {
        Statement {
            at: fact.ts,
            facts: vec![fact],
        }
    }
}

impl FactKind {
    /// The variant's name, for logs, goldens and anything that lists facts.
    pub fn name(&self) -> &'static str {
        match self {
            FactKind::Agent { .. } => "Agent",
            FactKind::Activity => "Activity",
            FactKind::Label { .. } => "Label",
            FactKind::Model(_) => "Model",
            FactKind::Tokens { .. } => "Tokens",
            FactKind::Prompt(_) => "Prompt",
            FactKind::Reasoning(_) => "Reasoning",
            FactKind::ToolStart { .. } => "ToolStart",
            FactKind::ToolEnd { .. } => "ToolEnd",
            FactKind::Spawn { .. } => "Spawn",
            FactKind::Ended(_) => "Ended",
            FactKind::Session { .. } => "Session",
            FactKind::Tally(_) => "Tally",
            FactKind::Title(_) => "Title",
        }
    }
}

/// How a tool call ended, as the format recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Err,
}

/// The three kinds of node the model produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// The root agent (`"main"`).
    Main,
    /// An agent spawned by another, with a transcript of its own.
    Subagent,
    /// A node with no ending of its own, whose status rolls up from its
    /// children. Claude's workflow runs are the only instance today.
    Group,
}

impl AgentKind {
    /// Display label when the provider recorded no `agent_type`. `Group`'s
    /// fallback is Claude's word for the only groups any format produces so
    /// far; a second kind of group would make it the provider's to state.
    pub fn default_label(self) -> &'static str {
        match self {
            AgentKind::Main => "agent",
            AgentKind::Subagent => "subagent",
            AgentKind::Group => "workflow",
        }
    }
}

/// Lifecycle status of an agent. Drives node colour and edge animation.
///
/// The states encode what we can truthfully claim. Spawned agents have
/// completion evidence in some formats and use `Running`/`Done`/`Failed`.
/// Interactive agents (main, forks) have no end marker anywhere, so they use
/// `Running`/`Idle`: "entries within the idle window" vs "silent", both
/// statements of fact, neither a claim of completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    /// Interactive agent with no recent activity. Never claims completion.
    Idle,
    Done,
    Failed,
    /// A background agent cut short by the user. Terminal but NOT a success;
    /// kept distinct from `Done` so a reviewer sees it did not finish.
    Stopped,
}

impl Fact {
    /// Whether this is session-level metadata that belongs off the timeline.
    pub fn is_session_meta(&self) -> bool {
        matches!(
            self.kind,
            FactKind::Session { .. } | FactKind::Tally(_) | FactKind::Title(_)
        )
    }
}
