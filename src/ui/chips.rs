//! Ephemeral tool-call chips — overlay decorations anchored to agent nodes.
//!
//! Chips are deliberately NOT graph nodes: they don't participate in layout,
//! fit-view bounds, the minimap, or hit testing, so they can pop and fade
//! without disturbing the graph. A chip spawns below an agent's card when a
//! tool call is observed and reads its live state (pending/✓/✗) from the model
//! at render time. Pending chips persist (they are the in-flight indicator);
//! completed ones fade from the moment completion was observed, then expire.
//! Anchoring uses `Flow::node_terminal_rect`; writes are clipped per cell with
//! `Flow::is_in_bounds` (the escape-hatch contract).
//!
//! **When a chip appears is content-timed; how long it lingers is a viewing
//! window.** A chip spawns the instant its tool folds (content-timed, automatic
//! — no clock needed). But its fade *duration* is a human-perception budget
//! (~2.5s to catch it), so it ages in **playing wall-time** (`reconcile`): real
//! time while the playhead advances, frozen when paused or scrubbed. This is
//! deliberately NOT media-time — under gap-compression the playhead fast-
//! forwards through dead air, and a media-time afterglow would be raced to zero
//! mid-gap (a run flickering out just before the next cluster extends it). A
//! bounded gap-crossing (log-compressed to ~2s) is shorter than the TTL, so the
//! run survives the gap and keeps aggregating; a genuinely long pause still
//! fades it. Aggregation (runs, below) is what makes a wall-time afterglow
//! churn-free at any speed. Chrome (camera glide, marching ants) also uses
//! wall-time; only the playhead itself is media-timed.
//!
//! `reconcile`: ChipTray::reconcile

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rataflow::Palette;
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

use crate::state::graph::AgentFlow;
use crate::state::session::{SessionModel, ToolState};

/// How long a successful chip stays visible AFTER completion. Pending chips
/// never expire — they are the in-flight indicator (the per-agent cap bounds
/// them) — and the fade clock starts at completion so every tool gets the
/// same bright-✓ afterglow regardless of how long it ran.
const CHIP_TTL: Duration = Duration::from_millis(2500);
/// Failed chips linger longer — the glanceable "something went wrong" moment.
const CHIP_TTL_ERR: Duration = Duration::from_millis(4000);
/// Max simultaneously visible chips per agent (newest win).
const MAX_PER_AGENT: usize = 3;
/// Minimum node width (terminal cells) to bother drawing chips under —
/// DERIVED from the card cell threshold so chips disappear exactly when the
/// card drops to cell level (a solid block with full-size text dangling under
/// it would look broken).
const MIN_NODE_WIDTH: i32 = crate::ui::nodes::CELL_MIN_WIDTH as i32;

/// Time-to-live for a chip in `state`.
fn ttl(state: ToolState) -> Duration {
    match state {
        ToolState::Err => CHIP_TTL_ERR,
        ToolState::Pending | ToolState::Ok => CHIP_TTL,
    }
}

/// One ephemeral chip — a **run of consecutive same-name tool calls** for one
/// agent, collapsed into a single overlay (`⚒ bash ×5`). Aggregation is what
/// keeps a busy agent's chips from churning through the per-agent cap: a burst
/// of 20 reads is one chip that counts up, not 20 that flash past. The run is a
/// contiguous index range `[start, start+count)` into the agent's `tool_calls`;
/// aggregate state (pending/✓/✗) is derived from those calls at render time, so
/// the chip flips in place, and the fade clock anchors to that flip.
struct Chip {
    agent_id: String,
    /// The shared tool name of every call in the run.
    name: String,
    /// First `tool_calls` index in the run, and how many calls it spans.
    start: usize,
    count: usize,
    /// Afterglow elapsed since the run last settled, accumulated in **playing
    /// wall-time** by `reconcile`(ChipTray::reconcile) (frozen while paused/scrubbed).
    /// `None` while any call is still pending — the run stays bright until the
    /// burst settles.
    afterglow: Option<Duration>,
}

