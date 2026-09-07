//! The unified replay/live timeline (a DVR).
//!
//! Live and replay are not two modes — they are one time-shifted timeline. There
//! is one ts-ordered item list ([`ReplayItem`]s) and one playhead ([`cursor`]).
//! The only real difference is whether the right edge is fixed (replay: the file
//! is complete) or growing (live: the tailer appends as the session runs).
//!
//! The edge is **always** the last event seen — never wall-clock now — so an old
//! session never grows an empty tail toward the present. **Pacing is decided by
//! the edge, not a mode:** behind the edge the cursor paces forward (compressing
//! dead air); at the edge it pins and new appends snap in; a scrub-back parks it
//! in the past and "follow" re-pins. Completion is unknowable (any session can be
//! resumed and appended to), so the feeder never stops tailing and "live" is
//! emergent (following the edge while it still grows).
//!
//! The one bit that survives is [`replay`](Timeline::replay) — the launch intent:
//! did you open a *recording to play back* or a *live session to follow*? It is
//! NOT a claim the file is complete (a replay can still grow and go live — the
//! feeder always tails, and nothing here is ever assumed finished). It's simply
//! the one thing the events can't reveal — a quiet live session is byte-identical
//! to a finished recording — so you state it at launch. It does NOT gate pacing
//! (the edge does); it only picks how liveness reads "now" — the **playhead** for
//! a replay (its timestamps are a past recording, unrelated to wall time), the
//! **wall clock** at a live edge — and whether reaching the end settles
//! interactive agents. See ARCHITECTURE.md §6.
//!
//! The Timeline owns the items and the playback state; the App owns the derived
//! `SessionModel`/`Flow` and folds the prefix `items[0..fold_target()]`. Folding
//! is incremental forward (cheap) and a rebuild only on backward seek.
//!
//! [`cursor`]: Timeline::cursor

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::fact::{FactKind, Statement};
use crate::state::session::MAIN_ID;
use crate::tailer::{ReplayItem, Timing};

/// The replay/live timeline and its playhead.
pub struct Timeline {
    /// Ts-ordered items (bulk-loaded, then appended as the feeder tails).
    pub items: Vec<ReplayItem>,
    /// The launch intent: opened as a **replay** of a recording (play it back from
    /// the start) vs a **live** session (follow the growing edge). Set once from
    /// `Mode`, never changes. NOT a claim the file is complete — a replay can grow
    /// and go live (the feeder always tails); it's just the one thing the events
    /// can't reveal (a quiet live session is byte-identical to a finished
    /// recording). Does not gate pacing (the edge does); it selects the liveness
    /// `now` reference — the playhead for a replay (its timestamps are a past
    /// recording, unrelated to wall time), wall-clock at a live edge — and the
    /// end-settle (`just_ended`).
    pub replay: bool,
    /// Playhead = the universal "now" for rendering. `None` before any item.
    pub cursor: Option<DateTime<Utc>>,
    /// How many items have been folded into the derived model so far.
    pub folded: usize,
    /// Pinned to the edge (live following / playing at the replay edge)?
    pub follow_head: bool,
    /// Replay playback multiplier (ignored in live).
    pub speed: f64,
    /// Cached newest timestamp across `items` (the right edge).
    head: Option<DateTime<Utc>>,
    /// Whether the replay end-of-stream has already been signalled once.
    ended: bool,
    /// Start of the gap the cursor is currently crossing (the newest item ts at
    /// or before the cursor). Resets `gap_progress` when it changes.
    gap_anchor: Option<DateTime<Utc>>,
    /// Fraction of the current gap already crossed (0..=1). Accumulated forward
    /// each frame at the current budget's rate, so a mid-gap budget change (`g`
    /// toggling inactivity-skip, or a speed change) only alters the forward rate
    /// — it never repositions the cursor, so the playhead can't jump backward.
    gap_progress: f64,
    /// Agent ids referenced by still-undated items (metas / journal lines whose
    /// agent has no entries yet): a batch carrying entries for one of these
    /// must re-run dating. Lets `append_live` skip the full date-and-sort for
    /// the common in-order, fully-timed batch.
    undated_agents: HashSet<String>,
    /// Bumped whenever an item's INDEX may have changed meaning: a bulk
    /// replay load, or a re-sort that moved already-present items.
    ///
    /// Anything that caches a result keyed by item index — the snapshot ladder
    /// in particular — records the generation it was built under and must
    /// discard itself when this moves. Without it a snapshot of `items[0..N]`
    /// silently describes a different prefix after a late batch dates a
    /// previously-pending item and the whole list re-sorts around it.
    pub generation: u64,
    /// How many leading items every generation bump since the last
    /// [`take_disturbance`](Self::take_disturbance) is known to have left
    /// alone.
    ///
    /// A re-sort is rarely a reshuffle: a late subagent entry inserts among the
    /// newest items and leaves the long prefix before it exactly where it was.
    /// `generation` alone cannot say that, so a cache keyed by index would have
    /// to discard itself wholesale on every out-of-order batch — which, once a
    /// subagent is writing, is most batches. This is the part that is provably
    /// still valid; it accumulates as a MINIMUM so a consumer that misses a bump
    /// (the App only reconciles while following the edge) still sees the worst
    /// case rather than the last one.
    stable_prefix: usize,
    /// Skip inactivity: compress dead-air gaps during paced playback (see
    /// `compress_gap`). On by default (review-friendly); toggle off for
    /// faithful real-time pacing. Presentation-only — never affects content.
    pub compress_gaps: bool,
}

/// Inactivity-skip parameters (see `compress_gap`). Skipping dead air is a
/// *presentation-pacing* policy — it governs how many real seconds we spend
/// crossing a gap in *time*, and touches nothing about content (chips age on
/// the playhead, so a compressed gap ages them correctly regardless). It's a
/// policy because the scrubber bar is event-based: a gap has zero width on it,
/// so without skipping the playhead would freeze there for the gap's full
/// duration. Toggle off ([`Timeline::compress_gaps`] = false) for faithful,
/// real-time pacing.
///
/// Gaps whose faithful crossing time is under the knee play at true pace; above
/// it, crossing time grows only logarithmically — so a 5-minute wait still
/// reads as *longer* than a 5-second one while staying bounded to a few seconds.
const GAP_FAITHFUL_KNEE: f64 = 0.8;
/// Log-compression scale above the knee (seconds per e-fold of overage).
const GAP_COMPRESS_SCALE: f64 = 0.6;

