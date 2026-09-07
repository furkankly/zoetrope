//! Portable replay-stream pieces — the timeline item and its ordering.
//!
//! Shared by the native replay assembly ([`super::replay`]) and the App's
//! `Timeline`, and free of any IO so it compiles on wasm too.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::fact::{Fact, FactKind, Statement};
use crate::provider::claude::{self, Source};

/// When a timeline item happens — its single source of truth for placement.
///
/// An item is either `Dated` (a real timestamp — its own envelope, an inherited
/// predecessor, or a resolved cross-file join) or **undated**, split into two
/// distinct cases:
///
/// - `Pending` — a fact about an agent whose true time lives on **another**
///   file (a subagent's birth from its sidecar, a workflow ledger's result).
///   It rides at the head until that agent's dated facts are discovered, then
///   [`date_and_sort`] promotes it to `Dated`. Because `Dated` is only ever
///   reached via a real join, the "fabricate a date then freeze it" bug is
///   unrepresentable.
/// - `Leader` — genuinely undated with no join target. Rides at the head
///   permanently.
#[derive(Debug, Clone)]
pub enum Timing {
    Dated(DateTime<Utc>),
    /// Undated, waiting on `agent`'s dated facts to appear (cross-file join).
    Pending(String),
    /// Undated with nothing to wait on.
    Leader,
}

/// One merged replay step: what one record stated, with its `Timing`.
///
/// The whole `Vec<ReplayItem>` is handed to the App via `UiEvent::ReplayLoaded`;
/// the App's `Timeline` owns it and folds a prefix up to the playhead.
#[derive(Debug)]
pub struct ReplayItem {
    pub(crate) timing: Timing,
    pub facts: Vec<Fact>,
}

impl ReplayItem {
    /// The resolved timestamp, if the item is dated — the value all the
    /// timeline geometry (folding, sorting, the scrubber) reads. `Pending` and
    /// `Leader` are both undated → `None` (they sort to the head).
    pub fn ts(&self) -> Option<DateTime<Utc>> {
        match self.timing {
            Timing::Dated(t) => Some(t),
            Timing::Pending(_) | Timing::Leader => None,
        }
    }

    /// Wrap a statement as a timeline item, placed where its record was
    /// written. An undated statement about an agent is `Pending` on that
    /// agent; one about nothing in particular leads.
    pub fn new(statement: Statement) -> Self {
        Self::at(statement.at, statement.facts)
    }

    /// Build an item from an explicit timestamp: `Some` → `Dated`, `None` → the
    /// undated case implied by the facts' envelopes.
    pub(crate) fn at(ts: Option<DateTime<Utc>>, facts: Vec<Fact>) -> Self {
        let timing = match ts {
            Some(t) => Timing::Dated(t),
            None => facts
                .iter()
                .find_map(|f| f.agent.clone())
                .map_or(Timing::Leader, Timing::Pending),
        };
        ReplayItem { timing, facts }
    }

    /// Whether any fact in this item is of the given shape.
    pub fn any(&self, pred: impl Fn(&Fact) -> bool) -> bool {
        self.facts.iter().any(pred)
    }
}

/// Date the untimed items in place — an agent's birth → its first dated fact,
/// an ending → its last (else earliest) — then stably sort the whole list by
/// timestamp. For the one-shot bulk replay assembly, where every file has
/// already been parsed so an unmatched ending is genuinely an orphan.
///
/// Ties sort births before other facts (so an agent exists before its first
/// activity folds); remaining `None` timestamps (true leaders) sort first.
/// Idempotent: a birth with no activity yet stays `None` and is re-dated once
/// its facts arrive on a later call.
pub(crate) fn date_and_sort(items: &mut [ReplayItem]) {
    date_and_sort_inner(items, true);
}

/// Like [`date_and_sort`], but for the growing live stream: an ending whose
/// agent has no dated facts YET stays undated — riding at the head like a
/// birth — so a later call re-dates it once the agent's transcript is
/// discovered. The bulk fallback to `earliest` would permanently stamp it with
/// the session START (a `Dated` item is never re-guessed), pinning e.g. a
/// workflow result hours before the workflow ran.
pub(crate) fn date_and_sort_live(items: &mut [ReplayItem]) {
    date_and_sort_inner(items, false);
}

