# zoetrope — Architecture & Principles

This document captures the *why* — the invariants, the recurring principles, and
the derived-state heuristics that hold the system together. [`DESIGN.md`](DESIGN.md)
is the structural spec (module map, transcript format, type shapes); this is the
reasoning that governs how those pieces are allowed to behave.

The whole program solves one hard problem:

> Reconstruct a faithful, navigable, live-or-replayed picture of a Claude Code
> agent session from an **undocumented, append-only, partially-timestamped,
> multi-file** transcript — in which **completion is frequently unknowable**.

Almost every design decision below is downstream of that one sentence. Watching
*every* live session at once (§8) adds a second: the same reconstruction, N times
over, cheaply enough that the sessions nobody is looking at cost almost nothing.

---

## 1. The two ground rules

### 1.1 Order-independence (the fold model)

The derived `SessionModel` is a **pure function of the set of facts folded into
it — never of their arrival order.** Every update is *idempotent* (re-applying a
line is a no-op) and *commutative* (two updates reach the same state in either
order). The completion/spawn joins are keyed stores (`completed_spawns`,
`task_terminal`, `journal_done`) precisely so a fact can arrive before *or* after
the thing it refers to and still attach; prompt/era attribution is likewise
arrival-order independent — timestamp-derived (`prompt_for_ts` over the `prompts`
list), not a join store.

Why this is non-negotiable — three independent consumers demand it:

- **Multi-file merge.** Main transcript, `subagents/*.jsonl`, and workflow
  `journal.jsonl` are tailed separately and interleave by timestamp; a subagent's
  result can be read before its spawn.
- **Backward seek.** Folding is forward-only, so seeking into the past *rebuilds*
  the model from `items[0..target]` from scratch. That rebuild must land on
  exactly the state that playing there would have.
- **Live vs replay.** The same items arrive as a bulk sorted `Vec` (replay) or as
  arrival-order appends (live). Both must converge.

Guarded by a **shuffle-invariance property test**: fold a real stream in bulk
order and in many shuffled orders; the final model must be identical.

**Corollary — derived state is never monotonic.** A rollup that concluded "done"
must *revert* when a contradicting fact (a late child, resumed activity) arrives.
Derived state is a function of the current fact set, never of derivation history.

### 1.2 Two clocks: content-time and presentation-time

Every time-varying quantity belongs to exactly one of two clocks, and confusing
them is the single most common bug we hit this session.

| | **Content-time** (media / playhead) | **Presentation-time** (wall / watch) |
|---|---|---|
| Also called | `cursor`, the playhead, "now-reference" | watch-time |
| Advances by | the session's own timestamps | real seconds, **only while playing** |
| Frozen when | never — it *is* the position | paused, scrubbed, or off the live edge |
| Governs | folding, liveness, pending state, dating, run **grouping** | animation: camera glide, chip **afterglow**, marching ants |
| Question it answers | "what is *true* at this moment of the session?" | "how long have *I been watching* this?" |

Two rules fall out, and they are the spine of the whole UI:

1. **State is read from content-time.** What's true (an agent's status, a tool's
   pending-ness, a run's membership) is a function of timestamped events / the
   current fact set at the playhead. It is reconstructable at any playhead,
   scrubbed to from any direction.
2. **Animation ages in presentation-time.** A fade, a glide, a pulse is *how long
   you've watched*, so it accrues only while playing and freezes when you pause or
   scrub. It is forward-only and is **not** replayed on a seek.

The failures this prevents, both of which we lived through:

- **Aging state in wall-time** → a chip's fade racing gap-compression, flickering
  as the playhead jumps. (Fixed by making the afterglow watch-time.)
- **Deriving state from a static/final record that folds early** → the
  `meta.stoppedByUser` bug: a *final* flag applied at the *start* of replay marked
  agents "stopped" for the whole session. **Time comes from timestamped events,
  never from a record that states the end but folds at the beginning.**

---

## 2. Ground truth vs derived state

Because completion is often unknowable, the model constantly fills gaps with
**derived state**. The discipline that keeps this honest is one sentence:

> **Derive only where no ground truth exists, and never let a derivation override
> ground truth sitting in the model.**

Violating the second half is exactly the class of bug this session hunted down.

### 2.1 The async-agent completion model

The most dangerous piece of "ground truth" is the one that *looks* like a
completion but isn't:

- The **`Agent` tool result is a spawn acknowledgment** — literally
  `"Async agent launched successfully"` — **NOT** a completion. The subagent then
  runs for minutes in its own sidechain transcript. Marking it done at the ack was
  the root cause of the entire chip-flicker saga.
- Likewise a **`Workflow` tool result** is `"Workflow launched in background…"` —
  a launch ack, never a completion. Workflow groups roll up from children instead.