/// Display state of a chip's run: `Pending` while ANY call is in flight, else
/// `Err` if any failed, else `Ok`. `None` if the calls no longer exist (a
/// session-reset race) — the chip is then dropped.
///
/// Pending-first shows the honest per-call lifecycle — a run is **yellow while
/// it's working** and settles to green/red when done — rather than jumping
/// straight to "done" the instant one call finishes. `reconcile` only starts a
/// run's fade once it is fully settled, so a burst stays bright until it truly
/// finishes.
fn group_state(
    model: &SessionModel,
    agent_id: &str,
    start: usize,
    count: usize,
) -> Option<ToolState> {
    let tool_calls = &model.agent(agent_id)?.tool_calls;
    // Out-of-range is `None`, matching the previous `get(range)?` — a truncated
    // run means the model was rebuilt underneath us.
    if count == 0 || start + count > tool_calls.len() {
        return None;
    }
    // `Vector::slice` splits in place and needs ownership, and `skip(start)` on
    // the iterator walks every element it discards — O(start) per chip, per
    // frame. A focus seeks the range in O(log n) instead.
    let focus = tool_calls.focus();
    let calls = || focus.clone().narrow(start..start + count).into_iter();
    if calls().any(|c| c.state == ToolState::Pending) {
        Some(ToolState::Pending)
    } else if calls().any(|c| c.state == ToolState::Err) {
        Some(ToolState::Err)
    } else {
        Some(ToolState::Ok)
    }
}

/// Two consecutive same-name calls farther apart than this (in media time) are
/// separate runs rather than one aggregate — this bounds run growth and gives
/// per-burst grouping, so a read now and a read a minute later don't collapse
/// into one ever-growing `read ×N`. Because the boundary is derived from the
/// calls' own timestamps (not animation history), the grouping is a pure
/// function of model state — the same on forward playback and on a seek. Calls
/// without timestamps (unit fixtures) always merge.
///
/// Deliberately equal to [`CHIP_TTL`]: once a run has been quiet long enough to
/// fade out, the next same-name call is also beyond the gap, so it opens a fresh
/// run instead of resurrecting the faded one.
const RUN_GAP: chrono::Duration = chrono::Duration::milliseconds(2500);

/// Whether two consecutive calls belong to the same run: same only if their
/// timestamps are within [`RUN_GAP`]. A missing timestamp can't split (merge).
fn within_gap(prev: Option<DateTime<Utc>>, next: Option<DateTime<Utc>>) -> bool {
    match (prev, next) {
        (Some(p), Some(n)) => (n - p).abs() <= RUN_GAP,
        _ => true,
    }
}

/// Chip bookkeeping: the currently-shown chips plus the per-agent high-water
/// mark that separates new activity from already-seen history. Owned by `App`;
/// driven every frame by the single `reconcile`(Self::reconcile) pass and
/// re-baselined on attach/seek by [`adopt_baseline`](Self::adopt_baseline);
/// drawn by [`render`].
#[derive(Default)]
pub struct ChipTray {
    chips: Vec<Chip>,
    /// Per agent, how many of its `tool_calls` `reconcile` has already accounted
    /// for. This is the line between *new* completions (past the mark → animate
    /// their afterglow) and *history* (below it → don't replay). Tool calls are
    /// append-only, so a plain count suffices.
    seen: HashMap<String, usize>,
    /// Whether [`adopt_baseline`](Self::adopt_baseline) has run for the current
    /// session (the live-attach backfill has been absorbed).
    seeded: bool,
}

impl ChipTray {
    /// Whether [`adopt_baseline`](Self::adopt_baseline) has run for the session.
    pub fn is_seeded(&self) -> bool {
        self.seeded
    }

    /// Absorb the model's current tool calls as history WITHOUT animating them —
    /// used on the live-attach backfill and after a seek. It only moves the
    /// `seen` mark to the end and clears the tray; the next `reconcile` then
    /// re-derives what to show from state. The effect: completed tools below the
    /// mark are silent history, but in-flight (pending) tools still surface —
    /// a pending tool is state ("running right now"), so it must reappear at any
    /// playhead inside its interval, however you scrubbed there.
    pub fn adopt_baseline(&mut self, model: &SessionModel) {
        self.chips.clear();
        self.seen.clear();
        for id in &model.spawn_order {
            if let Some(info) = model.agent(id) {
                self.seen.insert(id.clone(), info.tool_calls.len());
            }
        }
        self.seeded = true;
    }

