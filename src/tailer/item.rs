//! Portable replay-stream pieces — the timeline item and its ordering.
//!
//! Shared by the native replay assembly ([`super::replay`]) and the App's
//! `Timeline`, and free of any IO so it compiles on wasm too.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};

use crate::fact::{Fact, FactKind, Statement};
#[cfg(test)]
use crate::provider::claude::{self, Source};
use crate::provider::{FileRole, Provider, ReadMode, SessionFile, Stream, provider_of};

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

    /// Route this item's session-level metadata into `info`, leaving the
    /// activity. Returns whether anything is left: an item that was metadata
    /// through and through does not belong on the timeline.
    pub fn take_session_meta(&mut self, info: &mut crate::state::SessionInfo) -> bool {
        self.facts.retain(|f| {
            if f.is_session_meta() {
                info.apply(f);
                false
            } else {
                true
            }
        });
        !self.facts.is_empty()
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

/// A session handed over as text: the browser's feeder. JS reads the files
/// (a drop, an upload, or a directory it is allowed to tail) and passes each
/// one as `(path, text)`; the bundle classifies them with the same primitives
/// the native feeders use ([`Provider::classify`]), keeps one [`Stream`] per
/// tailed file so later appends continue where the load stopped, and states
/// each whole-read sidecar once.
///
/// The provider is read off the content, never a name: the first file any
/// provider recognises decides, and files that do not belong to the root's
/// session are ignored.
pub struct Bundle {
    provider: Provider,
    session: String,
    streams: HashMap<String, Stream>,
    seen: std::collections::HashSet<String>,
}

impl Bundle {
    /// Load a session from its files. `None` if no file is a transcript any
    /// provider reads, or none of them is a session's root.
    pub fn load(
        files: &[(&str, &str)],
    ) -> Option<(Bundle, Vec<ReplayItem>, crate::state::SessionInfo)> {
        let provider = files.iter().find_map(|(_, text)| provider_of(text))?;
        // The same path handed over twice is one file: the first copy wins.
        let mut seen_paths = std::collections::HashSet::new();
        let classified: Vec<(SessionFile, &str)> = files
            .iter()
            .filter(|(path, _)| seen_paths.insert(*path))
            .filter_map(|(path, text)| {
                provider
                    .classify(Path::new(path), head_of(text))
                    .map(|f| (f, *text))
            })
            .collect();
        let root = classified
            .iter()
            .find(|(f, _)| f.role == FileRole::Root)?
            .0
            .clone();
        let mut bundle = Bundle {
            provider,
            session: root.session.clone(),
            streams: HashMap::new(),
            seen: std::collections::HashSet::new(),
        };
        let mut items: Vec<ReplayItem> = Vec::new();
        // Root first, then the rest in the order given.
        let mut ordered: Vec<&(SessionFile, &str)> = classified.iter().collect();
        ordered.sort_by_key(|(f, _)| f.path != root.path);
        for (file, text) in ordered {
            if file.session != bundle.session {
                continue;
            }
            items.extend(bundle.feed(file, text).into_iter().map(ReplayItem::new));
        }
        let (items, info) = finish(items);
        Some((bundle, items, info))
    }

    /// Feed newly-read text: appended bytes for a file already loaded, or a
    /// whole new file. Returns what it stated, in order.
    pub fn append(&mut self, files: &[(&str, &str)]) -> Vec<Statement> {
        let mut out = Vec::new();
        for (path, text) in files {
            if let Some(stream) = self.streams.get_mut(*path) {
                out.extend(text.lines().filter_map(|l| stream.push(l)));
                continue;
            }
            let Some(file) = self.provider.classify(Path::new(path), head_of(text)) else {
                continue;
            };
            if file.session != self.session || file.role == FileRole::Root {
                continue;
            }
            out.extend(self.feed(&file, text));
        }
        out
    }

    /// One file's statements: through a new stream for a tailed file (kept for
    /// appends), or its sidecar statement once it parses.
    fn feed(&mut self, file: &SessionFile, text: &str) -> Vec<Statement> {
        let key = file.path.to_string_lossy().into_owned();
        match file.read {
            ReadMode::Tail => {
                let mut stream = self.provider.stream_for(file);
                let out: Vec<Statement> = text.lines().filter_map(|l| stream.push(l)).collect();
                self.streams.insert(key, stream);
                out
            }
            ReadMode::Whole => {
                if self.seen.contains(&key) {
                    return Vec::new();
                }
                match self.provider.sidecar(file, text) {
                    Some(st) => {
                        self.seen.insert(key);
                        vec![st]
                    }
                    // A mid-write read: retried on the next append.
                    None => Vec::new(),
                }
            }
        }
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// Tailed files being followed.
    pub fn file_count(&self) -> usize {
        self.streams.len()
    }

    /// Whole-read files whose statement has been made. A file handed over
    /// mid-write is not here until its text parses; the feeder resends it.
    pub fn accepted(&self) -> Vec<String> {
        let mut v: Vec<String> = self.seen.iter().cloned().collect();
        v.sort();
        v
    }
}

/// The first non-blank line, for classification.
fn head_of(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

#[cfg(test)]
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
    items.retain_mut(|item| item.take_session_meta(&mut info));
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
    fn bundle_parses_orders_and_routes_noise() {
        let text = concat!(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-06-05T10:00:02.000Z","message":{"role":"user","content":"second"}}"#,
            "\n",
            "\n",
            r#"{"type":"user","uuid":"u0","timestamp":"2026-06-05T10:00:01.000Z","message":{"role":"user","content":"first"}}"#,
            "\n",
            r#"garbage that should be skipped"#,
            "\n",
        );
        let (bundle, items, _info) = Bundle::load(&[("s.jsonl", text)]).unwrap();
        assert_eq!(bundle.provider(), Provider::Claude);
        assert_eq!(bundle.session(), "s");
        // Two valid entries (blank + garbage skipped), sorted by timestamp.
        assert_eq!(items.len(), 2);
        assert!(items[0].ts().unwrap() < items[1].ts().unwrap());
        // All by the root agent.
        assert!(
            items
                .iter()
                .all(|i| i.facts.iter().all(|f| f.agent.as_deref() == Some(MAIN_ID)))
        );
        // Nothing any provider reads: no session.
        assert!(Bundle::load(&[("x.txt", "hello")]).is_none());
    }

    #[test]
    fn bundle_emits_subagent_birth_and_activity_from_paths() {
        let main = r#"{"type":"user","uuid":"u1","timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"go"}}"#;
        let meta = r#"{"agentType":"Explore","description":"map it","toolUseId":"toolu_1"}"#;
        let sub = r#"{"type":"user","uuid":"s1","isSidechain":true,"agentId":"a1000000000000001","timestamp":"2026-06-05T10:00:05.000Z","message":{"role":"user","content":"task"}}"#;
        let (_, items, _info) = Bundle::load(&[
            ("s.jsonl", main),
            ("s/subagents/agent-a1000000000000001.meta.json", meta),
            ("s/subagents/agent-a1000000000000001.jsonl", sub),
        ])
        .unwrap();

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
    fn bundle_tags_workflow_subagents_and_journals_from_paths() {
        let main = r#"{"type":"user","uuid":"u1","timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"go"}}"#;
        let (_, items, _info) = Bundle::load(&[
            ("s.jsonl", main),
            (
                "s/subagents/workflows/wf-99/agent-w1000000000000001.meta.json",
                r#"{"agentType":"workflow-subagent","description":"review:bugs"}"#,
            ),
            (
                "s/subagents/workflows/wf-99/agent-w1000000000000001.jsonl",
                r#"{"type":"user","uuid":"s1","isSidechain":true,"agentId":"w1000000000000001","timestamp":"2026-06-05T10:00:05.000Z","message":{"role":"user","content":"task"}}"#,
            ),
            (
                "s/subagents/workflows/wf-99/journal.jsonl",
                r#"{"type":"result","key":"review","agentId":"w1000000000000001","result":{"ok":true}}"#,
            ),
        ])
        .unwrap();

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
            "a workflow journal result is the agent's ending"
        );
    }

    /// A Codex session dropped as its rollout files: the provider comes off
    /// the content, the root is the `user` thread, a child appended later
    /// continues to be read by the stream that learned whose file it is.
    #[test]
    fn bundle_reads_codex_rollouts_and_continues_streams_on_append() {
        let root = concat!(
            r#"{"timestamp":"2026-08-26T15:30:09.955Z","ordinal":0,"type":"session_meta","payload":{"id":"a","session_id":"a","cwd":"/p","originator":"codex-tui","cli_version":"0.149.1","source":"cli","thread_source":"user"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-26T15:31:00.000Z","ordinal":1,"type":"event_msg","payload":{"type":"item_completed","thread_id":"a","item":{"type":"UserMessage","content":[{"type":"text","text":"do it"}]},"started_at_ms":1787758260000,"completed_at_ms":1787758260000}}"#,
            "\n",
        );
        let child_head = concat!(
            r#"{"timestamp":"2026-08-26T15:33:46.000Z","ordinal":0,"type":"session_meta","payload":{"id":"c","session_id":"a","source":{"subagent":{"thread_spawn":{"parent_thread_id":"a","agent_path":"/root/x"}}},"thread_source":"subagent","subagent_history_start_ordinal":1}}"#,
            "\n",
        );
        let (mut bundle, items, _info) = Bundle::load(&[
            ("rollout-2026-08-26T15-30-09-a.jsonl", root),
            ("rollout-2026-08-26T15-33-46-c.jsonl", child_head),
        ])
        .unwrap();
        assert_eq!(bundle.provider(), Provider::Codex);
        assert_eq!(bundle.session(), "a");
        assert_eq!(bundle.file_count(), 2);
        assert!(
            items
                .iter()
                .any(|i| i.any(|f| matches!(f.kind, FactKind::Prompt(_))))
        );
        assert!(items.iter().any(|i| {
            i.any(|f| f.agent.as_deref() == Some("c") && matches!(f.kind, FactKind::Agent { .. }))
        }));

        // The child's own line arrives later: attributed to `c`, not lost.
        let later = r#"{"timestamp":"2026-08-26T15:34:00.000Z","ordinal":1,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"child said"}]}}"#;
        let statements = bundle.append(&[("rollout-2026-08-26T15-33-46-c.jsonl", later)]);
        assert_eq!(statements.len(), 1);
        assert_eq!(statements[0].facts[0].agent.as_deref(), Some("c"));
        assert!(
            matches!(&statements[0].facts[0].kind, FactKind::Reasoning(t) if t == "child said")
        );

        // A file of another session is ignored on append.
        let stranger = r#"{"timestamp":"2026-08-26T16:00:00.000Z","ordinal":0,"type":"session_meta","payload":{"id":"z","session_id":"z","source":"cli","thread_source":"user"}}"#;
        assert!(
            bundle
                .append(&[("rollout-2026-08-26T16-00-00-z.jsonl", stranger)])
                .is_empty()
        );
    }
}