/// A gap between consecutive events at least this long (seconds) is flagged on
/// the scrubber as a fast-forward (dead-air-compressed) stretch.
const GAP_MARKER_SECS: i64 = 60;

/// Real seconds to spend crossing a gap whose *faithful* crossing time (gap ÷
/// speed) is `faithful` seconds. Identity below the knee (play it straight),
/// logarithmic above it — bounded but monotone, so longer waits read longer.
/// Continuous at the knee (the log term is 0 there).
fn compress_gap(faithful: f64) -> f64 {
    if faithful <= GAP_FAITHFUL_KNEE {
        faithful
    } else {
        GAP_FAITHFUL_KNEE
            + GAP_COMPRESS_SCALE * (1.0 + (faithful - GAP_FAITHFUL_KNEE) / GAP_FAITHFUL_KNEE).ln()
    }
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    /// A fresh, empty timeline configured for **live** (growing edge, following).
    pub fn new() -> Self {
        Timeline {
            items: Vec::new(),
            replay: false,
            cursor: None,
            folded: 0,
            follow_head: true,
            speed: 1.0,
            head: None,
            ended: false,
            gap_anchor: None,
            gap_progress: 0.0,
            undated_agents: HashSet::new(),
            compress_gaps: true,
            generation: 0,
            stable_prefix: usize::MAX,
        }
    }

    /// Load a bulk-parsed (sorted) stream as a **replay** — play it back from the
    /// start. The feeder keeps tailing, so appends still extend this stream (a
    /// resumed session just plays on into the new events — effectively live again).
    pub fn load_replay(&mut self, items: Vec<ReplayItem>, speed: f64) {
        let start = items.iter().find_map(|i| i.ts());
        self.head = items.iter().filter_map(|i| i.ts()).max();
        self.items = items;
        self.replay = true;
        self.cursor = start;
        self.folded = 0;
        self.follow_head = true;
        self.speed = if speed > 0.0 { speed } else { 1.0 };
        self.ended = false;
        self.gap_anchor = start;
        self.gap_progress = 0.0;
        self.generation = self.generation.wrapping_add(1);
        // Every index means something new — nothing is stable.
        self.stable_prefix = 0;
        self.rescan_undated();
    }

    /// Append tailed updates at the head, advancing the edge. While *following*,
    /// the cursor tracks the new edge — but a cursor behind the edge (scrubbed
    /// back, or mid-replay) keeps its place and reaches the new events by playing
    /// forward, never jumping to them. New growth also un-latches the
    /// end-of-stream signal (a resumed session can end again).
    pub fn append_live(&mut self, statements: Vec<Statement>) {
        let before = self.items.len();
        // Ride the new edge only if the cursor was already at it. If behind —
        // scrubbed back, or catching up after pressing play — keep our place and
        // let `advance` pace forward to the growth. (Cursor-vs-head, not `folded`,
        // so it's self-contained: `folded` is the App's to maintain.)
        let was_at_edge = match (self.cursor, self.head) {
            (_, None) => true,            // no edge yet → ride
            (None, Some(_)) => false,     // have an edge but no cursor → not riding
            (Some(c), Some(h)) => c >= h, // cursor at/after the edge → riding
        };
        // `items` must stay timestamp-ordered (same dating + sort as replay) so
        // the scrubber maps correctly and a backward seek folds the right
        // prefix. The common batch — timestamped entries arriving in order —
        // keeps the invariant by construction, so it appends in O(batch); only
        // a batch that is out of order (a backfill/late-discovered subagent
        // dumps an earlier block), carries untimed items (metas/journals), or
        // can date a pending untimed item re-runs the full date-and-sort. The
        // model is order-independent, so the App applies due updates directly
        // (see the `Batch` handler) rather than by index, and a re-sort never
        // costs a rebuild. `head` is the max ts → unaffected by ordering.
        let mut tail_ts = self.items.last().and_then(|i| i.ts());
        let mut in_order = true;
        let mut needs_dating = false;
        // Earliest timestamp in this batch: where a re-sort can start moving
        // things, and so where the untouched prefix ends.
        let mut earliest_new: Option<DateTime<Utc>> = None;
        for statement in statements {
            let item = ReplayItem::new(statement);
            match item.ts() {
                Some(ts) => {
                    self.head = Some(self.head.map_or(ts, |h| h.max(ts)));
                    earliest_new = Some(earliest_new.map_or(ts, |e: DateTime<Utc>| e.min(ts)));
                    if tail_ts.is_some_and(|t| ts < t) {
                        in_order = false;
                    }
                    tail_ts = Some(tail_ts.map_or(ts, |t| t.max(ts)));
                    if !self.undated_agents.is_empty()
                        && item.any(|f| {
                            f.agent
                                .as_ref()
                                .is_some_and(|id| self.undated_agents.contains(id))
                        })
                    {
                        needs_dating = true;
                    }
                }
                None => needs_dating = true,
            }
            self.items.push(item);
        }
        if needs_dating || !in_order {
            // Existing items can move: an item that was `Pending` sorts ahead
            // of every dated one and jumps into place once its date resolves,
            // shifting every index in between. Index-keyed caches are void
            // from `stable_prefix` on.
            let stable = if needs_dating {
                // Undated items sort to the head, so one arriving (or leaving,
                // once it dates) shifts every index after it — i.e. all of them.
                0
            } else {
                // Purely an out-of-order batch: the new items are all dated and
                // land at or after the first item they undercut. Everything
                // before that is untouched. `<` rather than `<=` keeps this
                // conservative against the sort's equal-timestamp tie-break.
                let earliest = earliest_new.expect("an out-of-order batch is dated");
                self.items[..before].partition_point(|i| i.ts().is_none_or(|t| t < earliest))
            };
            self.stable_prefix = self.stable_prefix.min(stable);
            crate::tailer::date_and_sort_live(&mut self.items);
            self.rescan_undated();
            self.generation = self.generation.wrapping_add(1);
        }
        if self.items.len() != before {
            self.ended = false;
        }
        if self.follow_head && was_at_edge {
            self.cursor = self.head;
        }
    }

    /// How many leading items are still known-good since the last call, then
    /// reset. Consumers of `generation` pair the two: a bump says *some* index
    /// changed meaning, this says from where.
    pub fn take_disturbance(&mut self) -> usize {
        std::mem::replace(&mut self.stable_prefix, usize::MAX)
    }

    /// Recompute which undated items are waiting on future data (the metas /
    /// journal lines whose agent has no entries yet), so [`append_live`] knows
    /// when a batch makes re-dating worthwhile.
    fn rescan_undated(&mut self) {
        self.undated_agents.clear();
        for item in &self.items {
            // A `Pending` item names the agent it's blocked on; `Leader`/`Dated`
            // items wait on nothing. The join target is read off the item — no
            // re-deriving it from the update shape.
            if let Timing::Pending(agent) = &item.timing {
                self.undated_agents.insert(agent.clone());
            }
        }
    }

    /// Whether we're actively riding the live edge — pinned (`follow_head`) AND
    /// the cursor is at/past the edge. Both are required: pausing at the edge
    /// drops `follow_head` while the cursor still sits at the head, and a paused
    /// session must NOT ride new appends (they buffer behind the edge instead).
    /// An empty edge counts as following. This gates whether a live batch snaps
    /// into the view or buffers.
    pub fn following(&self) -> bool {
        self.follow_head && self.cursor.zip(self.head).is_none_or(|(c, h)| c >= h)
    }

    /// The right edge timestamp (newest item ts), if any.
    pub fn head_ts(&self) -> Option<DateTime<Utc>> {
        self.head
    }

    /// Whether an item is "due" at the current cursor — already past the
    /// playhead. Timestamp-less items (metas/journals/leaders) ride along with
    /// their predecessor, so they are due as soon as the scan reaches them.
    fn due(&self, item: &ReplayItem) -> bool {
        match (item.ts(), self.cursor) {
            (Some(t), Some(c)) => t <= c,
            (None, _) => true,
            (Some(_), None) => false,
        }
    }

    /// Number of leading items that should be folded at the current cursor.
    ///
    /// Items are ts-ordered with untimed (always-due) items sorted first, so
    /// "due" is a monotone prefix — binary search it instead of walking the
    /// whole vec (this runs every frame).
    pub fn fold_target(&self) -> usize {
        self.items.partition_point(|item| self.due(item))
    }

    /// Advance the playhead one frame. Replay paces the cursor toward the next
    /// recorded event; with inactivity-skip on ([`compress_gaps`]) dead air is
    /// log-compressed (see `compress_gap`) so idle stretches pass quickly but
    /// graded, off it is paced faithfully in real time. Live following keeps the
    /// cursor pinned to the (already-advanced) edge. A no-op when paused, parked
    /// (not following), or at the edge.
    ///
    /// [`compress_gaps`]: Timeline::compress_gaps
    pub fn advance(&mut self, elapsed: Duration, paused: bool) {
        if paused || !self.follow_head {
            return;
        }
        // No timed cursor/edge yet → nothing to pace.
        let (Some(cur), Some(h)) = (self.cursor, self.head) else {
            return;
        };
        if cur >= h {
            // Riding the edge: pin to it (new items fold in via `append_live`).
            // This is the only "snap to live" path — behind the edge we always
            // pace forward, so pressing play resumes from the cursor (and a live
            // session caught up to the edge then follows), never jumping ahead.
            self.cursor = Some(h);
            return;
        }

        // The next recorded event strictly after the cursor, and the newest event
        // at or before it (the gap we're crossing). Both derived from the cursor,
        // not `folded`, so advance is self-contained. Items are ts-sorted with
        // untimed ones first, so both are one binary search apart — this runs
        // every frame, a linear scan here dominated replay CPU.
        let idx = self
            .items
            .partition_point(|i| i.ts().is_none_or(|t| t <= cur));
        let Some(boundary) = self.items.get(idx).and_then(|i| i.ts()) else {
            // No future event: we've reached the end; pin to the edge.
            self.cursor = self.head;
            return;
        };
        let anchor = self.items[..idx]
            .iter()
            .rev()
            .find_map(|i| i.ts())
            .unwrap_or(cur);

        // Entering a new gap resets the per-gap progress.
        if self.gap_anchor != Some(anchor) {
            self.gap_anchor = Some(anchor);
            self.gap_progress = 0.0;
        }

        let gap_ms = (boundary - anchor).num_milliseconds().max(0) as f64;
        // Real time allotted to cross this gap. Faithful = gap ÷ speed; with
        // inactivity-skip on, log-compress it so long dead air passes quickly
        // but graded. Off → faithful (real-time pacing, no skipping).
        let faithful = gap_ms / 1000.0 / self.speed;
        let budget = if self.compress_gaps {
            compress_gap(faithful)
        } else {
            faithful
        };

        // Accumulate the fraction crossed at the current budget's rate. Advancing
        // progress forward (rather than recomputing it from elapsed÷budget every
        // frame) is what keeps the cursor monotone across a mid-gap budget change:
        // pressing `g` to switch from compressed to faithful grows the budget, and
        // a from-scratch recompute would shrink the fraction and snap the cursor
        // back toward the anchor — the "time goes backwards, then barely moves"
        // symptom. Here the change only slows the forward rate.
        if budget > 0.0 {
            self.gap_progress += elapsed.as_secs_f64() / budget;
        }
        if budget <= 0.0 || self.gap_progress >= 1.0 {
            self.cursor = Some(boundary); // reached the next event
        } else {
            self.cursor =
                Some(anchor + chrono::Duration::milliseconds((gap_ms * self.gap_progress) as i64));
        }
    }

    /// Reset per-gap pacing after an external cursor jump (a seek). Without it the
    /// next `advance` reuses the stale gap budget — a seek landing inside the same
    /// gap would over-jump on the first post-seek frame.
    pub fn reset_pacing(&mut self) {
        self.gap_anchor = None;
        self.gap_progress = 0.0;
    }

    /// Whether the playhead is at the right edge (everything folded).
    pub fn at_edge(&self) -> bool {
        !self.items.is_empty() && self.folded >= self.items.len()
    }

    /// The "now" reference for liveness derivation. Following a live (follow-
    /// intent) edge it is wall clock — so quiet agents go idle in real time, and
    /// an old session followed to its edge correctly reads as idle. Pacing a
    /// replay or scrubbed back, it is the playhead (the as-of-then state, no
    /// wall-clock bleed).
    pub fn now_reference(&self) -> Option<DateTime<Utc>> {
        if !self.replay && self.follow_head && self.at_edge() {
            Some(Utc::now())
        } else {
            self.cursor
        }
    }

    /// A **replay** reached its end exactly once — settle interactive agents to
    /// idle (the buffer is exhausted). A live stream never fires this (its edge
    /// could always grow). Latches; new appends un-latch it (a resumed session can
    /// end again).
    pub fn just_ended(&mut self) -> bool {
        if self.replay && self.at_edge() && !self.ended {
            self.ended = true;
            return true;
        }
        false
    }

    /// The earliest timestamp in the stream (the left edge), if any.
    pub fn start_ts(&self) -> Option<DateTime<Utc>> {
        self.items.iter().find_map(|i| i.ts())
    }

    /// Whether the stream has a non-trivial span to scrub (≥1 timestamped item).
    pub fn has_span(&self) -> bool {
        self.head.is_some()
    }

    /// Number of items unavoidably folded at the very start — a same-timestamp
    /// clump (and dated session metadata) that the model can only apply
    /// atomically. The bar is normalized over `[floor, len]` so this clump maps
    /// to position 0; otherwise the playhead could never reach the left edge
    /// (and a left-click would spring back as the next tick time-folds the clump).
    pub fn floor(&self) -> usize {
        // Cheap: scans only the leading clump and breaks at the first event
        // after the start, so it's ~O(clump), not O(items) — no cache needed.
        let Some(start) = self.start_ts() else {
            return self.items.len(); // no timed events → nothing to scrub
        };
        let mut n = 0;
        for item in &self.items {
            match item.ts() {
                Some(t) if t <= start => n += 1,
                None => n += 1, // untimed leaders ride along with the start
                Some(_) => break,
            }
        }
        n
    }

    /// Scrubber position, `0.0..=1.0` — **by event, not by wall-clock time**, and
    /// normalized over the reachable range `[floor, len]` so the playhead can sit
    /// flush-left at the start clump and flush-right at the end.
    pub fn progress(&self) -> f64 {
        let floor = self.floor();
        let reach = self.items.len().saturating_sub(floor);
        if reach == 0 {
            return if self.folded > 0 { 1.0 } else { 0.0 };
        }
        (self.folded.saturating_sub(floor) as f64 / reach as f64).clamp(0.0, 1.0)
    }

    /// The folded-item count at scrubber fraction `f` — inverse of
    /// [`progress`](Self::progress), in `[floor, len]`, so a click lands the
    /// playhead under the cursor exactly.
    pub fn fold_at_fraction(&self, f: f64) -> usize {
        let floor = self.floor();
        let reach = self.items.len().saturating_sub(floor);
        floor + (f.clamp(0.0, 1.0) * reach as f64).round() as usize
    }

    /// Bar position (`0.0..=1.0`) of the event at item `idx` — for placing the
    /// fast-forward markers. Items inside the start clump map to 0.
    pub fn bar_fraction_for_index(&self, idx: usize) -> f64 {
        let floor = self.floor();
        let reach = self.items.len().saturating_sub(floor);
        if reach == 0 {
            return 0.0;
        }
        ((idx + 1).saturating_sub(floor) as f64 / reach as f64).clamp(0.0, 1.0)
    }

    /// Item indices that begin after a large real-time gap (≥ `GAP_MARKER_SECS`)
    /// — where playback compresses dead air, i.e. "time goes fast here."
    pub fn gap_markers(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut prev: Option<DateTime<Utc>> = None;
        for (i, item) in self.items.iter().enumerate() {
            if let Some(ts) = item.ts() {
                if let Some(p) = prev
                    && (ts - p).num_seconds() >= GAP_MARKER_SECS
                {
                    out.push(i);
                }
                prev = Some(ts);
            }
        }
        out
    }

    /// Item indices of main-thread human prompts — the prompt-era boundaries that
    /// `[`/`]` step between (see [`App::seek_prompt`]). Surfaced on the scrubber
    /// as chapter ticks so those jump targets are visible. Shares the era spine's
    /// definition: a `Prompt` fact, which a provider states only for text a
    /// person typed, so injected user text is never marked.
    ///
    /// [`App::seek_prompt`]: crate::state::App::seek_prompt
    pub fn prompt_markers(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                item.any(|f| {
                    matches!(f.kind, FactKind::Prompt(_)) && f.agent.as_deref() == Some(MAIN_ID)
                })
                .then_some(i)
            })
            .collect()
    }

    /// The newest timestamp at or before item index `idx` — the cursor value for
    /// an index-based seek (so pacing/liveness still have a real timestamp).
    ///
    /// Untimed leader items (flat metadata, empty subagents) sort to the FRONT,
    /// so a seek to index 0 would otherwise land a `None` cursor — which blanks
    /// the date readout and stalls `advance` (it needs a real cursor). Fall back
    /// to the first timestamp in the stream so the cursor is never `None` while
    /// any timed event exists.
    pub fn ts_at_index(&self, idx: usize) -> Option<DateTime<Utc>> {
        let end = (idx + 1).min(self.items.len());
        self.items[..end]
            .iter()
            .rev()
            .find_map(|i| i.ts())
            .or_else(|| self.start_ts())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::Fact;
    use crate::provider::claude::wire;
    use crate::provider::claude::{Record, Source};
    use crate::tailer::ReplayItem;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// A human prompt on the main file at `t`.
    fn stmt(t: &str) -> Statement {
        let line = format!(
            "{{\"type\":\"user\",\"uuid\":\"u\",\"parentUuid\":null,\"origin\":{{\"kind\":\"human\"}},\"timestamp\":\"{t}\",\"message\":{{\"role\":\"user\",\"content\":\"x\"}}}}"
        );
        Record::Entry {
            source: Source::Main,
            entry: wire::parse_line(&line).unwrap(),
        }
        .statement()
        .unwrap()
    }

    fn entry_item(t: &str) -> ReplayItem {
        ReplayItem::new(stmt(t))
    }

    /// An untimed leader (no join target) — rides at the head.
    fn leader() -> Statement {
        Statement {
            at: None,
            facts: vec![Fact {
                agent: None,
                ts: None,
                kind: FactKind::Activity,
            }],
        }
    }

    fn meta_item() -> ReplayItem {
        ReplayItem::new(leader())
    }

    #[test]
    fn following_requires_both_follow_head_and_at_edge() {
        let mut tl = Timeline::new();
        tl.load_replay(
            vec![
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T10:00:10.000Z"),
            ],
            8.0,
        );
        // Playhead parked at the start, behind the edge → not following.
        assert!(!tl.following(), "a cursor behind the edge is not following");

        // Cursor rides up to the edge while pinned → following.
        tl.cursor = tl.head_ts();
        assert!(tl.following());

        // Parked at the edge (pause drops `follow_head` but the cursor still sits
        // at the head) → NOT following, so a live append buffers instead of
        // snapping the view. This is the live-pause invariant.
        tl.follow_head = false;
        assert!(
            !tl.following(),
            "a parked cursor at the edge is not following"
        );
    }

    #[test]
    fn now_reference_is_wall_clock_only_at_a_live_edge_else_the_playhead() {
        let old = ts("2020-01-01T00:00:00.000Z");
        let mut tl = Timeline::new();
        tl.items = vec![entry_item("2020-01-01T00:00:00.000Z")];
        tl.cursor = Some(old);
        tl.folded = 1; // at the edge (everything folded)

        // LIVE, following, at the edge → the WALL CLOCK: a quiet live session
        // idles its agents out as real time passes.
        tl.replay = false;
        tl.follow_head = true;
        let now = tl.now_reference().unwrap();
        assert!(
            now > old && (Utc::now() - now).num_seconds() < 5,
            "a live edge judges liveness against real now"
        );

        // REPLAY at the same edge → the PLAYHEAD, never wall clock: an old
        // recording must read as-of-then, not "everything idle because the file
        // is from 2020". This is the whole job of the `replay` flag.
        tl.replay = true;
        assert_eq!(
            tl.now_reference(),
            Some(old),
            "a replay judges liveness against the playhead"
        );

        // LIVE but scrubbed back off the edge → the playhead (as-of-then), no
        // wall-clock bleed into a historical view.
        tl.replay = false;
        tl.follow_head = false;
        assert_eq!(
            tl.now_reference(),
            Some(old),
            "scrubbed back → the playhead"
        );
    }

    #[test]
    fn gap_markers_flag_gaps_at_or_over_the_threshold() {
        // GAP_MARKER_SECS = 60, and the check is `>=` — pin the boundary so an
        // off-by-one there can't silently drop (or fabricate) a fast-forward mark.
        let mut tl = Timeline::new();
        tl.items = vec![
            entry_item("2026-06-05T10:00:00.000Z"), // 0
            entry_item("2026-06-05T10:00:59.000Z"), // +59s → no marker
            entry_item("2026-06-05T10:01:59.000Z"), // +60s → marker at idx 2
            entry_item("2026-06-05T10:03:00.000Z"), // +61s → marker at idx 3
        ];
        assert_eq!(
            tl.gap_markers(),
            vec![2, 3],
            "a 60s gap flags; a 59s gap does not"
        );
    }

    #[test]
    fn prompt_markers_index_human_prompts_only() {
        // Chapter ticks mark human prompts only: entry_item carries origin.kind
        // "human" (a real prompt); a task-notification-origin line is system-
        // injected (excluded); a non-User item is skipped. Values are ITEM
        // indices, not columns.
        let system = {
            let line = r#"{"type":"user","uuid":"u","parentUuid":null,"origin":{"kind":"task-notification"},"timestamp":"2026-06-05T10:30:00.000Z","message":{"role":"user","content":"3 background agents were stopped"}}"#;
            ReplayItem::new(
                Record::Entry {
                    source: Source::Main,
                    entry: wire::parse_line(line).unwrap(),
                }
                .statement()
                .unwrap(),
            )
        };
        let mut tl = Timeline::new();
        tl.items = vec![
            entry_item("2026-06-05T10:00:00.000Z"), // 0: human prompt
            system,                                 // 1: system-injected → excluded
            meta_item(),                            // 2: not a User prompt
            entry_item("2026-06-05T11:00:00.000Z"), // 3: human prompt
        ];
        assert_eq!(tl.prompt_markers(), vec![0, 3]);
    }

    #[test]
    fn bar_fraction_for_index_spans_zero_to_one_and_clamps() {
        // Empty → 0.0, not a divide-by-zero.
        let tl = Timeline::new();
        assert_eq!(tl.bar_fraction_for_index(0), 0.0);

        let mut tl = Timeline::new();
        tl.load_replay(
            vec![
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T10:00:10.000Z"),
                entry_item("2026-06-05T10:00:20.000Z"),
            ],
            8.0,
        );
        // The last item sits at the right end; past-the-end clamps, never panics.
        let last = tl.items.len() - 1;
        assert_eq!(
            tl.bar_fraction_for_index(last),
            1.0,
            "last item → far right"
        );
        assert_eq!(tl.bar_fraction_for_index(999), 1.0, "beyond the end clamps");
        // Monotonic non-decreasing and within [0, 1].
        let f0 = tl.bar_fraction_for_index(0);
        assert!((0.0..=1.0).contains(&f0));
        assert!(tl.bar_fraction_for_index(1) >= f0);
    }

    #[test]
    fn replay_starts_at_first_item_and_folds_it() {
        let mut tl = Timeline::new();
        tl.load_replay(
            vec![
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T10:00:10.000Z"),
            ],
            8.0,
        );
        assert!(tl.replay);
        assert_eq!(tl.cursor, Some(ts("2026-06-05T10:00:00.000Z")));
        // The first moment is due immediately; the later one is not.
        assert_eq!(tl.fold_target(), 1);
    }

    #[test]
    fn advance_paces_cursor_by_speed_and_clamps_to_head() {
        let mut tl = Timeline::new();
        // A 2s gap stays under the compression knee at speed 8 (2/8 = 0.25s), so
        // this exercises pure speed-pacing rather than dead-air compression.
        tl.load_replay(
            vec![
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T10:00:02.000Z"),
            ],
            8.0,
        );
        // 125ms real is half the 0.25s budget → halfway across the 2s gap (+1s).
        tl.advance(Duration::from_millis(125), false);
        assert_eq!(tl.cursor, Some(ts("2026-06-05T10:00:01.000Z")));
        assert_eq!(tl.fold_target(), 1);

        // The rest of the budget reaches (and clamps to) the head, folding item 2.
        tl.advance(Duration::from_millis(125), false);
        assert_eq!(tl.cursor, Some(ts("2026-06-05T10:00:02.000Z")));
        assert_eq!(tl.fold_target(), 2);
    }

    #[test]
    fn advance_is_a_noop_when_paused_or_parked() {
        let mut tl = Timeline::new();
        tl.load_replay(
            vec![
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T10:00:10.000Z"),
            ],
            8.0,
        );
        tl.advance(Duration::from_secs(1), true); // paused
        assert_eq!(tl.cursor, Some(ts("2026-06-05T10:00:00.000Z")));
        tl.follow_head = false; // parked (scrubbed)
        tl.advance(Duration::from_secs(1), false);
        assert_eq!(tl.cursor, Some(ts("2026-06-05T10:00:00.000Z")));
    }

    #[test]
    fn advance_compresses_long_idle_gaps_but_eases() {
        let mut tl = Timeline::new();
        tl.load_replay(
            vec![
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T11:00:00.000Z"), // an hour of dead air
            ],
            1.0,
        );
        // A small step eases only partway across — not an instant teleport.
        tl.advance(Duration::from_millis(100), false);
        let mid = tl.cursor.unwrap();
        assert!(mid > ts("2026-06-05T10:00:00.000Z") && mid < ts("2026-06-05T11:00:00.000Z"));
        // A generous chunk (past any gap's bounded budget) reaches the edge — an
        // hour of dead air is crossed in a few real seconds, not paced in full.
        tl.advance(Duration::from_secs(10), false);
        assert_eq!(tl.cursor, Some(ts("2026-06-05T11:00:00.000Z")));
    }

    #[test]
    fn faithful_mode_paces_gaps_in_real_time() {
        let mut tl = Timeline::new();
        tl.compress_gaps = false; // skip inactivity OFF
        tl.load_replay(
            vec![
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T11:00:00.000Z"), // 1h gap
            ],
            1.0,
        );
        // At speed 1 with skipping off, 10 real-seconds crosses ~10s of the hour
        // — the gap is NOT compressed.
        tl.advance(Duration::from_secs(10), false);
        let c = tl.cursor.unwrap();
        assert!(
            c >= ts("2026-06-05T10:00:09.000Z") && c <= ts("2026-06-05T10:00:11.000Z"),
            "faithful pacing crosses ~10s of the hour, got {c}"
        );
    }

    #[test]
    fn toggling_inactivity_skip_mid_gap_never_rewinds_the_cursor() {
        // Regression: pressing `g` mid-gap flips the budget from compressed to
        // faithful. A from-scratch elapsed÷budget recompute would shrink the
        // fraction and snap the cursor back toward the gap anchor (time running
        // backwards, then barely advancing). Progress must stay monotone.
        let mut tl = Timeline::new(); // compress ON
        tl.load_replay(
            vec![
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T11:00:00.000Z"), // 1h of dead air
            ],
            1.0,
        );
        // Ease partway across the compressed gap.
        tl.advance(Duration::from_secs(1), false);
        let before = tl.cursor.unwrap();
        assert!(
            before > ts("2026-06-05T10:00:00.000Z"),
            "compressed pacing moved off the anchor"
        );

        // Flip inactivity-skip OFF (the `g` keypress) and step again. The much
        // larger faithful budget must only slow forward progress — never rewind.
        tl.compress_gaps = false;
        tl.advance(Duration::from_secs(1), false);
        let after = tl.cursor.unwrap();
        assert!(
            after >= before,
            "cursor rewound on toggle: {before} -> {after}"
        );
    }

    #[test]
    fn compress_gap_is_faithful_below_knee_then_bounded_and_graded() {
        // Below the knee: identity (play it straight).
        assert_eq!(compress_gap(0.5), 0.5);
        // Continuous at the knee (log term is 0 there).
        assert!((compress_gap(GAP_FAITHFUL_KNEE) - GAP_FAITHFUL_KNEE).abs() < 1e-9);
        // Bounded: an hour of dead air crosses in a few seconds.
        assert!(compress_gap(3600.0) < 10.0);
        // Graded/monotone: a longer wait still reads as longer (the point of
        // log-compression — bounded, but not all collapsed to one instant).
        assert!(compress_gap(3600.0) > compress_gap(60.0));
        assert!(compress_gap(60.0) > compress_gap(10.0));
    }

    #[test]
    fn live_following_pins_cursor_to_growing_head() {
        let mut tl = Timeline::new();
        assert!(!tl.replay);

        // A timestamp-less update (e.g. a meta) rides along at the head.
        tl.append_live(vec![leader()]);
        assert_eq!(tl.fold_target(), tl.items.len());

        // A timestamped live entry advances the head, and following pins to it.
        tl.append_live(vec![stmt("2026-06-05T10:00:05.000Z")]);
        assert_eq!(tl.head_ts(), Some(ts("2026-06-05T10:00:05.000Z")));
        assert_eq!(tl.cursor, Some(ts("2026-06-05T10:00:05.000Z")));
        assert_eq!(
            tl.fold_target(),
            tl.items.len(),
            "following folds everything"
        );
    }

    #[test]
    fn append_live_keeps_items_timestamp_sorted() {
        let mut tl = Timeline::new();
        // An out-of-order arrival: a later main entry, then a backfilled subagent
        // block with earlier timestamps (read after the main file on one tick).
        tl.append_live(vec![
            stmt("2026-06-05T10:00:05.000Z"),
            stmt("2026-06-05T10:00:01.000Z"),
            stmt("2026-06-05T10:00:03.000Z"),
        ]);
        let got: Vec<_> = tl.items.iter().filter_map(|i| i.ts()).collect();
        let mut want = got.clone();
        want.sort();
        assert_eq!(
            got, want,
            "items are kept timestamp-ordered for correct seeks"
        );
        assert_eq!(tl.head_ts(), Some(ts("2026-06-05T10:00:05.000Z")));

        // A later out-of-order batch still merges into sorted order.
        tl.append_live(vec![
            stmt("2026-06-05T10:00:08.000Z"),
            stmt("2026-06-05T10:00:04.000Z"),
        ]);
        let got: Vec<_> = tl.items.iter().filter_map(|i| i.ts()).collect();
        let mut want = got.clone();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn early_journal_result_is_redated_once_its_agent_arrives() {
        let mut tl = Timeline::new();
        // The session starts at 10:00.
        tl.append_live(vec![stmt("2026-06-05T10:00:00.000Z")]);

        // A workflow journal `result` arrives BEFORE the agent's transcript is
        // discovered (journal flushed a tick earlier) — it must NOT be
        // permanently dated to the session start.
        let result_line = r#"{"type":"result","key":"v2:abcd","agentId":"aaaaaaaaaaaaaaaaa","result":{"summary":"done"}}"#;
        let entry = wire::parse_line(result_line).unwrap();
        tl.append_live(vec![
            Record::Entry {
                source: Source::Ledger("wf1".into()),
                entry,
            }
            .statement()
            .unwrap(),
        ]);
        let journal_ts = |tl: &Timeline| {
            tl.items
                .iter()
                .find(|i| i.any(|f| matches!(f.kind, FactKind::Ended(_))))
                .unwrap()
                .ts()
        };
        assert_eq!(journal_ts(&tl), None, "stays undated while agent unknown");

        // The agent's entries land on a later tick: the result is re-dated to
        // the agent's last entry, not the session start.
        let sub_line = r#"{"type":"user","uuid":"s1","parentUuid":null,"isSidechain":true,"agentId":"aaaaaaaaaaaaaaaaa","timestamp":"2026-06-05T11:30:00.000Z","message":{"role":"user","content":"task"}}"#;
        tl.append_live(vec![
            Record::Entry {
                source: Source::Sub("aaaaaaaaaaaaaaaaa".into()),
                entry: wire::parse_line(sub_line).unwrap(),
            }
            .statement()
            .unwrap(),
        ]);
        assert_eq!(
            journal_ts(&tl),
            Some(ts("2026-06-05T11:30:00.000Z")),
            "re-dated to the agent's entry, not the session start"
        );
    }

    #[test]
    fn append_live_fast_path_keeps_order_without_resort() {
        let mut tl = Timeline::new();
        // In-order timed batches take the fast path; order must still hold.
        tl.append_live(vec![
            stmt("2026-06-05T10:00:01.000Z"),
            stmt("2026-06-05T10:00:02.000Z"),
        ]);
        tl.append_live(vec![stmt("2026-06-05T10:00:03.000Z")]);
        let got: Vec<_> = tl.items.iter().filter_map(|i| i.ts()).collect();
        let mut want = got.clone();
        want.sort();
        assert_eq!(got, want);
        assert_eq!(tl.fold_target(), 3);
    }

    #[test]
    fn append_live_keeps_a_behind_the_edge_cursor_in_place() {
        let mut tl = Timeline::new();
        tl.load_replay(
            vec![
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T10:00:10.000Z"),
            ],
            8.0,
        );

        // Scrubbed back and parked: a live append advances the head but must NOT
        // move a cursor the user parked in the past.
        tl.cursor = Some(ts("2026-06-05T10:00:00.000Z"));
        tl.follow_head = false;
        tl.append_live(vec![stmt("2026-06-05T10:00:20.000Z")]);
        assert_eq!(tl.head_ts(), Some(ts("2026-06-05T10:00:20.000Z")));
        assert_eq!(
            tl.cursor,
            Some(ts("2026-06-05T10:00:00.000Z")),
            "parked cursor stays put"
        );

        // Resumed (follow_head) but still BEHIND the edge — catching up. An append
        // still must not snap the cursor to the edge; `advance` paces it forward.
        tl.follow_head = true;
        tl.append_live(vec![stmt("2026-06-05T10:00:30.000Z")]);
        assert_eq!(tl.head_ts(), Some(ts("2026-06-05T10:00:30.000Z")));
        assert_eq!(
            tl.cursor,
            Some(ts("2026-06-05T10:00:00.000Z")),
            "a catching-up cursor isn't snapped to the live edge"
        );
    }

    #[test]
    fn progress_is_event_based_and_inverts_fold_at_fraction() {
        let mut tl = Timeline::new();
        // Distinct timestamps → floor is 1 (only the first event due at the
        // start), reachable range [1, 4].
        tl.load_replay(
            vec![
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T10:00:01.000Z"),
                entry_item("2026-06-05T10:00:02.000Z"),
                entry_item("2026-06-05T10:00:03.000Z"),
            ],
            8.0,
        );
        assert_eq!(tl.floor(), 1);

        // The floor folds map to position 0; the end maps to 1.
        tl.folded = 1;
        assert!((tl.progress() - 0.0).abs() < 1e-9);
        tl.folded = 4;
        assert!((tl.progress() - 1.0).abs() < 1e-9);

        // fold_at_fraction is the inverse, over [floor, len].
        assert_eq!(tl.fold_at_fraction(0.0), 1);
        assert_eq!(tl.fold_at_fraction(1.0), 4);
        assert_eq!(tl.fold_at_fraction(0.5), 3); // 1 + round(0.5 * 3)
    }

    #[test]
    fn floor_absorbs_a_start_clump_so_the_left_reaches_zero() {
        // Several events share the first timestamp (+ an untimed leader): they
        // can only fold atomically. The bar normalizes them into position 0.
        let mut tl = Timeline::new();
        tl.load_replay(
            vec![
                meta_item(), // untimed leader
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T10:00:00.000Z"), // same instant
                entry_item("2026-06-05T10:00:05.000Z"),
            ],
            8.0,
        );
        // leader + the two same-instant events all fold together.
        assert_eq!(tl.floor(), 3);
        // The leftmost click folds exactly the floor and sits at position 0.
        let target = tl.fold_at_fraction(0.0);
        assert_eq!(target, 3);
        tl.folded = target;
        assert!((tl.progress() - 0.0).abs() < 1e-9, "left edge must reach 0");
    }

    #[test]
    fn ts_at_index_falls_back_to_start_for_leading_untimed_items() {
        let mut tl = Timeline::new();
        // An untimed leader (sorts to the front) then two timed events.
        tl.load_replay(
            vec![
                meta_item(),
                entry_item("2026-06-05T10:00:00.000Z"),
                entry_item("2026-06-05T10:00:10.000Z"),
            ],
            8.0,
        );
        // Seeking to index 0 (the untimed leader) must NOT yield a None cursor —
        // it falls back to the first real timestamp so the date shows and play
        // can advance.
        assert_eq!(tl.ts_at_index(0), Some(ts("2026-06-05T10:00:00.000Z")));
    }

    #[test]
    fn playback_reaches_the_end_across_a_huge_trailing_gap() {
        // A common shape: dense early activity, a huge idle gap, one final
        // event. Playback must traverse it and land exactly on the edge.
        let mut items = Vec::new();
        for i in 0..20 {
            items.push(entry_item(&format!("2026-06-05T10:00:{i:02}.000Z")));
        }
        items.push(entry_item("2026-06-05T20:00:00.000Z")); // ~10h later
        let head = ts("2026-06-05T20:00:00.000Z");

        let mut tl = Timeline::new();
        tl.load_replay(items, 8.0);

        let mut frames = 0;
        while !tl.at_edge() && frames < 5000 {
            tl.advance(Duration::from_millis(16), false);
            tl.folded = tl.fold_target();
            frames += 1;
        }
        assert!(tl.at_edge(), "playback stalled before the end");
        assert_eq!(tl.cursor, Some(head), "cursor must land on the edge");
    }

    #[test]
    fn just_ended_latches_once_at_replay_end() {
        let mut tl = Timeline::new();
        tl.load_replay(vec![entry_item("2026-06-05T10:00:00.000Z")], 8.0);
        tl.folded = tl.items.len();
        assert!(tl.just_ended(), "fires once when fully folded");
        assert!(!tl.just_ended(), "does not refire");
    }

    // --- property test: live delivery converges to bulk ordering -------------

    /// Deterministic PRNG (SplitMix64) — reproducible "random" interleavings
    /// without a `rand` dependency, and without `Math::random`/`Date::now`.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        /// Uniform in `0..n` (n > 0).
        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
    }

    fn main_entry(sec: u64, uid: usize) -> Statement {
        let line = format!(
            "{{\"type\":\"user\",\"uuid\":\"m{uid}\",\"parentUuid\":null,\"timestamp\":\"2026-06-05T10:00:{sec:02}.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"x\"}}}}"
        );
        Record::Entry {
            source: Source::Main,
            entry: wire::parse_line(&line).unwrap(),
        }
        .statement()
        .unwrap()
    }

    fn sub_entry(agent: &str, sec: u64, uid: usize) -> Statement {
        let line = format!(
            "{{\"type\":\"user\",\"uuid\":\"s{uid}\",\"parentUuid\":null,\"isSidechain\":true,\"agentId\":\"{agent}\",\"timestamp\":\"2026-06-05T10:00:{sec:02}.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"x\"}}}}"
        );
        Record::Entry {
            source: Source::Sub(agent.to_string()),
            entry: wire::parse_line(&line).unwrap(),
        }
        .statement()
        .unwrap()
    }

    fn meta_update(agent: &str) -> Statement {
        Record::Meta {
            agent_id: agent.to_string(),
            workflow: None,
            meta: wire::SubagentMeta {
                agent_type: Some("explorer".into()),
                description: None,
                tool_use_id: None,
                stopped_by_user: None,
            },
        }
        .statement()
        .unwrap()
    }

    fn journal_result(agent: &str) -> Statement {
        let line = format!(
            "{{\"type\":\"result\",\"key\":\"v2:x\",\"agentId\":\"{agent}\",\"result\":{{\"summary\":\"done\"}}}}"
        );
        Record::Entry {
            source: Source::Ledger("wf1".into()),
            entry: wire::parse_line(&line).unwrap(),
        }
        .statement()
        .unwrap()
    }

    /// Build one scenario deterministically from `seed` as a set of per-file
    /// streams: the main file, and 1–3 subagents each carrying `[meta, entries…,
    /// result?]`. Metas/journals are undated and must borrow their date from the
    /// agent's entries via the cross-file join; NO orphans (every agent has ≥1
    /// timed entry). Returned as streams so the live path can interleave them
    /// while preserving the two guarantees a real tailer gives (see the test):
    /// each file's lines arrive in order, and a `result` (agent finished) trails
    /// all its agent's entries. Called twice per seed (bulk + live) since
    fn scenario(seed: u64) -> Vec<Vec<Statement>> {
        let mut rng = Rng::new(seed);
        let mut uid = 0;
        let mut streams = Vec::new();

        // Main file: entries in chronological (nondecreasing) order.
        let mut main: Vec<(u64, Statement)> = (0..(2 + rng.below(4)))
            .map(|_| {
                let sec = rng.below(30) as u64;
                let u = main_entry(sec, uid);
                uid += 1;
                (sec, u)
            })
            .collect();
        main.sort_by_key(|(sec, _)| *sec);
        streams.push(main.into_iter().map(|(_, u)| u).collect());

        // Subagents: [meta, entries in ts order, result?].
        let agents = [
            "a1000000000000001",
            "a2000000000000002",
            "a3000000000000003",
        ];
        for &agent in agents.iter().take(1 + rng.below(agents.len())) {
            let mut s = vec![meta_update(agent)];
            let mut entries: Vec<(u64, Statement)> = (0..(1 + rng.below(3)))
                .map(|_| {
                    let sec = rng.below(30) as u64;
                    let u = sub_entry(agent, sec, uid);
                    uid += 1;
                    (sec, u)
                })
                .collect();
            entries.sort_by_key(|(sec, _)| *sec);
            s.extend(entries.into_iter().map(|(_, u)| u));
            if rng.below(2) == 0 {
                s.push(journal_result(agent)); // trails the entries (agent done)
            }
            streams.push(s);
        }
        streams
    }

    #[test]
    fn live_delivery_converges_to_bulk_ordering() {
        use std::collections::VecDeque;
        for seed in 0..400u64 {
            // Bulk: date + sort the whole set once (the ground truth).
            let mut bulk: Vec<ReplayItem> = scenario(seed)
                .into_iter()
                .flatten()
                .map(ReplayItem::new)
                .collect();
            crate::tailer::date_and_sort(&mut bulk);
            let bulk_ts: Vec<_> = bulk.iter().map(|i| i.ts()).collect();

            // Live: interleave the per-file streams randomly — the cross-file,
            // cross-tick race the tailer sees — but preserve each stream's
            // internal order (a file's lines arrive in order; a `result` trails
            // its agent's entries). Then chop the interleaving into arbitrary
            // batches and deliver incrementally.
            let mut rng = Rng::new(seed ^ 0x00AB_CDEF);
            let mut decks: Vec<VecDeque<Statement>> =
                scenario(seed).into_iter().map(VecDeque::from).collect();
            let mut merged = Vec::new();
            loop {
                let live: Vec<usize> = decks
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| !d.is_empty())
                    .map(|(i, _)| i)
                    .collect();
                let Some(&pick) = live.get(rng.below(live.len().max(1))) else {
                    break;
                };
                merged.push(decks[pick].pop_front().unwrap());
            }
            let mut tl = Timeline::new();
            let mut rest = merged;
            while !rest.is_empty() {
                let take = (1 + rng.below(3)).min(rest.len());
                let tail = rest.split_off(take);
                tl.append_live(rest);
                rest = tail;
            }
            let live_ts: Vec<_> = tl.items.iter().map(|i| i.ts()).collect();

            // No orphans → every undated item resolves; and any realistic
            // interleaving must land the same timestamp sequence as bulk.
            assert!(
                tl.items.iter().all(|i| i.ts().is_some()),
                "seed {seed}: an item stayed undated (unexpected orphan)"
            );
            assert_eq!(
                bulk_ts, live_ts,
                "seed {seed}: live delivery diverged from bulk ordering"
            );
        }
    }
}