/// Whether a fact is the agent's own output (as opposed to a statement about
/// it from elsewhere), and so evidence of when the agent was active.
fn is_by_agent(fact: &Fact) -> bool {
    !matches!(
        fact.kind,
        FactKind::Agent { .. } | FactKind::Label { .. } | FactKind::Ended(_)
    )
}

fn date_and_sort_inner(items: &mut [ReplayItem], complete: bool) {
    let earliest = items.iter().filter_map(|i| i.ts()).min();

    let mut first_ts: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut last_ts: HashMap<String, DateTime<Utc>> = HashMap::new();
    for item in items.iter() {
        for fact in item.facts.iter().filter(|f| is_by_agent(f)) {
            // A fact's own time is the more precise witness (a completion
            // record that also says when the call started); the record's
            // time is the fallback.
            if let (Some(ts), Some(id)) = (fact.ts.or(item.ts()), &fact.agent) {
                first_ts
                    .entry(id.clone())
                    .and_modify(|t| *t = (*t).min(ts))
                    .or_insert(ts);
                last_ts
                    .entry(id.clone())
                    .and_modify(|t| *t = (*t).max(ts))
                    .or_insert(ts);
            }
        }
    }
    for item in items.iter_mut() {
        // A `Dated` item is settled — only undated (`Pending`/`Leader`) items
        // try to resolve, so a resolved date can never be re-guessed.
        let Timing::Pending(agent) = &item.timing else {
            continue;
        };
        // The join rule (which edge of the agent's lifespan to borrow, and the
        // bulk-only orphan fallback) is keyed on what the record says.
        let ending = item
            .facts
            .iter()
            .any(|f| matches!(f.kind, FactKind::Ended(_)));
        let resolved = if ending {
            // Bulk only: an orphan ending (no matching transcript anywhere)
            // → earliest, so it folds with the start instead of leading as an
            // untimed item. Live keeps it undated so it can be re-dated once
            // the agent's file is discovered. (Births never take this
            // fallback — they stay `Pending` until their agent lands.)
            last_ts
                .get(agent)
                .copied()
                .or(if complete { earliest } else { None })
        } else {
            first_ts.get(agent).copied()
        };
        if let Some(t) = resolved {
            item.timing = Timing::Dated(t);
        }
    }

    let rank = |i: &ReplayItem| u8::from(!i.any(|f| matches!(f.kind, FactKind::Agent { .. })));
    items.sort_by(|a, b| match (a.ts(), b.ts()) {
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| rank(a).cmp(&rank(b))),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => std::cmp::Ordering::Equal,
    });
}

/// Build a replay stream from a single transcript's text — the browser
/// frontend's data source (a bundled or drag-dropped `.jsonl`).
///
/// Unlike the native `build_replay`, there are no sidecar files to discover, so
/// this parses only the main transcript: subagents appear only insofar as it
/// records them (their own transcripts live in separate files the browser can't
/// reach). Untimed session metadata is routed into [`SessionInfo`](crate::state::SessionInfo);
/// the rest is dated and stably sorted — same shape the App expects from
/// `UiEvent::ReplayLoaded`.
pub fn replay_from_jsonl(text: &str) -> (Vec<ReplayItem>, crate::state::SessionInfo) {
    let mut items: Vec<ReplayItem> = Vec::new();
    push_lines(text, &Source::Main, &mut items);
    finish(items)
}

/// One non-main file for [`replay_from_session`]: a subagent's transcript +
/// `meta.json`, or a workflow's `journal.jsonl`.
///
/// Mirrors what the native `build_replay` discovers on disk, so the browser
/// frontend (which has no filesystem — JS reads the files and hands the text
/// across) produces the same graph from the same session.
pub struct DemoSubagent<'a> {
    pub agent_id: &'a str,
    pub meta: &'a str,
    pub transcript: &'a str,
    /// Owning workflow id for anything under `subagents/workflows/<id>/`;
    /// `None` for a direct subagent. Drives the group node + parentage.
    pub workflow: Option<&'a str>,
    /// True when `transcript` is a workflow's `journal.jsonl` rather than a
    /// subagent transcript — it folds under [`Source::Ledger`] and carries no
    /// meta. Requires `workflow` to be set.
    pub journal: bool,
}

