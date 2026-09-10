---
title: Design & architecture
description: "How zoetrope stays correct: an event-sourced projection, two clocks, and time-travel over one timeline for live and replay."
---

zoetrope has one hard job, and most of its design follows from it:

> Reconstruct a faithful, navigable, live-or-replayed view of a coding agent's
> session (Claude Code, Codex) from a transcript that is **undocumented,
> append-only, only partly timestamped, and split across files**, and in which
> **you often can't tell when something finished.**

The shape of the answer is **event sourcing**: treat the transcript as an
append-only event log, and derive everything else from it.

## The model is a projection

zoetrope never mutates a session model in place. It **folds** (reduces) the event
log into a **projection**, and that projection is a pure function of the *set* of
events, not of the order they arrive in. Every update is **idempotent** (replaying
an event is a no-op) and **commutative** (two events reach the same state in either
order), so the fold is **order-independent**: any arrival order converges on the
same projection. Three things depend on that:

- **Multi-file merge.** The main transcript, the subagent files, and the workflow
  journals are separate append-only logs that interleave by timestamp. A subagent's
  result can be folded before its spawn.
- **Time-travel.** Seeking into the past rebuilds the projection from a prefix of
  the log (`events[0..cursor]`), so it has to land on exactly the state that playing
  there would have produced. This is time-travel debugging for a session.
- **Live and replay.** The same events arrive pre-sorted (replay) or in arrival
  order (live). Both have to converge on one projection.

A **property-based test** shuffles a real event stream and asserts the final
projection is identical on every run.

One consequence: **derived state is reversible.** Because the projection is
recomputed from the current event set instead of accumulated, a rollup that
concluded "done" reverts the instant a contradicting event arrives, such as a late
child or a resumed tool. It is never write-once.

## Two clocks

Every value that changes over time lives on one of two clocks, and they are never
mixed.

- **Content-time** is the playhead: the session's own logical clock, advanced by
  event timestamps. **Domain state** reads from here (an agent's status, whether a
  tool call is still in flight, which run a call belongs to), so it is deterministic
  at any playhead, however you seeked there.
- **Presentation-time** is wall-clock, and it advances only while you are playing.
  **View state** lives here: a fade, a camera glide, a pulse. It is forward-only,
  and it is not reproduced on a seek.

Keeping domain state on content-time and view state on wall-clock is what lets a
seeked-to frame render exactly what was true at that point, deterministically, with
no wall-clock leaking into the model.

## Ground truth over derivation

The log rarely records that something *finished*, so the projection fills the gaps
with **computed state**. The invariant that keeps that honest:

> Derive only where there is no ground truth, and never let a derivation override
> ground truth already in the model.

Two cases carry most of the weight:

- **A spawn acknowledgement is not a completion.** "Async agent launched
  successfully" is a handle, not a result. The subagent then runs for minutes in its
  own sidechain. A real completion is an explicit terminal event (a task
  notification, or a workflow-journal result), and it pins the agent so a time-based
  heuristic can't revive it.
- **A pending tool call is ground truth.** An in-flight call is hard evidence that
  the agent is working, which outranks any "idle for N seconds" inference. So a slow
  tool's indicator does not disappear before its result lands.

Everything computed (liveness windows, group rollups, the ordering of undated
events) has to be a reversible function of the current facts that fills a real gap.
Anything that overrides a fact already in the model is a bug.

## One boundary per format

Every transcript format enters through a **provider**, one for Claude Code and
one for Codex. A provider turns its own records into **facts**, a small shared
vocabulary (an agent exists, a tool call started, a tool call ended with this
outcome, an agent's end was observed), and the core folds facts without knowing
which format stated them. The rule that decides what belongs on which side:

> A provider states what its records contain. The core decides what it means
> when nothing was recorded.

So a provider never concludes. It does not mark an agent done because a tool
result came back, and it does not invent an outcome for a call whose end it never
saw. Reasoning about absence, such as the difference between a spawn being
acknowledged and finishing, or an agent that went quiet, lives in the core once
and survives every format.

The same rule holds one level up, for finding files: a provider states what a
file is, and the core decides what a session is. That is why `zoe <file>` opens
any file of a session from either agent, and tells the formats apart by content
rather than by name.

## Time-travel over one timeline

Live and replay are not two modes. They are one time-shifted model: a single
timestamp-ordered event list and a single playhead. The only difference is whether
the right **edge** is fixed (a finished file) or still growing (a session being
written).

- **The edge decides pin vs. pace, not a mode flag.** Behind the edge the playhead
  paces forward and compresses idle gaps on a log curve, so a five-minute wait still
  reads longer than a five-second one (<kbd>s</kbd> toggles real-time). At the edge
  it pins and streams in new appends. <kbd>space</kbd> resumes from the playhead in
  both cases, and a seeked-back live session catches up and then follows.
- **The scrubber is indexed by event, not by wall-clock.** Real sessions work in
  bursts and then sit idle for hours, so a time-linear axis would bury the work in a
  sliver. The track is a tool-activity sparkline spread over event ranges.
- **Transport state is derived, not stored.** Live, Playing, Paused, History, and
  Idle are computed from where the playhead sits against the edge and how fresh the
  appends are. It is a state machine, not a mode you set.

## One core, two frontends

At the bottom is the **portable core**: the domain model, the replay/live timeline,
the flow-graph projection, the rendering, and the providers that read each
transcript format. It does no IO and compiles for any target, WebAssembly
included. Two deliberately thin frontends
sit on top of it, one per crate.

- **`zoetrope`** — the crate you install, and the one the core lives in. On top of
  the core it adds the **native frontend**: the file-tailing loop, the terminal
  lifecycle, and keyboard input. Those pull `tokio` and the filesystem, so they sit
  behind a `native` feature that is on by default and off for every other target.
- **`zoetrope-web`** — the **browser frontend**, in
  [`web/wasm/`](https://github.com/furkankly/zoetrope/tree/main/web/wasm). It
  depends on the core with default features off and adds a
  [ratzilla](https://github.com/ratatui/ratzilla) WebGL2 backend plus a small
  event-conversion layer. The browser has no filesystem, so bytes are handed
  straight to the same engine, whether from the bundled demo, an uploaded
  transcript, or a folder opened through the File System Access API. It is deployed
  as this site's [`/app`](/app) route, never installed.

All IO is local. The input is your filesystem, or in the browser the bytes you hand
it, and there is no HTTP client anywhere in the dependency tree.

---

The deeper reasoning (the heuristics catalogue, the two-clocks table, and the known
rough edges) is in
[`docs/ARCHITECTURE.md`](https://github.com/furkankly/zoetrope/blob/main/docs/ARCHITECTURE.md).
The module map, transcript format, and type shapes are in
[`docs/DESIGN.md`](https://github.com/furkankly/zoetrope/blob/main/docs/DESIGN.md).