    /// The single reconcile pass — derive the whole chip set from model state
    /// each frame, aging completed-run afterglows in **playing wall-time** (`dt`
    /// accrues only while `playing`, so a chip you pause/scrub on freezes).
    ///
    /// Runs are grouped purely from state (`within_gap`); afterglows are
    /// carried across the rebuild by run identity `(agent, start)`. A run shows
    /// as:
    /// - **pending** (bright) whenever any call is in flight — so seeking into a
    ///   running tool's interval always reconstructs it;
    /// - **fading** once settled, but only if we were already tracking it (it was
    ///   pending and just settled) or it's a genuinely new completion (past the
    ///   `seen` mark, e.g. born-completed in replay);
    /// - **hidden** if it's completed history already below the mark (a seek/
    ///   attach absorbed it) — its afterglow is a forward-only animation.
    pub fn reconcile(&mut self, dt: Duration, playing: bool, model: &SessionModel) {
        // Age existing afterglows in playing wall-time.
        if playing {
            for chip in &mut self.chips {
                if let Some(age) = &mut chip.afterglow {
                    *age += dt;
                }
            }
        }
        // Snapshot the prior tray by run identity so the rebuild can carry
        // afterglows across it. We keep each run's (count, afterglow): the count
        // detects a run that gained a member (a fresh call joined a same-name
        // burst) so it re-anchors bright instead of aging out mid-burst.
        let prior: HashMap<(String, usize), (usize, Option<Duration>)> = self
            .chips
            .drain(..)
            .map(|c| ((c.agent_id, c.start), (c.count, c.afterglow)))
            .collect();

        for id in &model.spawn_order {
            let Some(info) = model.agent(id) else {
                continue;
            };
            let seen = self.seen.get(id).copied().unwrap_or(0);
            let calls = &info.tool_calls;
            let mut i = 0;
            while i < calls.len() {
                // One run: a maximal group of consecutive same-name calls within
                // RUN_GAP of each other.
                let name = &calls[i].name;
                let start = i;
                i += 1;
                while i < calls.len()
                    && calls[i].name == *name
                    && within_gap(calls[i - 1].ts, calls[i].ts)
                {
                    i += 1;
                }
                let count = i - start;
                // Ranged, not `skip(start)`: the latter walks every element
                // it discards, once per run, on every frame.
                let settled = !calls
                    .focus()
                    .narrow(start..start + count)
                    .into_iter()
                    .any(|c| c.state == ToolState::Pending);
                let prev = prior.get(&(id.clone(), start)).copied();

                // Afterglow decision. `Some(inner)` shows the run with that chip
                // afterglow (`None` inner = bright/pending); the outer `None`
                // hides it entirely.
                let afterglow: Option<Option<Duration>> = if !settled {
                    Some(None) // in flight → shown bright
                } else {
                    match prev {
                        // Untracked new completion past the mark → animate;
                        // completed history below the mark → hidden.
                        None => (start + count > seen).then_some(Some(Duration::ZERO)),
                        // Tracked run that gained a member, or one that was
                        // pending and just settled → re-anchor a fresh afterglow;
                        // otherwise (unchanged, already fading) keep aging.
                        Some((pc, pa)) => Some(if count > pc || pa.is_none() {
                            Some(Duration::ZERO)
                        } else {
                            pa
                        }),
                    }
                };
                let Some(afterglow) = afterglow else {
                    continue;
                };
                // Drop an afterglow that has already faded out.
                if let Some(age) = afterglow {
                    let state = group_state(model, id, start, count);
                    if state.is_some_and(|s| age >= ttl(s)) {
                        continue;
                    }
                }
                self.chips.push(Chip {
                    agent_id: id.clone(),
                    name: name.clone(),
                    start,
                    count,
                    afterglow,
                });
            }
            self.seen.insert(id.clone(), calls.len());
        }

        // A pending run persists until its owner is AUTHORITATIVELY finished
        // (`terminal`: a sync ack / task-notification). We deliberately do NOT
        // drop it on the reversible 120s-quiet "Done", because that heuristic is
        // wrong for exactly the case that matters: a subagent blocked on a long
        // tool looks quiet, but its tool is still in flight and will still
        // resolve. Dropping the chip there made the tool vanish instead of
        // settling to ✓/✗ — a real error even went unshown. A pending tool_call
        // is stronger evidence of "working" than "no output for 120s" is of
        // "done". (Completed runs are gated by their afterglow above.)
        self.chips.retain(|chip| {
            chip.afterglow.is_some() || model.agent(&chip.agent_id).is_some_and(|a| !a.terminal)
        });

        // Per-agent cap: keep the newest MAX_PER_AGENT COMPLETED runs of each
        // agent. Pending runs are exempt — they are the in-flight truth — and
        // with aggregation this rarely bites (a run, not a call, per name).
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut keep = vec![true; self.chips.len()];
        for (i, chip) in self.chips.iter().enumerate().rev() {
            if chip.afterglow.is_none() {
                continue; // pending → always kept
            }
            let count = counts.entry(chip.agent_id.as_str()).or_insert(0);
            if *count < MAX_PER_AGENT {
                *count += 1;
            } else {
                keep[i] = false;
            }
        }
        drop(counts);
        let mut keep = keep.into_iter();
        self.chips.retain(|_| keep.next().unwrap_or(true));
    }