The signals that *are* authoritative, and set the **`terminal`** flag (which pins
an agent against time-derived revival):

- A **non-superseded** spawn ack (a *sync* subagent: the ack timestamp is not
  older than the agent's last activity — see the soft spot in §3).
- A workflow **journal `result`** entry naming the agent.
- A **`<task-notification>`** — the timestamped terminal report for a background
  agent (`stopped` / `completed` / `failed`). Routed off the prompt spine into
  `apply_task_notification`.

`meta.stoppedByUser` is a *static final* flag and is **not** used as a completion
time — see §1.2.

### 2.2 A pending tool_call *is* ground truth

An unresolved (`Pending`) tool_call is **direct evidence the agent is working** —
strictly stronger than any "no output for N seconds" inference. This is the lesson
of the 2m22s `Bash`: the subagent produced no transcript output for 143s, the
quiet-time heuristic declared it "Done," and its in-flight indicator vanished —
and when the tool finally **errored**, that error was never shown, because the run
had already been written off as history. Both liveness (§4) and chips (§5) now
read the pending tool_call as the working signal it is.

---

## 3. The derived-state heuristics catalogue

These are the places the model computes agent/timeline state as a function of
other facts because the transcript has **no direct event** for it. The **litmus
test** for whether such a heuristic is legitimate:

> **Legitimate iff it is a *reversible pure function of current facts* AND it
> *fills a genuine gap* rather than overriding ground truth already in the model.**

| Heuristic | Location | Derives | Verdict |
|---|---|---|---|
| **Workflow-group rollup** | `recompute_workflow_status` | Group status = all-children-terminal (`Failed` if any child failed) | ✅ **Textbook.** No workflow completion event exists; children's own terminal status *is* the ground truth; fully re-derived, reverts to `Running` when a late child appears. |
| **Time-derived liveness** | `recompute_liveness` | Agent `Running`/`Idle`/`Done` from an activity window **+ pending tool_calls** | ✅ **Legit** (as of this session). Terminal acks and pending tools — the ground truth — short-circuit it; the 120s window only fills the residual gap, reversibly. This is the one that *used* to fail the litmus. |
| **Spawn ack classification** | `resolve_spawn_status` | Whether an ack is a completion (sync) or a handle (async still running), via `last_ts > ack_ts` | ⚠️ **Mostly legit — one soft spot.** Grounded in real timestamps, but an async agent with *no own activity yet* + a spawn ack defaults to terminal `Done`. A definitive "done" from absence of evidence — the same shape as the bug we fixed, on a much rarer path. See §7. |
| **Undated-item dating** | `Timing` enum, `date_and_sort` (tailer) | Timestamps/order for **journal entries that carry none** in the format | ✅ **Legit.** A real gap (the data genuinely has no timestamp); dated relative to dated neighbours / a leader; guarded by an order-independence property test. |
| **Prompt / era attribution** | `prompt_for_ts`, era derivation | Which prompt-era an agent or entry belongs to | ✅ **Legit.** A pure function of recorded timestamps, cross-source-arrival-order independent. No "belongs to prompt X" field exists. |

**Not this class — presentation thresholds.** `RUN_GAP` (chip grouping),
`GAP_MARKER_SECS` (gap markers), `LIVE_FRESH` (live-dot), `GLIDE_SECS` (camera),
`POLL_INTERVAL` (tailer). These derive *how things look/pace*, not model state.
Being "wrong" here tweaks a pixel or a cadence; it can never mislabel an agent or
hide an error. They are held to a comfort standard, not a correctness one.

---

## 4. Time-derived liveness (`recompute_liveness`)

The reference clock is the timeline's **`now`**: wall-clock only at a *live* edge,
the **playhead otherwise** — so a scrubbed-back or replaying view shows the
as-of-then state with no wall-clock bleed. (This is the one job of the `replay`
flag — a recording judges liveness by the playhead, since its timestamps are a
past recording unrelated to wall time; a live session by wall-clock at its edge.
See §6.)

For each agent with a `last_ts`, "active" means **within `INTERACTIVE_IDLE_SECS`
(~120s) of `now`, OR holding a pending tool_call** (§2.2). Then:

- **Interactive** (main, forks): `Running` if active, else `Idle`. Never
  `Done`/`Failed` — interactive completion is unclaimable. Reversible.
- **Subagent, `terminal`:** keep the authoritative status (§2.1). Short-circuits
  the heuristic entirely.
- **Subagent, non-terminal:** `Running` if active, else `Done` — **reversible**: a
  long gap that settled it to `Done` flips straight back to `Running` when it
  resumes (or the moment a tool goes pending).

A **replay** that reaches its end fires `end_of_stream` once, settling interactive
agents to `Idle` (the recording is over; activity is provably absent). A live
stream never fires it.

---

## 5. The chip system — a case study in the two clocks

Chips are the ephemeral `⚒ bash ×5` overlays under agent cards. They are the
cleanest worked example of §1.2, and were rebuilt this session into a **single
reconcile pass**.

**A chip is a *run*** — a maximal group of consecutive same-name tool calls
(`⚒ read ×N`), a range `[start, start+count)` into an agent's `tool_calls`.
Aggregation is what stops a busy agent from churning chips through the per-agent
cap: a burst of 20 reads is one counting chip, not 20 that flash past.

**One derivation each frame — `ChipTray::reconcile(dt, playing, model)`.** It
rebuilds the whole tray from model state, carrying afterglows across the rebuild
by run identity `(agent, start)`. The design splits cleanly along the two clocks:

- **Grouping is content** — a pure function of the calls' own **timestamps**
  (`within_gap`, `RUN_GAP`). Because it never consults animation history, the
  grouping is *identical on forward playback and on a seek* — which is precisely
  what let the old two paths (`observe` + `seed`) collapse into one.
- **The afterglow is presentation** — a fade that ages in **watch-time**, anchored
  when a run settles, re-anchored bright when a run gains a member (so a long burst
  doesn't sawtooth-fade mid-stream). Forward-only; not replayed on a seek.
- **Pending is content** — a run with any in-flight call shows bright and is
  **reconstructed at any playhead inside its interval**, however you scrubbed
  there. On attach/seek, `adopt_baseline` moves the `seen` mark to the end
  (absorbing completed history silently) and the next `reconcile` re-derives the
  in-flight runs from state.

**Two invariants worth their own line:**

- `RUN_GAP == CHIP_TTL` (2.5s). Once a run has been quiet long enough to fade, the
  next same-name call is also beyond the gap — so it opens a *fresh* run instead of
  **resurrecting** the faded one.
- **`seen` is the born-completed / history discriminator.** A completed run past
  the mark is new work (flash it); one below the mark is history a seek absorbed
  (don't). It is the one genuinely irreducible bit of memory — everything else is a
  pure function of state.

**Owner-liveness is `terminal`-gated.** A pending chip persists until its owner is
*authoritatively* finished — **not** on the reversible 120s-quiet `Done` (§2.2).
This is what keeps a long tool's chip alive to settle into its ✓/✗ instead of
vanishing.

---

## 6. Time-travel over one timeline

Detailed in [`DESIGN.md`](DESIGN.md#timeline); the architectural essence:

- **Live and replay are one time-shifted model**, not two modes. One ts-ordered
  item list, one playhead. The only difference is whether the right **edge** is
  fixed (finished file) or growing (session being written). The edge is *always
  the last event*, never wall-clock now — an old session never grows an empty tail
  toward the present.
- **Pin-vs-pace is decided by the edge, not the mode.** Behind the edge the cursor
  paces forward, compressing dead air by a graded log curve (`compress_gap`, not a
  flat cap — a 5-min wait still reads longer than a 5-sec one; `g` toggles faithful
  pacing); at the edge it pins. So `space` resumes from the playhead in both modes,
  and a scrubbed-back live session catches up then follows.
- **The scrubber is event-indexed, not time-linear** — real sessions cluster work
  then idle for hours, so a time-linear bar buries the action in a sliver. The
  track is a tool-activity sparkline over event ranges.
- **Transport is emergent** (`Live`/`Playing`/`Paused`/`History`/`Idle`), read from
  edge-following + append freshness — never a hardcoded mode.

### 6.1 Seeking backward without re-folding (the snapshot ladder)

Folding is forward-only, so moving the playhead back means rebuilding the model
from a prefix. Doing that from item zero costs O(k) in the distance travelled —
visibly janky on a large session.

- **A snapshot is a clone.** `SessionModel` is built from persistent (`imbl`)
  collections, so cloning it is O(1) and shares structure rather than copying.
  A ladder of rungs every `SNAPSHOT_STRIDE` items therefore costs little, and a
  backward seek restores the nearest rung and folds forward from there.
- **This only works if the *values* are persistent too.** `imbl` copy-on-writes
  the value at a touched node, so an `AgentInfo` owning a plain `Vec` of tool
  calls would deep-copy that agent's whole history on every append — O(n²), the
  exact quadratic `tool_index` was added to remove. Every growing collection in
  the model is persistent, not just the outer map.
- **Rungs hold fold state, never projections.** Liveness and workflow rollups are
  functions of the playhead (§4), not of the fact set, so baking them into a rung
  would restore yesterday's answer. `resync` re-runs them after every restore.
  This is the fold/projection split (§2) becoming load-bearing rather than
  descriptive.
- **Item indices are not stable, so rungs expire.** A late batch that dates a
  previously-pending item re-sorts the list around it (undated items sort ahead
  of dated ones), shifting every index in between. A rung keyed to `items[0..N]`
  then silently describes a different prefix — wrong, not slow. `Timeline::generation`
  moves on every re-sort and stale rungs are discarded.

The remaining cost of a backward seek is no longer the fold: `rebuild_to` still
discards the whole `Flow` and re-projects it. That is the next thing to fix, and
why `agents` is an `OrdMap` (which has a structural `diff`) rather than the
faster `HashMap` (which does not).

---

## 7. Watching every live session

The canvas shows every session that is currently running, each as a root card
with its agents beneath it. The rail indexes them — including the idle ones the
canvas leaves out — and picks which one is *focused*.

- **Only the focused session gets a playhead.** It owns the `Timeline` and can be
  scrubbed; the rest are folded straight off their live edge and only render.
  The asymmetry is the domain shape, not an accident: one collection holding both
  would either hand every session a timeline it never uses, or pretend the focused
  one is not special when every seek path says it is.
- **A monitored session costs a model, not a timeline.** Its batches fold and are
  dropped, so N sessions cost `1× items + N× models` — and a model is small next
  to the parsed item list.
- **Discovery over-includes, then the bytes decide.** The stat sweep filters on
  mtime, which can only ever *over*-include (appending always advances it), so no
  live session can be filtered out. The index then reports ground truth. Activity
  is the newest mtime across the main transcript **and its sidecars**: a session
  blocked on a subagent writes nothing to its own file, and filtering on that file
  alone drops exactly the sessions most worth watching.
- **Focus is derived, never stored twice.** The rail marks whichever row matches
  the session actually being watched, which the tailer confirms via `SessionReset`.
  A parallel focus field would eventually point at a session nobody is tailing —
  a marker that lies.
- **Node ids carry their session.** Every `SessionModel` names its root `"main"`,
  so a shared flow needs the session in the id or the second root is a duplicate
  no-op and its agents hang off the first session's tree. Wrong picture, no error.

### 7.1 A second reader of the same format

The skeleton index parses none of the payload — it probes for the handful of
facts the timeline needs and leaves the body on disk. Two readers of one
undocumented format is normally a mistake; here it is the point, and the thing
that makes it legitimate is that **the fast path is never allowed to guess**:

- where the format is ambiguous, the record says so and the caller parses that
  line properly;
- a differential test asserts record-for-record agreement with the real parser
  over a whole corpus.

That test is not a formality. Both format-drift bugs it found — envelope keys
that are not emitted first, and activity counts that must come from real content
blocks rather than from text that merely contains the literal — were invisible to
review and to every synthetic fixture.

---

## 8. Known soft spots

Honest ledger of what's derived-but-imperfect, for whoever touches this next:

- **`resolve_spawn_status` no-activity → `Done`.** An async agent that has a spawn
  ack but no own activity *yet* is classified terminal `Done`. It's a definitive
  conclusion from absence of evidence — the same species as the liveness bug we
  fixed, just on a rare path (sidechain agents normally produce activity). The
  principled fix: don't default a zero-activity async agent to terminal; let
  time-derived liveness own it until real evidence arrives. Left documented, not
  yet changed.
- **Aggregate chip error-coloring.** A settled run shows a single aggregate
  ✓/✗; a run of N where only one call failed still reads as an error run. Cosmetic
  over-alarm, noted for a future pass.
- **The monitor fleet is capped silently.** Past `MAX_MONITORED` a live session is
  simply not drawn, and nothing on screen says so.

---

## Appendix — the principles, distilled

1. The model is a **pure function of the fact set**, never of arrival order.
2. Derived state is **reversible**; it reverts when contradicted. Never monotonic.
3. **State ages in content-time; animation ages in presentation-time.** Never mix.
4. **Time comes from timestamped events**, never from a final/static record that
   folds early.
5. A **spawn ack is not a completion.** Completion is `terminal`, or it is derived
   and reversible.
6. A **pending tool_call is ground truth** — it outranks any quiet-time inference.
7. A heuristic is legitimate only when it is a **reversible function of current
   facts filling a real gap** — never when it overrides ground truth already in the
   model.
8. A **fast path may be faster, never different** — where it cannot know, it says
   so, and a differential test keeps it honest.
9. **Cache the fold, never the projection.** Anything derived from the playhead is
   re-run after a restore, not stored with it.
10. **An index into a list that can re-sort is not an identity.** Anything keyed by
    position carries the generation it was taken under.