/// Build a replay stream from a full session's files — the main transcript plus
/// subagents (transcript + meta), workflow subagents, and workflow journals.
/// This is the multi-file equivalent of [`replay_from_jsonl`]; the native side
/// reads the same shapes off disk via `build_replay`. The meta sets each
/// subagent's parent (→ main, or → its workflow group) and type, so the graph
/// connects even before the spawning tool call is folded.
pub fn replay_from_session(
    main: &str,
    subagents: &[DemoSubagent],
) -> (Vec<ReplayItem>, crate::state::SessionInfo) {
    let mut items: Vec<ReplayItem> = Vec::new();
    push_lines(main, &Source::Main, &mut items);
    for sub in subagents {
        // A journal belongs to the workflow, not to any one agent: no meta, and
        // it folds under its own source. Ignore one with no workflow id — there
        // is nothing to attribute it to.
        if sub.journal {
            if let Some(wf) = sub.workflow {
                push_lines(sub.transcript, &Source::Ledger(wf.to_string()), &mut items);
            }
            continue;
        }
        if let Ok(meta) =
            serde_json::from_str::<crate::provider::claude::wire::SubagentMeta>(sub.meta)
        {
            items.push(ReplayItem::new(claude::Stream::meta(
                sub.agent_id,
                sub.workflow,
                &meta,
            )));
        }
        push_lines(
            sub.transcript,
            &Source::Sub(sub.agent_id.to_string()),
            &mut items,
        );
    }
    finish(items)
}

/// Parse a transcript's complete lines into `items` under `source`, inheriting
/// the previous in-file timestamp for lines that lack one.
fn push_lines(text: &str, source: &Source, items: &mut Vec<ReplayItem>) {
    let mut stream = claude::Stream::new(source.clone());
    items.extend(
        text.lines()
            .filter_map(|l| stream.push(l))
            .map(ReplayItem::new),
    );
}