    /// Forget everything (session switch/reset).
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Compact tool-duration label: `847ms`, `1.2s`, `42s`, `2m3s`. Sub-second in
/// milliseconds (most tools are), one decimal under 10s, then whole seconds,
/// then `m`/`s` — readable at a glance in the narrow chip.
fn fmt_dur(d: chrono::Duration) -> String {
    let ms = d.num_milliseconds().max(0);
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else {
        let s = ms / 1000;
        format!("{}m{}s", s / 60, s % 60)
    }
}

/// Draw the live chips below their agents' cards.
///
/// Call right after rendering the flow in the same draw pass so the anchors
/// are frame-exact; writes are clipped per cell with `is_in_bounds`. `now` is
/// the timeline's `now_reference` — a single-tool chip shows its duration, which
/// live-ticks against `now` while the tool is still pending.
pub fn render(
    tray: &ChipTray,
    flow: &AgentFlow,
    model: &SessionModel,
    now: Option<DateTime<Utc>>,
    buf: &mut Buffer,
) {
    let palette = flow.theme.palette();
    // Stack slot per agent: successive chips of one agent go one row lower.
    let mut slots: HashMap<&str, i32> = HashMap::new();

    for chip in &tray.chips {
        let Some(state) = group_state(model, &chip.agent_id, chip.start, chip.count) else {
            continue;
        };
        // Pending runs persist (age 0); completed ones fade over their afterglow.
        let age = chip.afterglow.unwrap_or(Duration::ZERO);
        if chip.afterglow.is_some() && age >= ttl(state) {
            continue;
        }
        let Some((left, _, right, bottom)) = flow.node_terminal_rect(&chip.agent_id) else {
            continue;
        };
        // Zoomed far out, a full-size chip dwarfs its card — skip rather than
        // dangle text off a sliver of a node.
        if right - left < MIN_NODE_WIDTH {
            continue;
        }
        let slot = slots
            .entry(chip.agent_id.as_str())
            .and_modify(|s| *s += 1)
            .or_insert(0);
        let y = bottom + *slot;

        // Body fades; the state glyph keeps the status-color language so the
        // chip's ✓/✗ matches the panel's at every instant. A run of >1 call
        // shows its count (`⚒ bash ×5`); a single tool shows its duration
        // (`⚒ bash 0.5s`), live-ticking against `now` while it's still pending.
        let body_style = chip_style(state, age, &palette);
        let body = if chip.count > 1 {
            format!("⚒ {} ×{}", chip.name, chip.count)
        } else {
            let dur = model
                .agent(&chip.agent_id)
                .and_then(|a| a.tool_calls.get(chip.start))
                .and_then(|tc| tc.duration(now))
                .map(|d| format!(" {}", fmt_dur(d)))
                .unwrap_or_default();
            format!("⚒ {}{}", chip.name, dur)
        };
        let glyph: Option<(&str, Style)> = match state {
            ToolState::Pending => None,
            ToolState::Ok => Some((" ✓", body_style.fg(palette.success))),
            ToolState::Err => Some((
                " ✗",
                body_style.fg(palette.error).add_modifier(Modifier::BOLD),
            )),
        };
        let mut cells: Vec<(char, Style)> = body.chars().map(|c| (c, body_style)).collect();
        if let Some((g, gs)) = glyph {
            cells.extend(g.chars().map(|c| (c, gs)));
        }
        for (i, (ch, style)) in cells.into_iter().enumerate() {
            let x = left + 1 + i as i32;
            if flow.is_in_bounds(x, y) {
                buf[(x as u16, y as u16)].set_char(ch).set_style(style);
            }
        }
    }
}

/// Chip style by state and age: pending is prominent, success fades through
/// the palette (text → subtle → muted), failure stays error-red until expiry.
fn chip_style(state: ToolState, age: Duration, palette: &Palette) -> Style {
    match state {
        ToolState::Err => Style::default()
            .fg(palette.error)
            .add_modifier(Modifier::BOLD),
        ToolState::Pending => Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
        ToolState::Ok => {
            let f = age.as_secs_f64() / CHIP_TTL.as_secs_f64();
            let color = if f < 0.45 {
                palette.text
            } else if f < 0.75 {
                palette.subtle
            } else {
                palette.muted
            };
            Style::default().fg(color)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::claude::wire::SubagentMeta;
    use crate::state::session::SessionModel;

    /// A model with one subagent carrying `n` tool calls.
    fn model_with_tools(n: usize) -> SessionModel {
        let mut m = SessionModel::new("s1".into());
        let meta = SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("t1".into()),
            stopped_by_user: None,
        };
        m.apply_meta("sub1", None, &meta);
        let agent = m.agents.get_mut("sub1").unwrap();
        for i in 0..n {
            agent
                .tool_calls
                .push_back(crate::state::session::ToolCallInfo {
                    id: format!("toolu_{i}"),
                    name: "Bash".into(),
                    summary: None,
                    ts: None,
                    end_ts: None,
                    state: ToolState::Pending,
                });
        }
        m
    }

    /// A model with one subagent whose tool calls have the given `names`, in
    /// order — for exercising run-grouping and the per-agent cap on runs.
    fn model_with_named_tools(names: &[&str]) -> SessionModel {
        let mut m = SessionModel::new("s1".into());
        let meta = SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("t1".into()),
            stopped_by_user: None,
        };
        m.apply_meta("sub1", None, &meta);
        let agent = m.agents.get_mut("sub1").unwrap();
        for (i, name) in names.iter().enumerate() {
            agent
                .tool_calls
                .push_back(crate::state::session::ToolCallInfo {
                    id: format!("toolu_{i}"),
                    name: (*name).into(),
                    summary: None,
                    ts: None,
                    end_ts: None,
                    state: ToolState::Pending,
                });
        }
        m
    }

    /// `model` with every tool call marked `Ok` — the common replay case where
    /// a call and its result fold together (born completed).
    fn all_ok(mut m: SessionModel) -> SessionModel {
        for tc in &mut m.agents.get_mut("sub1").unwrap().tool_calls {
            tc.state = ToolState::Ok;
        }
        m
    }

    /// Stamp the agent's tool calls with timestamps (seconds from a fixed base),
    /// so `within_gap` can split runs on real gaps. `secs.len()` should match
    /// the call count; extra calls keep `None`.
    fn at_secs(mut m: SessionModel, secs: &[i64]) -> SessionModel {
        let base = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let calls = &mut m.agents.get_mut("sub1").unwrap().tool_calls;
        for (tc, s) in calls.iter_mut().zip(secs) {
            tc.ts = Some(base + chrono::Duration::seconds(*s));
        }
        m
    }

    /// One reconcile at the current playhead (no wall-time elapsed) — the "a
    /// fold happened, redraw" case.
    fn observe(tray: &mut ChipTray, model: &SessionModel) {
        tray.reconcile(Duration::ZERO, true, model);
    }

    /// Advance the tray by `secs` of *playing* wall-time (frames while following).
    fn play(tray: &mut ChipTray, secs: u64, model: &SessionModel) {
        tray.reconcile(Duration::from_secs(secs), true, model);
    }

    #[test]
    fn reconcile_aggregates_consecutive_same_name_calls() {
        let mut tray = ChipTray::default();
        // Two consecutive Bash calls collapse into ONE run of count 2.
        observe(&mut tray, &model_with_tools(2));
        assert_eq!(tray.chips.len(), 1);
        assert_eq!(tray.chips[0].count, 2);

        // Same model again: nothing new.
        observe(&mut tray, &model_with_tools(2));
        assert_eq!(tray.chips.len(), 1);
        assert_eq!(tray.chips[0].count, 2);

        // One more Bash extends the run to 3 — no new chip, no churn.
        observe(&mut tray, &model_with_tools(3));
        assert_eq!(tray.chips.len(), 1);
        assert_eq!(tray.chips[0].count, 3);
    }

    #[test]
    fn reconcile_opens_a_new_run_on_a_different_name() {
        let mut tray = ChipTray::default();
        // bash, bash, read, read → two runs of 2 (order preserved).
        observe(
            &mut tray,
            &model_with_named_tools(&["bash", "bash", "read", "read"]),
        );
        assert_eq!(tray.chips.len(), 2);
        assert_eq!(
            (tray.chips[0].name.as_str(), tray.chips[0].count),
            ("bash", 2)
        );
        assert_eq!(
            (tray.chips[1].name.as_str(), tray.chips[1].count),
            ("read", 2)
        );
    }

    #[test]
    fn a_burst_of_same_name_calls_is_one_counting_chip() {
        let mut tray = ChipTray::default();
        // 20 consecutive Bash, all completed in one batch — the churn case.
        observe(&mut tray, &all_ok(model_with_tools(20)));
        assert_eq!(tray.chips.len(), 1, "a burst is one chip, not 20 churning");
        assert_eq!(tray.chips[0].count, 20);
    }

    #[test]
    fn a_real_time_gap_splits_a_run() {
        // Same name, but a gap beyond RUN_GAP → two runs, not one aggregate.
        // (Timestamps drive the split; without them calls always merge.)
        let mut tray = ChipTray::default();
        let model = at_secs(all_ok(model_with_tools(3)), &[0, 1, 100]);
        observe(&mut tray, &model);
        assert_eq!(tray.chips.len(), 2, "the 100s gap opens a second run");
        assert_eq!((tray.chips[0].start, tray.chips[0].count), (0, 2));
        assert_eq!((tray.chips[1].start, tray.chips[1].count), (2, 1));
    }

    #[test]
    fn a_growing_run_re_anchors_bright_instead_of_aging_out() {
        // A same-name burst that keeps gaining members must not fade mid-burst:
        // each new call re-anchors the afterglow to zero.
        let mut tray = ChipTray::default();
        // Cluster 1 completes → run of 2, afterglow starts at zero.
        observe(&mut tray, &all_ok(model_with_tools(2)));
        assert_eq!(tray.chips[0].count, 2);

        // Two playing seconds pass — under the 2.5s TTL, still alive.
        play(&mut tray, 2, &all_ok(model_with_tools(2)));
        assert_eq!(tray.chips.len(), 1, "run survives the gap");

        // Cluster 2 (same name, no gap) extends the SAME run and re-anchors it
        // bright — no fresh chip, and the fade clock resets.
        observe(&mut tray, &all_ok(model_with_tools(4)));
        assert_eq!(tray.chips.len(), 1);
        assert_eq!(tray.chips[0].count, 4, "extended, not a fresh chip");
        assert_eq!(
            tray.chips[0].afterglow,
            Some(Duration::ZERO),
            "a new member re-anchors the afterglow"
        );
    }

    #[test]
    fn a_run_reads_pending_while_in_flight_then_settles() {
        // The honest per-call lifecycle: a run reads Pending (yellow) while any
        // call is in flight, and stays bright (no afterglow), then settles to
        // Ok/Err and begins fading once every call has returned.
        let mut tray = ChipTray::default();
        let mut model = model_with_tools(3);
        let calls = &mut model.agents.get_mut("sub1").unwrap().tool_calls;
        calls[0].state = ToolState::Ok;
        calls[1].state = ToolState::Ok;
        // calls[2] stays Pending — the in-flight tail of the burst.
        observe(&mut tray, &model);
        assert_eq!(tray.chips.len(), 1);
        assert_eq!(tray.chips[0].count, 3);
        assert_eq!(
            group_state(&model, "sub1", 0, 3),
            Some(ToolState::Pending),
            "an in-flight run reads pending, not done"
        );
        assert_eq!(
            tray.chips[0].afterglow, None,
            "an unsettled run stays bright (no fade until every call returns)"
        );

        // The tail returns → the run settles → reads Ok and the afterglow starts.
        model.agents.get_mut("sub1").unwrap().tool_calls[2].state = ToolState::Ok;
        observe(&mut tray, &model);
        assert_eq!(group_state(&model, "sub1", 0, 3), Some(ToolState::Ok));
        assert_eq!(tray.chips[0].afterglow, Some(Duration::ZERO));
    }

    #[test]
    fn per_agent_cap_exempts_pending_and_keeps_newest_completed_runs() {
        let mut tray = ChipTray::default();

        // Five distinct-name calls → five separate runs, all pending: the cap
        // must not evict in-flight indicators.
        let names = ["a", "b", "c", "d", "e"];
        observe(&mut tray, &model_with_named_tools(&names));
        assert_eq!(tray.chips.len(), 5, "pending runs are cap-exempt");

        // All complete: the cap applies — newest MAX_PER_AGENT runs survive.
        observe(&mut tray, &all_ok(model_with_named_tools(&names)));
        assert_eq!(tray.chips.len(), MAX_PER_AGENT);
        let kept: Vec<&str> = tray.chips.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(kept, vec!["c", "d", "e"], "newest runs kept");
    }

    #[test]
    fn afterglow_ages_in_playing_time_and_expires() {
        let mut tray = ChipTray::default();
        let model = all_ok(model_with_tools(1));
        observe(&mut tray, &model);
        assert_eq!(
            tray.chips[0].afterglow,
            Some(Duration::ZERO),
            "afterglow starts at zero on completion"
        );

        // Two playing seconds → still within the 2.5s TTL.
        play(&mut tray, 2, &model);
        assert_eq!(tray.chips.len(), 1);
        // One more → past 2.5s → pruned.
        play(&mut tray, 1, &model);
        assert!(
            tray.chips.is_empty(),
            "expired 3 playing-seconds after completion"
        );
    }

    #[test]
    fn afterglow_freezes_when_not_playing() {
        // Paused/scrubbed: the afterglow must not age, so a chip you pause on
        // stays put.
        let mut tray = ChipTray::default();
        let model = all_ok(model_with_tools(1));
        observe(&mut tray, &model);
        tray.reconcile(Duration::from_secs(60), false, &model); // not playing
        assert_eq!(tray.chips.len(), 1, "frozen while paused");
        assert_eq!(tray.chips[0].afterglow, Some(Duration::ZERO));
        // Resume playing → it ages and eventually expires.
        play(&mut tray, 3, &model);
        assert!(tray.chips.is_empty());
    }

    #[test]
    fn pending_runs_persist_then_anchor_on_completion() {
        let mut tray = ChipTray::default();
        let mut model = model_with_tools(1);
        observe(&mut tray, &model);
        assert_eq!(tray.chips[0].afterglow, None, "pending → no afterglow yet");

        // Pending never expires, even after a long playing stretch.
        play(&mut tray, 60, &model);
        assert_eq!(tray.chips.len(), 1, "pending runs never expire");
        assert_eq!(tray.chips[0].afterglow, None);

        // Completes → the afterglow starts at zero (a fresh viewing window).
        model.agents.get_mut("sub1").unwrap().tool_calls[0].state = ToolState::Ok;
        observe(&mut tray, &model);
        assert_eq!(tray.chips[0].afterglow, Some(Duration::ZERO));
    }

    #[test]
    fn a_new_call_after_the_run_faded_starts_a_fresh_run() {
        let mut tray = ChipTray::default();
        // One completed Bash at t=0.
        let m1 = at_secs(all_ok(model_with_tools(1)), &[0]);
        observe(&mut tray, &m1);
        assert_eq!(tray.chips[0].afterglow, Some(Duration::ZERO));

        // Age past the TTL → faded out.
        play(&mut tray, 5, &m1);
        assert!(tray.chips.is_empty());

        // A second Bash far past the run gap (t=100s) → a fresh run at the new
        // index, NOT a resurrected ×2.
        let m2 = at_secs(all_ok(model_with_tools(2)), &[0, 100]);
        observe(&mut tray, &m2);
        assert_eq!(tray.chips.len(), 1);
        assert_eq!(
            tray.chips[0].start, 1,
            "fresh run at the new call, not extended"
        );
        assert_eq!(tray.chips[0].count, 1);
    }

    #[test]
    fn reconcile_drops_orphaned_chips_after_reset() {
        let mut tray = ChipTray::default();
        observe(&mut tray, &model_with_tools(2));
        assert_eq!(tray.chips.len(), 1); // two Bash → one run

        // A fresh model without the agent: chips are orphaned.
        observe(&mut tray, &SessionModel::new("s2".into()));
        assert!(tray.chips.is_empty());
    }

    #[test]
    fn a_running_owners_in_flight_run_survives_ticks() {
        // A pending run of a RUNNING owner must survive frame ticks (pending
        // never ages) — otherwise an in-flight burst flickers out between folds
        // (the "no chips for a busy subagent" bug). Liveness only removes a
        // pending run once its owner stops running (next test).
        let mut tray = ChipTray::default();
        let model = model_with_tools(1); // pending, owner Running (default)
        observe(&mut tray, &model);
        assert_eq!(tray.chips.len(), 1);

        play(&mut tray, 5, &model);
        assert_eq!(
            tray.chips.len(),
            1,
            "a running owner's in-flight run persists"
        );
        assert_eq!(tray.chips[0].afterglow, None);
    }

    #[test]
    fn pending_chips_survive_quiet_but_die_when_terminal() {
        let mut tray = ChipTray::default();
        let mut model = model_with_tools(1);
        observe(&mut tray, &model);
        assert_eq!(tray.chips.len(), 1);

        // Owner reads as Done via the reversible 120s-quiet heuristic (terminal
        // still false): its tool may still be in flight (a long-running Bash),
        // so the in-flight chip MUST persist — else its eventual ✓/✗ is lost.
        model.agents.get_mut("sub1").unwrap().status = crate::state::session::AgentStatus::Done;
        observe(&mut tray, &model);
        assert_eq!(
            tray.chips.len(),
            1,
            "a quiet-but-not-terminal owner keeps its in-flight chip"
        );

        // Owner is now AUTHORITATIVELY finished (sync ack / task-notification):
        // the pending tool truly isn't coming → drop it.
        model.agents.get_mut("sub1").unwrap().terminal = true;
        observe(&mut tray, &model);
        assert!(
            tray.chips.is_empty(),
            "a terminal owner's dangling pending tool is dropped"
        );
    }

    #[test]
    fn failed_chips_outlive_ok_chips() {
        assert!(ttl(ToolState::Err) > ttl(ToolState::Ok));
    }

    #[test]
    fn fmt_dur_reads_at_a_glance() {
        use chrono::Duration as D;
        assert_eq!(fmt_dur(D::milliseconds(3)), "3ms");
        assert_eq!(fmt_dur(D::milliseconds(847)), "847ms");
        assert_eq!(fmt_dur(D::milliseconds(1234)), "1.2s"); // one decimal under 10s
        assert_eq!(fmt_dur(D::seconds(42)), "42s"); // whole seconds under a minute
        assert_eq!(fmt_dur(D::seconds(143)), "2m23s"); // m/s over a minute
        assert_eq!(fmt_dur(D::milliseconds(-5)), "0ms"); // clamps negative
    }

    #[test]
    fn adopt_baseline_hides_completed_history_but_reconcile_shows_new_work() {
        let mut tray = ChipTray::default();
        assert!(!tray.is_seeded());

        // Attach onto two already-completed tools: absorbed as silent history —
        // adopt_baseline itself spawns nothing, and the next reconcile keeps
        // them hidden (their afterglow is a forward-only animation).
        let hist = all_ok(model_with_tools(2));
        tray.adopt_baseline(&hist);
        assert!(tray.is_seeded());
        assert!(tray.chips.is_empty(), "baseline itself spawns nothing");
        observe(&mut tray, &hist);
        assert!(tray.chips.is_empty(), "completed history must not chip");

        // A genuinely new tool, past the run gap, chips on its own.
        let more = at_secs(all_ok(model_with_tools(3)), &[0, 1, 100]);
        observe(&mut tray, &more);
        assert_eq!(tray.chips.len(), 1);
        assert_eq!(tray.chips[0].start, 2);
        assert_eq!(tray.chips[0].count, 1);
    }

    #[test]
    fn reconcile_reconstructs_in_flight_pending_runs_after_a_seek() {
        // Seeking into the middle of a running tool must show it — a pending tool
        // is state, not an animation. adopt_baseline absorbs the two completed
        // calls; the next reconcile reconstructs the still-pending tail (split
        // off by the time gap) as a bright chip.
        let mut tray = ChipTray::default();
        let mut model = at_secs(model_with_tools(3), &[0, 1, 100]);
        let calls = &mut model.agents.get_mut("sub1").unwrap().tool_calls;
        calls[0].state = ToolState::Ok;
        calls[1].state = ToolState::Ok;
        // calls[2] stays Pending — the long-running, in-flight tool.
        tray.adopt_baseline(&model);
        assert!(tray.chips.is_empty(), "baseline itself spawns nothing");
        observe(&mut tray, &model);
        assert_eq!(tray.chips.len(), 1, "the in-flight tool is reconstructed");
        assert_eq!(
            tray.chips[0].start, 2,
            "only the pending tail, not the done history"
        );
        assert_eq!(tray.chips[0].afterglow, None, "shown as pending (bright)");
    }

    #[test]
    fn clear_resets_seeding() {
        let mut tray = ChipTray::default();
        tray.adopt_baseline(&model_with_tools(1));
        assert!(tray.is_seeded());
        tray.clear();
        assert!(
            !tray.is_seeded(),
            "a new session needs a fresh backfill baseline"
        );
    }
}