/// Route untimed session-level metadata into the info store (dropping it from
/// the timeline), then date + stably sort the rest.
fn finish(mut items: Vec<ReplayItem>) -> (Vec<ReplayItem>, crate::state::SessionInfo) {
    let mut info = crate::state::SessionInfo::default();
    items.retain(|item| {
        if item.facts.iter().all(Fact::is_session_meta) {
            item.facts.iter().for_each(|f| info.apply(f));
            false
        } else {
            true
        }
    });
    date_and_sort(&mut items);
    (items, info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::session::MAIN_ID;

    #[test]
    fn births_date_to_first_activity_and_endings_to_last() {
        let sub = |t: &str| {
            format!(
                r#"{{"type":"user","uuid":"u","timestamp":"{t}","message":{{"role":"user","content":"x"}}}}"#
            )
        };
        let mut items = Vec::new();
        // The agent's birth from its sidecar: undated, pending on the agent.
        let meta: crate::provider::claude::wire::SubagentMeta =
            serde_json::from_str(r#"{"agentType":"guide"}"#).unwrap();
        items.push(ReplayItem::new(claude::Stream::meta("subX", None, &meta)));
        // The subagent's own transcript: first entry at :05, last at :15.
        push_lines(
            &format!(
                "{}\n{}\n",
                sub("2026-06-05T10:00:05.000Z"),
                sub("2026-06-05T10:00:15.000Z")
            ),
            &Source::Sub("subX".into()),
            &mut items,
        );
        // An undated ledger `result` for that agent: an ending. A `started`
        // ledger line states nothing and yields no item at all.
        push_lines(
            r#"{"type":"started","key":"k","agentId":"subX"}"#,
            &Source::Ledger("wf".into()),
            &mut items,
        );
        push_lines(
            r#"{"type":"result","key":"k","agentId":"subX","result":"done"}"#,
            &Source::Ledger("wf".into()),
            &mut items,
        );
        assert_eq!(
            items.len(),
            4,
            "birth + two entries + ending; `started` is nothing"
        );

        date_and_sort(&mut items);

        let birth_ts = items
            .iter()
            .find(|i| i.any(|f| matches!(f.kind, FactKind::Agent { .. })))
            .and_then(|i| i.ts());
        let ending_ts = items
            .iter()
            .find(|i| i.any(|f| matches!(f.kind, FactKind::Ended(_))))
            .and_then(|i| i.ts());
        assert_eq!(
            birth_ts,
            Some("2026-06-05T10:00:05.000Z".parse::<DateTime<Utc>>().unwrap()),
            "a birth dates to the agent's FIRST activity"
        );
        assert_eq!(
            ending_ts,
            Some("2026-06-05T10:00:15.000Z".parse::<DateTime<Utc>>().unwrap()),
            "an ending dates to the agent's LAST activity"
        );
        // Ties sort the birth before the activity it borrowed its date from.
        assert!(items[0].any(|f| matches!(f.kind, FactKind::Agent { .. })));
    }

    #[test]
    fn replay_from_jsonl_parses_orders_and_routes_noise() {
        let text = concat!(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-06-05T10:00:02.000Z","message":{"role":"user","content":"second"}}"#,
            "\n",
            "\n",
            r#"{"type":"user","uuid":"u0","timestamp":"2026-06-05T10:00:01.000Z","message":{"role":"user","content":"first"}}"#,
            "\n",
            r#"garbage that should be skipped"#,
            "\n",
        );
        let (items, _info) = replay_from_jsonl(text);
        // Two valid entries (blank + garbage skipped), sorted by timestamp.
        assert_eq!(items.len(), 2);
        assert!(items[0].ts().unwrap() < items[1].ts().unwrap());
        // All by the root agent.
        assert!(
            items
                .iter()
                .all(|i| i.facts.iter().all(|f| f.agent.as_deref() == Some(MAIN_ID)))
        );
    }

    #[test]
    fn replay_from_session_emits_subagent_birth_and_activity() {
        let main = r#"{"type":"user","uuid":"u1","timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"go"}}"#;
        let sub = DemoSubagent {
            agent_id: "a1000000000000001",
            meta: r#"{"agentType":"Explore","description":"map it","toolUseId":"toolu_1"}"#,
            transcript: r#"{"type":"user","uuid":"s1","isSidechain":true,"agentId":"a1000000000000001","timestamp":"2026-06-05T10:00:05.000Z","message":{"role":"user","content":"task"}}"#,
            workflow: None,
            journal: false,
        };
        let (items, _info) = replay_from_session(main, &[sub]);

        let by_sub = |f: &Fact| f.agent.as_deref() == Some("a1000000000000001");
        assert!(
            items
                .iter()
                .any(|i| i.any(|f| by_sub(f) && matches!(f.kind, FactKind::Agent { .. }))),
            "the subagent's birth is stated"
        );
        assert!(
            items
                .iter()
                .any(|i| i.any(|f| by_sub(f) && !matches!(f.kind, FactKind::Agent { .. }))),
            "the subagent's own activity is attributed to it"
        );
    }

    /// Workflow parity with the native loader: a subagent under
    /// `subagents/workflows/<id>/` must carry its workflow as parent (so the
    /// model creates the group node and parents it there), and the workflow's
    /// `journal.jsonl` must state endings — not activity of some agent.
    /// Without this the browser silently renders workflow sessions as a flat
    /// fan-out, while the native TUI shows the group.
    #[test]
    fn replay_from_session_tags_workflow_subagents_and_journals() {
        let main = r#"{"type":"user","uuid":"u1","timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"go"}}"#;
        let subs = [
            DemoSubagent {
                agent_id: "w1000000000000001",
                meta: r#"{"agentType":"workflow-subagent","description":"review:bugs"}"#,
                transcript: r#"{"type":"user","uuid":"s1","isSidechain":true,"agentId":"w1000000000000001","timestamp":"2026-06-05T10:00:05.000Z","message":{"role":"user","content":"task"}}"#,
                workflow: Some("wf-99"),
                journal: false,
            },
            DemoSubagent {
                agent_id: "",
                meta: "",
                transcript: r#"{"type":"result","key":"review","agentId":"w1000000000000001","result":{"ok":true}}"#,
                workflow: Some("wf-99"),
                journal: true,
            },
        ];
        let (items, _info) = replay_from_session(main, &subs);

        assert!(
            items.iter().any(|i| i.any(|f| matches!(
                &f.kind,
                FactKind::Agent { parent: Some(p), .. }
                    if f.agent.as_deref() == Some("w1000000000000001") && p == "wf-99"
            ))),
            "a workflow subagent's birth names its workflow as parent"
        );
        assert!(
            items
                .iter()
                .any(|i| i.any(|f| matches!(f.kind, FactKind::Ended(_))
                    && f.agent.as_deref() == Some("w1000000000000001"))),
            "journal results are endings for the agent they name"
        );
        assert!(
            !items
                .iter()
                .any(|i| i.any(|f| f.agent.as_deref() == Some(""))),
            "the journal is not mistaken for a subagent transcript"
        );
    }
}
