# zoetrope — Design Document (v1)

**zoetrope** is a terminal UI that visualizes Claude Code agent sessions as a live flow graph: the main agent, its subagents, workflows, and tool activity — rendered with [rataflow](../../rataflow). Synthesized from a multi-agent research pass over rwy (`/Users/furkan/personal/projects/rwy`), rataflow (`/Users/furkan/personal/projects/rataflow`), and real transcripts under `~/.claude/projects/`. Full research: see the workflow output referenced in the repo history.

> **This is the v1 structural spec** (module map, transcript format, type shapes). For the *invariants and principles* the implementation now follows — order-independence, the content-vs-presentation clocks, ground-truth-over-heuristics, and the derived-state heuristics catalogue — see [`ARCHITECTURE.md`](ARCHITECTURE.md).

## Hard constraints

1. **No network IO, ever.** All IO is local filesystem. No reqwest/hyper/etc. in the dep tree (dev-deps included). "Your transcripts never leave your machine" goes in the README.
2. **Defensive parsing.** The transcript format is undocumented/internal. Unknown entry types, missing fields, malformed lines: skip, never panic. Mirrors Claude Code's own resilience.
3. **rwy's async architecture**, ported: single-task UI loop owns all state; background tokio tasks feed typed messages over bounded mpsc; no `Arc<Mutex>`.
4. Dependencies live in `Cargo.toml`, with the reason for each beside it; the split that matters is the portable core versus the `native` feature (tokio, crossterm, the filesystem).

## CLI

One TUI command over the unified timeline engine (see the **Timeline** section) plus the headless `inspect`. The launch only picks **defaults** — *what* to open and *where the playhead starts*; once open, scrub / follow / pause / go-live all work regardless.

```
zoe                      # follow the current project's live session
zoe <file.jsonl>         # replay a recording, played from the start (any provider's file)
zoe <id>                 # replay a session by id, or a unique prefix, across providers
zoe <dir>                # follow another project's live session
zoe <file> --follow      # ride a file's live edge instead of replaying it
zoe <file> --speed 8     # playback speed (default 8.0)
zoe --provider codex ... # force the format instead of reading it off the content
zoe inspect <file|id>    # no TUI: print session info + parsed tree (smoke-test)
```

Resolution goes through `provider::open` (see [DISCOVERY.md](DISCOVERY.md)): a **file** is `Target::Path` and bulk-loads + tails (replay feeder), the provider read off its first record; an **id** is `Target::Id`, looked up across every provider's roots; a **dir** (or none → cwd) is `Target::Here` and follows the newest session of that project, any provider. `--follow` only changes the start position (head vs beginning) via `Mode`. `Cli = View { target: Option<String>, follow: bool, speed: f64, provider: Option<Provider> } | Inspect { file, provider }`. Arg parsing: hand-rolled over `std::env::args` (no clap; keep deps lean).

## Workspace layout

Two crates. `zoetrope` (repo root) is the published one: the portable core plus the native frontend, split by a Cargo feature (`default = ["native"]`). `zoetrope-web` (`web/wasm/`) is the browser frontend — `publish = false`, built only for `wasm32`.

The split keeps the browser frontend *out of the published crate*: nothing wasm ships to crates.io, and depending on the `zoetrope` library from a wasm target imposes no ratzilla/getrandom choices on you.

The browser frontend is **excluded** from the root workspace rather than being a member of it, so it resolves as its own workspace with its own lockfile and target dir. This is the mainstream shape for a wasm frontend, not a workaround of last resort: putting one in a workspace is what produces the well-known trunk/`wasm-bindgen` version-mismatch failures, because membership forces a single `wasm-bindgen` across crates that have no reason to agree on one. The reason is that it cannot be compiled for the host *at all*: rataflow gates its ratzilla `From` impls on `all(feature = "ratzilla", target_arch = "wasm32")`, so a host-target check fails to typecheck. `default-members` would have kept it out of a bare `cargo build`, but not out of `cargo check --workspace` — and not out of rust-analyzer, which would sit on two permanent phantom errors. Excluding it is the only thing that makes the editor honest. `web/wasm/.cargo/config.toml` sets `[build] target = "wasm32-unknown-unknown"` so cargo and rust-analyzer both default to the right target there — it sits next to `Trunk.toml` in the crate root, which is where trunk runs from, so one copy serves everything; cargo's own answer to this, `per-package-target`/`forced-target`, is still nightly-only.

Two lockfiles means the two frontends can drift apart. Most of that drift is harmless and even desirable — `wasm-bindgen`, `getrandom` and the ratzilla stack should track the browser build's needs, not the terminal's. What must **not** drift is anything that decides what gets drawn: the ratatui tree (`ratatui`, `ratatui-core`, `ratatui-widgets`, and their `unicode-width` / `lru` / `line-clipping` / `instability`) plus rataflow's `rust-sugiyama`. Those are pinned to the same versions in both lockfiles on purpose. If you `cargo update` one, re-check the other.

```text
Cargo.toml          # [workspace] exclude = ["web/wasm"]  — its own workspace, own lockfile
src/                # the zoetrope library + the `zoe` bin (src/main.rs)
web/wasm/           # the zoetrope-web crate: Cargo.toml, index.html (trunk entry), src/main.rs
herdr-plugin/       # the Herdr plugin: a manifest and shell scripts that launch `zoe`; ships by git clone, not with the crate
```

## Dependencies (Cargo.toml)

`edition = "2024"`, `license = "MIT"`.

```toml
# Portable core (native + wasm): model, timeline, graph, UI rendering, parsing.
ratatui       = { version = "0.30", default-features = false, features = ["underline-color"] }
rataflow  = { path = "../rataflow", default-features = false, features = ["sugiyama"] }
serde         = { version = "1", features = ["derive"] }
serde_json    = "1"
chrono        = { version = "0.4", features = ["serde", "clock"] }   # + "wasmbind" on wasm
web-time      = "1"          # Instant/SystemTime that work on wasm (perf.now); re-exports std on native
unicode-width = "0.2"        # display-column width for truncation (CJK/emoji are 2 cols)

# native feature → the native frontend (all optional, gated #[cfg(feature = "native")])
crossterm = { version = "0.29", features = ["event-stream"], optional = true }
tokio     = { version = "1", features = ["rt-multi-thread","macros","time","sync","fs","io-util"], optional = true }
futures   = { version = "0.3", optional = true }
anyhow    = { version = "1", optional = true }
# native also flips on ratatui/crossterm + rataflow/crossterm.

# the library's own wasm need, under [target.'cfg(target_arch="wasm32")'.dependencies]
chrono = { features = ["wasmbind"] }   # so Utc::now() reads the browser clock
```

The browser frontend's deps live in `web/wasm/Cargo.toml`, not here:

```toml
zoetrope  = { path = "../..", default-features = false }   # the portable core, no native IO
ratzilla  = "0.3.1"          # ratatui's wasm backend (WebGl2 + on_mouse_event)
rataflow  = { features = ["sugiyama", "ratzilla"] }        # event From impls + Flow::handle_wheel
web-sys, wasm-bindgen, console_error_panic_hook            # wheel listener + panic hook
critical-section = { features = ["std"] }                  # ratatui's layout-cache guard, picked by the bin
getrandom (0.3) + getrandom_v04 (0.4)                      # both, wasm_js backend (pulled via ratzilla)
```

**One binary per crate:** `zoe` → `src/main.rs` (`required-features = ["native"]`), the only thing `cargo install zoetrope` puts on your PATH; `web` → `web/wasm/src/main.rs`, built by trunk (`web/scripts/build-wasm.sh`) into `web.js` / `web_bg.wasm`. No network deps anywhere (hard constraint #1).

## Module map

```
src/
├── lib.rs         # crate root — the portable core shared by the native + browser frontends
├── main.rs        # native binary + CLI parsing; spawns the tailer task, runs the TUI; the `inspect` subcommand
├── tui.rs         # terminal lifecycle + the central native event loop (tick_camera/tick_timeline/status_tick/draw)
├── handler.rs     # input routing: app-level keys → App, the rest → the flow; scrubber clicks; process_flow_events
├── autopilot.rs   # native-only: the scripted pointer/keystroke pilot behind ZOETROPE_DEMO=1 (see DEMO-ASSETS.md)
├── fact.rs        # the provider boundary: Fact + FactKind, the vocabulary every provider speaks and the model folds
├── provider/
│   ├── mod.rs     # the input side: Provider enum, SessionFile, Session, the Stream enum, open / sweep / assemble (DISCOVERY.md)
│   ├── harness.rs # (test) the conformance check: fold a fixture in shuffled orders and diff the result against its goldens
│   ├── claude/
│   │   ├── mod.rs       # the Claude provider: Entry → Facts (`facts`, `Record`) and the per-file `Stream`; the tool-summary lexicon
│   │   ├── wire.rs      # Claude's serde model for JSONL entries + meta.json sidecars
│   │   └── discovery.rs # the ~/.claude/projects layout: cwd sanitization, session / subagent / journal scans, the primitives
│   └── codex/
│       ├── mod.rs       # the Codex provider: rollout lines → Facts through a `Stream` that learns its thread from its first line
│       ├── wire.rs      # Codex's serde model: the envelope, `response_item`, `event_msg` and its `item_completed` items
│       └── discovery.rs # the ~/.codex/sessions/YYYY/MM/DD layout: rollouts, the head read that classifies them, the primitives
├── state/
│   ├── mod.rs     # App: owns the Flow + SessionModel + Timeline + SessionInfo + UI state; handle_ui_event, seek, camera
│   ├── session.rs # SessionModel: the pure domain model (agents, statuses, tool calls) folded from Facts — knows no format
│   ├── timeline.rs# Timeline: the ts-ordered item list + playhead (time-travel); pacing, gap-compression, seek, floor
│   ├── graph.rs   # incremental SessionModel → Flow projection (never rebuilds except backward seek; Sugiyama on `r`)
│   ├── info.rs    # SessionInfo: untimed session metadata, folded off the timeline (i overlay + inspect header)
│   └── render.rs  # the headless text view: what `inspect` prints, and what a provider's golden test compares
├── tailer/        # background FEEDER (pure: no pacing/seeking — the App owns the playhead)
│   ├── mod.rs     # task entry + shared wire types (TailRequest / UiEvent); the wire carries Statements
│   ├── live.rs    # live tailing — one poll loop per session, emits UiEvent::Batch
│   ├── replay.rs  # replay assembly (native): parse all files up front, merge by ts, then keep tailing
│   ├── item.rs    # portable replay-stream pieces — ReplayItem (a statement + its Timing), dating, and Bundle (the browser's feeder: files as text in, streams kept for appends); IO-free (wasm)
│   └── bytes.rs   # incremental byte reader: stat / read-appended / split-on-\n / buffer-partial (pure, testable)
└── ui/
    ├── mod.rs     # draw: canvas + scrubber + status bar; help/info overlays
    ├── nodes.rs   # AgentNode: the agent-card NodeContent (semantic zoom: Card vs Cell)
    ├── edges.rs   # AgentEdge: parent→agent EdgeContent (animated while target Running)
    ├── chips.rs   # ephemeral tool-call chips — one reconcile pass, anchored to agent nodes (see ARCHITECTURE.md §5)
    └── panel.rs   # detail panel for the selected agent
```

## Claude Code transcript format (verified against real data, Claude Code 2.1.153–2.1.165)

This is one provider's format, modelled in `provider/claude/wire.rs` and found on disk by `provider/claude/discovery.rs`. Nothing in the core (`fact`, `state`, `ui`) depends on any of it; the feeders reach a provider only through `provider::open`, `sweep` and `provider_of`.

### Layout
- Main: `~/.claude/projects/<sanitized-cwd>/<session-uuid>.jsonl`. Sanitization: absolute cwd, every `/` → `-` (leading slash → leading dash). Only `<uuid>.jsonl` directly in that dir are transcripts.
- Subagents: `<session-uuid>/subagents/agent-<agentId>.jsonl` + `agent-<agentId>.meta.json`. `agentId` = 17 hex chars (NOT a UUID), also present as a field on every line of its file.
- Workflow subagents: `<session-uuid>/subagents/workflows/<wf-id>/agent-*.jsonl` + `.meta.json` + `journal.jsonl` (ledger of `started`/`result` entries — NOT a transcript; `result` entries carry `agentId` = workflow-subagent completion marker).
- **Ignore**: `vercel-plugin/skill-injections.jsonl` (no `type` field), `memory/`, `sessions-index.json`, `tool-results/` (output overflow spill).

### meta.json
`{agentType: String, description: Option<String>, toolUseId: Option<String>}`. Direct Agent calls have all three; workflow subagents have only `agentType: "workflow-subagent"`.
**Linkage:** `meta.toolUseId` === the `Agent` tool_use block `.id` in the main transcript; `meta.agentType` === `tool_use.input.subagent_type`.

### Entries — `#[serde(tag = "type")]` + `#[serde(other)] Unknown`
- **Transcript entries** (`user`, `assistant`, `system`, `attachment`): envelope has `uuid`, `parentUuid` (null only on the single root — distinguish *present-and-null* from *absent*), `timestamp` (ISO8601 UTC millis, e.g. `2026-06-05T13:51:15.151Z`), `sessionId`, `isSidechain` (false in main, true in subagent files), optional `promptId`, `requestId` (assistant-only). Subagent lines add `agentId` (all lines) and `attributionAgent` (assistant lines only — do NOT rely on it; join on `agentId`).
- **assistant**: `.message = {role, model, content[], stop_reason, usage}`. Content block types: `text {text}`, `thinking {thinking, signature}`, `tool_use {id, name, input, caller?}` (`caller` is newer-schema; Option). `usage.output_tokens` etc. — sub-fields vary by version, all Option/default. `model` e.g. `claude-opus-4-8`.
- **user**: `.message.content` is **string OR array** (untagged enum). Array blocks: `text`, `tool_result {tool_use_id, content, is_error?}` — `content` is **also string-or-array**; `is_error` is `Option<bool>`, **missing means success**. Top-level optional `toolUseResult` (object|string) sibling of `.message`.
- **system**: subtypes via `subtype` field (`turn_duration`, `stop_hook_summary`, `local_command`, …) — keep as a lean variant, mostly ignored.
- **attachment**: `.attachment.type` various — context injections, not graph material.
- **Flat metadata entries** (`ai-title`, `last-prompt`, `mode`, `permission-mode`, `file-history-snapshot`, `queue-operation`): NO uuid/parentUuid/timestamp — must deserialize into lean variants (a struct requiring envelope fields fails). **`ai-title` provides the session title for the header bar.**
- **Ledger entries** (`started {key, agentId}`, `result {key, agentId, result}`): appear in subagent files and journal.jsonl. Excluded from graph; `result` in journal.jsonl marks workflow-subagent completion.
- `summary` type: documented to exist, never observed — handled by Unknown.
- Format is strict JSONL: one JSON object per line, no embedded newlines, no blob lines. Longest observed line 38KB; largest file 2MB/792 lines.

### Tool calls
`tool_use` names observed: Bash, Edit, Read, Write, ToolSearch, AskUserQuestion, Agent, Workflow, TaskStop, WebFetch. `Agent` input: `{description, prompt, subagent_type}`. Pair `tool_use.id` with the later user `tool_result.tool_use_id` → pending (no result yet = in-flight) vs complete (`is_error` decides Failed).

## Domain model (state/session.rs)

Folded from `Fact`s only, through one entry point, `apply_fact`; the model never
sees a provider's records. Collections are `imbl` persistent structures, so a
snapshot is an O(1) clone (the seek ladder depends on that).

```rust
pub struct SessionModel {
    pub session_id: String,
    pub(crate) agents: OrdMap<String, AgentInfo>, // keyed by node id ("main", the provider's agent id, or a group id)
    pub(crate) spawn_order: Vector<String>,       // stable discovery order (map key order ≠ spawn order)
    pub last_activity: Option<DateTime<Utc>>,
    // Order-independent join stores — a fact attaches whether it arrives before or
    // after the thing it refers to (ARCHITECTURE.md §1.1):
    //   labels:          agent_id → (agent_type, description) — `Label` facts (a group named before it exists)
    //   completed_calls: call_id  → (is_err, end_ts)          — every `ToolEnd`; a spawning call's end is the ACK
    //   ended:           agent_id → AgentStatus               — `Ended` facts: authoritative, outrank the ack
    //   spawn_context:   call_id  → SpawnContext              — provenance from `Spawn` facts (era + reasoning)
    //   prompts:         Vector<PromptInfo>                   — the root's prompt eras (prompt_for_ts attribution)
    //   last_reasoning:  agent_id → String                    — cross-record reasoning fallback
}
pub struct AgentInfo {
    pub kind: AgentKind,                 // Main | Subagent | Group
    pub interactive: bool,               // main/fork — no completion signal; selects the liveness branch
    pub agent_type: Option<String>,      // "claude-code-guide", "workflow-subagent", "fork", …
    pub description: Option<String>,     // meta.description or Agent tool_use input.description
    pub parent: Option<String>,          // node id of parent (main or wf-id)
    pub spawned_by: Option<String>, // the spawning call id — the spawn/ack join key
    pub status: AgentStatus,             // Running | Idle | Done | Failed | Stopped
    pub(crate) terminal: bool,           // authoritative completion — pins against time-derived revival
    pub model: Option<String>,
    pub tool_calls: Vec<ToolCallInfo>,   // {id, name, summary: Option<String>, ts, state: Pending|Ok|Err}
    pub output_tokens: u64,              // summed from usage (deduped per requestId)
    pub first_ts, last_ts: Option<DateTime<Utc>>,
    // + internal indices: tool_index (tool_use_id → slot), seen_request_ids (token-dedup)
}
```

**Untimed session metadata → `SessionInfo` (not the timeline).** Session-level facts (`Title`, `Session { label, value }`, `Tally`) carry no timestamp and are not activity, so they would otherwise clump at the front of the sorted timeline. Feeders route them into `SessionInfo { title, fields: Vec<(label, value)>, tallies }` instead, whichever record carried them (`Statement::take_session_meta`; a Codex root names itself and its app on one line): the provider labels the rows (Claude: `mode`, `permission`, `last prompt`; tallies `queued`, `file edits`; Codex: `app`, `version`, `cwd`), values are latest-wins per label in first-seen order, and the `i` overlay and `inspect` render whatever rows arrived (`state::render`). Lives on `App` (not `SessionModel`), so it survives backward-seek rebuilds — it's session-constant. The title is on `SessionInfo`, NOT on `SessionModel`, so the model holds only timed, foldable state.

**Graph topology (v1): nodes are agents, not messages.** One node per agent + one group node per workflow run. Edges: one parent edge per agent, id `e-<child id>` (`graph::edge_id`), `workflow → its subagents`. Sessions have 800+ lines — per-message nodes would be noise; agents are the story.

**Status rules** — the concrete derivations; the *principles* they follow (ground-truth-over-heuristics, reversibility, the async completion model) are in [`ARCHITECTURE.md`](ARCHITECTURE.md) §2–4.

- **The `Agent` tool result is a SPAWN ACK, not a completion** (`"Async agent launched successfully"`). A direct subagent is completed by the main-transcript `tool_result` (`tool_use_id == spawned_by`, `is_error` → Failed) **only if the ack is not superseded** by the agent's own later activity — `resolve_spawn_status`: `last_ts > ack_ts` ⇒ still `Running`, non-terminal. A superseded (async) subagent stays `Running` and settles to `Done` at `end_of_stream`.
- **`<task-notification>` is the authoritative terminal report** for a background agent. The Claude provider states it as `Ended(Done | Stopped | Failed)`; the fold records it in the `ended` store, where it **outranks** the ack and time-derived liveness, and pins `terminal`. (`meta.stoppedByUser` is deliberately **not** applied — the meta folds at the agent's *first* activity, so applying it would strand the agent `Stopped` for the whole replay; only the timestamped notification is trusted.)
- **Workflow subagent**: `Done` when `journal.jsonl` has a `result` naming its `agentId` (the provider states `Ended(Done)`; pins `terminal`). Group node (`recompute_group_status`): an all-children-**terminal** rollup (`Failed` if any child failed, else `Done`; `Done`/`Stopped` both count as terminal; a childless group stays `Running`; re-derived every call, so it reverts if a running child is discovered late). The Workflow tool_use's `tool_result` is a *launch ack* ("Workflow launched in background…"), NOT a completion — never complete groups from it.
- **Liveness** (`recompute_liveness`, against the timeline's **`now` reference** — wall clock at a live edge, the playhead otherwise, so a scrubbed/paced view shows the as-of-then state with no wall-clock bleed): "active" = `now − last_ts ≤ INTERACTIVE_IDLE_SECS` (~2 min) **OR the agent holds a pending tool_call** (an unresolved tool is direct proof it's working — §2.2/§4). Interactive → `Running`/`Idle` (never claims completion); non-interactive non-terminal → `Running`/`Done`, **reversible**; non-interactive terminal → keeps its status. `end_of_stream` settles interactive agents to `Idle` and any still-`Running` async agent to `Done`.
- Edge `animated = target agent Running`.

## Tailer — the feeder (tailer/)

**Decision: poll-based, no `notify` dep** (poll is simpler, WASM-trait-friendly, 200ms is imperceptible). The tailer is a **pure feeder** — it no longer paces or seeks (that moved to the App's `Timeline`); it only produces an ordered update stream and keeps the files watched.

```rust
pub enum TailRequest { Watch(Target) }                   // switch session; only request now (a path, an id, or a directory to follow)
pub enum UiEvent {
    ReplayLoaded { session_id, items: Vec<ReplayItem>, speed, info: SessionInfo }, // bulk hand-off
    Batch { session_id: String, statements: Vec<Statement> }, // per poll tick, one per record read
    SessionReset { session_id: String },                 // truncation/rotation/auto-switch
    Error(String),
}
pub struct Statement { at: Option<DateTime<Utc>>, facts: Vec<Fact> } // what one record stated, and when it was written
pub struct ReplayItem { timing: Timing, pub facts: Vec<Fact> }      // one statement + its Timing; .ts() → Some only when Dated
pub enum Timing {                     // how an item is placed on the timeline (tailer/item.rs)
    Dated(DateTime<Utc>),             // has, or has derived, a real timestamp
    Pending(String /* agent */),      // undated fact about an agent — awaits a cross-file join on that agent
    Leader,                           // genuinely undated, about no agent — rides at the head permanently
}
```

The wire carries `Statement`s (`src/fact.rs`), not any provider's records: one
per record read, holding the record's own time plus the facts it stated. The
two times differ on purpose — a record sits on the timeline where the file
wrote it (the only moment a live viewer could have known it), while each fact
keeps its own time for the fold (a completion record can also say when the
call started). A feeder opens a session with `provider::open` and holds one
`provider::Stream` per tailed file (the provider's own stream inside: Claude's
carries the inherited timestamp for lines that lack one, Codex's the thread
its file is by) and calls `push(line)`; whole-read sidecars (Claude's
`meta.json`) are stated once through `Provider::sidecar`, and files that
appear later come from `Session::rescan`. Nothing above a feeder sees a
provider's record or path vocabulary. `Timing` for an undated statement reads its facts' envelopes —
`Pending(agent)` if it is about an agent, else `Leader` — and dating joins a
birth (`Agent`) to that agent's first dated activity and an ending (`Ended`)
to its last.

**Two load strategies, one tail loop.** Both feeders end in the shared `tail_loop`, so EVERY session keeps tailing for appends (a replayed file that grows just "goes live" on its own — completion is unknowable, so nothing is ever assumed finished):
- **File target** (`run_replay`): `build_replay` parses every session file, dates undated statements — sidecar births, ledger endings — (`date_and_sort`), **routes session-level statements into `SessionInfo`** (off the timeline via `Statement::is_session_meta`), and merges the rest into a ts-sorted `Vec<ReplayItem>` → one `ReplayLoaded`. Then enter `tail_loop`, resuming each file's tail from the **byte offset the parse consumed** (a *snapshot seed* — not live EOF — so lines appended *during* the parse aren't dropped). Auto-switch disabled (`follow = None`; you asked for this file).
- **Dir/none target** (`run_live`): announce `SessionReset` (id adoption), then `tail_loop` — the first poll backfills the existing file (arrival order); subsequent polls emit appends; the project dir is re-scanned for a *newer* session (throttled auto-switch: `SWITCH_SCAN_EVERY`~2s, only after `SWITCH_IDLE_TICKS`~30s idle, dir targets only).

Per-file tail state `{ offset, partial, overflowed, identity: (dev, ino) }`. Each tick: stat the file; a shrink (`len < offset`) **or an inode swap** (rotation — a different `(dev,ino)` even if not shorter) → reset + `SessionReset` and re-attach; grown → read appended bytes, split on `\n`, hand complete lines to the file's provider `Stream`, buffer the trailing partial (a runaway line past `MAX_PARTIAL`=8 MiB is dropped, not buffered forever). asks `Session::rescan` for files that appeared; absent dirs are fine).

**Everything stamped with session_id; App drops events where `!is_current(session_id)`** (rwy's identity-stamping; stale buffered messages across a switch).

## Timeline — the unified replay/live model (state/timeline.rs)

**Live and replay are NOT two modes — one time-shifted timeline (time-travel).** There is one ts-ordered item list and one playhead (`cursor`); the only real difference is whether the right edge is fixed (a finished file) or growing (a session being written). The **edge is always the last event** — never wall-clock now — so an old session never grows an empty tail toward the present.

```rust
pub struct Timeline {
    pub items: Vec<ReplayItem>,   // bulk-loaded, then appended as the feeder tails
    pub replay: bool,             // launch intent: replaying a recording vs following live. NOT a completeness claim (a replay can grow & go live). Not runtime-derivable. Does NOT gate pacing (the edge does); gates the `now` reference (playhead vs wall-clock) + the end-settle latch
    pub cursor: Option<DateTime<Utc>>,  // playhead = the universal "now" for rendering
    pub folded: usize,            // items applied to the derived model so far
    pub follow_head: bool,        // pinned to the edge (playing/following) vs parked (scrubbed)
    pub speed: f64,
    pub compress_gaps: bool,      // skip idle dead air (default on); `s` toggles faithful pacing
    // + cached head, gap-pacing anchor/elapsed, `undated_agents` (Pending-item join set), ended latch
}
```

- **Pin-vs-pace is decided by the edge, not the mode.** `advance`/`append_live` compare cursor-vs-head: behind the edge the cursor always **paces forward**; only at the edge does it **pin** (and live appends snap in). So `space` resumes from the playhead in *both* modes, and a scrubbed-back live session **catches up** to the edge then follows — there's no "play = jump to live." `End`/`go_live` is the explicit jump.
- **`replay`** is the one surviving "mode" bit — the **launch intent**: are you replaying a recording, or following a live session? Set from the launch `Mode`, and **not runtime-derivable** (a quiet live session is byte-identical to a finished recording, so the flag can't be eliminated). It is NOT a claim the file is complete — a replay can grow and go live (the feeder always tails; nothing is assumed finished), which is why it's named for the intent, not a "bounded/complete" property. It does NOT gate pacing (the edge does); it gates only the `now` reference and the end-settle latch.
- **Pacing** (`advance`, per 16ms frame): paces the cursor toward the next event, **compressing dead air** — but not with a flat cap. `compress_gap` is a **log-compression** curve (`GAP_FAITHFUL_KNEE`=0.8s, `GAP_COMPRESS_SCALE`=0.6): real-time below the knee, then `knee + scale·ln(1 + (t−knee)/knee)` above it — *graded*, so a 5-minute wait still reads longer than a 5-second one (an hour of dead air crosses in <10s). The `s` key sets `compress_gaps = false` for faithful real-time pacing. The App folds the prefix `items[0..fold_target()]` (`App::fold_to`); the live append and replay paths share it.
- **`now` reference** = wall clock only at a *live* edge (`!replay && follow_head && at_edge`), the cursor otherwise (incl. live catch-up) — so a replay always judges liveness as-of-the-playhead (its timestamps are a past recording, unrelated to wall time). See Status rules.
- **Seek / scrub** (`App::seek`, `seek_to_fraction`, `seek_prompt`, `go_live`): forward → fold in place (cheap); backward → `App::rebuild_to` re-folds the prefix into a fresh `SessionModel` and re-syncs, carrying view across by id (`graph::restore_positions` + `select_node`). A seek is discontinuous → ephemerals reset (chips re-baseline via `adopt_baseline`, then the per-frame `reconcile` reconstructs in-flight runs from state; glide cancels — see [`ARCHITECTURE.md`](ARCHITECTURE.md) §5). `space` is a unified play/pause that resumes from the current cursor; `End`/`go_live` re-pins to the edge.
- **Scrubber position is event-indexed, not time-linear** — real sessions cluster work then sit idle (the rwy sample: ~11 min across 10.65 h), so a time-linear bar would bury all action in a sliver. `progress` / `fold_at_fraction` map the bar over `[floor, len]` where `floor` is the unavoidable start clump (same-timestamp ties + dated metadata that can only fold atomically), so the leftmost click reaches position 0. `gap_markers` (≥`GAP_MARKER_SECS`=60s) place the fast-forward `»` markers on the marker strip. Because the axis is event-indexed, a raw event-count would be flat — so the track is a **tool-activity sparkline** (per-column count of `ToolStart` facts over its item range) which peaks where the work happened; see UI.
- **Emergent transport** (`App::transport` → Live / Playing / Paused / History / Idle): "Live" = following the edge **and** a fresh append (`last_batch_at` within ~10s), so a resumed *replay* reads Live and an old followed session reads Idle. Drives the status badge + scrubber tag — never a hardcoded mode.

## Event loop — native (tui.rs)

The native terminal loop. The **browser frontend** (`web/wasm/src/main.rs`) runs an equivalent loop driven by ratzilla's `requestAnimationFrame`: the *same* per-frame ticks (`tick_auto_pan`/`tick_animation`/`tick_camera`/`tick_timeline`), but it calls `status_tick` every frame (no ~1s gate) and takes input via exported `zoetrope_load`/`zoetrope_append` JS entry points instead of a crossterm stream. The portable core is shared (`lib.rs`); only the loop + IO differ.

```
ratatui::init() → execute!(EnableMouseCapture)
spawn crossterm EventStream reader → unbounded mpsc
tick = interval(16ms)
loop {
    let elapsed = now - last_tick;
    flow.tick_auto_pan(elapsed);                   // return value may be ignored (rwy does)
    flow.tick_animation(elapsed);                  // marching-ant edges (rwy lacks this)
    app.tick_camera(elapsed);                      // ease the Follow CameraGlide
    app.tick_timeline(elapsed);                    // advance the replay playhead + fold due items
    last_tick = now;
    // ~1s: app.status_tick() re-derives interactive liveness for a quiet session
    terminal.draw(|f| ui::draw(f, app))?;          // draw EVERY iteration or animation freezes
    tokio::select! {
        _ = tick.tick() => {}
        Some(ev) = ui_rx.recv() => app.handle_ui_event(ev),
        Some(ev) = event_rx.recv() => if handler::handle_event(&ev, app, &tail_tx) { break },
    }
    while let Ok(ev) = event_rx.try_recv() { if handler::handle_event(&ev, app, &tail_tx) { break } }  // drain → no mouse lag
    while let Ok(ev) = ui_rx.try_recv() { app.handle_ui_event(ev) }
}
execute!(DisableMouseCapture); ratatui::restore()
```
The crossterm input channel is **unbounded** (input must never block); the **cap-32 bounded** channels (`CHANNEL_CAP`, `main.rs`) are the tailer-request + UI-event channels — they backpressure on `send().await`, and the tailer batches per tick so this is fine. Panic hook: `ratatui::init` installs screen restore but NOT mouse-capture disable — a custom hook layer also disables mouse capture.

## Graph sync (state/graph.rs — rwy's incremental pattern)

- **Never rebuild — except a backward seek.** Forward (live/replay playback): `flow.node_content_mut(id)` → mutate in place; else `flow.add_node(...)` + `flow.add_edge(...)` (duplicate-id `Err` is an idempotent no-op; add nodes before edges). The ONE exception is scrubbing into the past: folding is forward-only, so `App::rebuild_to` builds a fresh `SessionModel` + `Flow` from the prefix and `graph::restore_positions` carries node positions across by id (selection too) so the layout doesn't jump.
- Node: `Node::new(id, (0.0, 0.0), (W, H), AgentNode{…})` — fixed dims ~`(30.0, 7.0)` main/workflow, ~`(26.0, 6.0)` subagents (explicit dims; no DOM-style measuring). Handles: `Handle::source(HandlePosition::Bottom).with_hidden(true)`, `Handle::target(HandlePosition::Top).with_hidden(true)` (clean look, rwy does this).
- **Layout (strictly user-driven):** `sync` never auto-relayouts (it retains a `relayout` param, but every caller passes `false`). A Sugiyama pass on every new node reflowed the whole graph and read as "jumpy" as a session grew (confirmed by toggling it off), so newcomers always get local placement (below parent, fanned past siblings) and nothing existing ever moves on its own. The ONLY relayout trigger is `r` (`App::relayout_now` → `flow.apply_layout(Sugiyama::vertical())` + reframe for the current camera). Layout is orthogonal to the camera: `o`/`f` move the viewport only and never rearrange nodes. `layout_dirty` (set on structural growth, cleared by `r`) drives a subtle status-bar hint so pending growth is discoverable. (Earlier designs auto-relayouted, then auto-relayouted except in Manual, then applied debt on `o`/`f`; all superseded — layout is now always explicit.)
- **Camera** (supersedes the original "fit only on first populate" — a one-shot fit goes stale as the graph grows): three mutually exclusive modes on `App.camera`. **Overview** (default) re-requests fit-view on every structural change — the camera pulls back as the swarm grows. **Follow** holds readable zoom (≥ `FOLLOW_ZOOM`) and centers on the most recently active agent (`SessionModel::last_active_agent_id`, latest `last_ts`, spawn-order tie-break). **Manual**: a uniform rule in `process_flow_events`, not per-event — every `FlowEvent` there is a user gesture (programmatic `select_node`/`center_on`/`set_offset` are quiet and never surface), so **Follow yields to ANY interaction**: click, spatial nav (`SelectionChanged`), pan/zoom (`ViewportChanged`), or node drag (`NodeDragged`). **Overview** yields only to a viewport change — selecting/dragging while auto-framing is fine to leave in Overview. Dropping Follow on selection is deliberate: the user is inspecting a node, and spatial nav already pans to keep it visible (rataflow's `ensure_selected_node_visible`, 1-cell margin) — staying in Follow would fight that and glide back over the selection. Keys name destinations: `o` → Overview, `f` → Follow — the only exits from Manual. Session reset → Overview. Follow auto-narrates the panel (quiet `select_node`, no event); a user selection ends that by dropping to Manual. Status bar shows `⌖ overview` / `⌖ follow` / nothing. Camera moves in Follow are eased (`CameraGlide`), cancelled the instant the user takes over.
- **Semantic zoom:** `AgentNode` renders at two levels chosen by on-screen size. **Card** (default): priority-ordered lines (title → description → tools → status) with ellipsis overflow — degrades continuously. **Cell** (below `CELL_MIN_WIDTH`/`CELL_MIN_HEIGHT`, ~zoom 0.5): solid status-colored fill, no border/text — a zoomed-out swarm reads as a field of status cells. Edge labels follow the same rule: `AgentEdge` (wraps `StepEdge`) measures effective zoom through `ctx.world_to_terminal` at render time and drops labels below card scale; chips are width-gated likewise. Swarm view = cells + animated edges only.
- `flow.set_edge_animated(edge_id, running)` on status change; node card colors react to status via content mutation.
- Flow config: `Flow::new().with_deselect_on_pane_click(false)` + `flow.deselect_on_drag = false` (detail panel persists), `with_min_zoom(0.1)` (Sugiyama trees outgrow default 0.5 fit-view limit).
- Selection survives sync because node ids are stable (`main`, agentId, wf-id) and we never clear()+re-add.

## UI

- **AgentNode card** (ui/nodes.rs): border + title (glyph + agent_type, or "claude" for main), description (truncated), tools line (`⚒ N · last_tool`), footer (**status word + output token count**, e.g. `running · 1.2k tok`). Read `ctx.theme.palette()`, `ctx.selected`. **Five status glyphs** (single source: `AgentStatus::glyph`/`status_word`/`status_color`): `●` running (green; a `●`/`○` pulse on the animation clock), `◌` idle (subtle), `✓` done (gold/accent), `✗` failed (red), `■` stopped (muted). *(The help-overlay legend still lists only 4 — Stopped is omitted there.)*
- **Detail panel** (ui/panel.rs): when `flow.selected_nodes().next()` is Some → a **30/70** horizontal split (orientation canvas 30% · panel 70%); panel shows the selected agent's description, model, status, timing, and a scrollable recent-tool-call list (name + summary, `⏳`/`✓`/`✗` + local time; path tools keep the basename). Data from `SessionModel`, keyed by node id. Copy the selected id out before borrowing app mutably elsewhere (borrow-checker note from rwy).
- **Tool-call chips** (ui/chips.rs): ephemeral `⚒ read ×N` overlays anchored *below* agent cards (NOT graph nodes — no layout/minimap/hit-test), drawn in `render_canvas` after the flow. One reconcile pass per frame ages them in watch-time; pending persists as the in-flight indicator, completed fade (`CHIP_TTL` 2.5s, err 4s, ≤3/agent), width-gated like edge labels. This is where "current tool" lives now — edges carry no labels. Full model: [`ARCHITECTURE.md`](ARCHITECTURE.md) §5.
- **Scrubber** (`render_scrubber`, shown when the timeline has a span): a **bordered panel** (rounded, subtle), 6 rows = border + marker strip (1) + bars (2) + info (1) + border. Markers and bars are on **separate rows** so neither can overwrite the other (a marker on a bar cell hid real activity; the gap seam was the worst offender).
  - **Marker strip (1 row, on top)**: **fast-forward `»`** at idle-gap columns (≥`GAP_MARKER_SECS`, where playback compresses dead air; full-session; drawn only when gap-compression is on); **spawn** (an `Agent` birth, or a `Spawn` call whose agent never appeared) drawn as the session's provider's emblem — Claude's sunburst `❋` in coral ≈ xterm 173, Codex's circled star `❂` in green ≈ xterm 36, a plain `✦` in a neutral before the root is stated (`ui::spawn_mark`; the browser's font atlas drops the colour and shows the glyph alone) and **failure `✗`** (red, a failed `ToolEnd`), **past-only** (`c < head`) so they reveal as the playhead reaches them (in sync with the graph's chips).
  - **Activity bars (2 rows)**: a tool-call sparkline via ratatui's `Sparkline` — per-column height = tool calls in that slice (`ToolStart` facts counted over the column's item-index range, binned on the event-index axis). Counts normalized to the available eighths (`rows × 8` = 16) with a **floor of 1 for any nonzero column** (`ceil(count/max × levels)`) — else the busiest column scales the rest down and a low-activity tick rounds to 0 (invisible). Played/unplayed fill: bright accent left of the playhead, dim right.
  - **Playhead**: a gold vertical line `│` over a translucent (`muted`-bg) column, spanning the marker strip + both bar rows.
  - **Info row**: playhead date+time (left), transport tag (right). Full-width so changing labels can't reflow it; the whole row is the seekable area, so a click maps to the exact width the playhead is drawn over. Row 2 is an info line: the playhead's local date+time (left) and the emergent transport tag (right). `App.scrubber_area` is recorded each frame for hit-testing mouse drags.
- **Status bar**: gold `zoetrope` wordmark, emergent transport badge (● LIVE / ▶ PLAY / ⏸ PAUSE / ⏮ PAST / ■ IDLE), session title, agent & tool counts, camera mode, last error, key hints. (`q` quit, `? `help.)
- **Overlays**: `?` help (full key reference) and `i` session info (the untimed `SessionInfo`: mode, permission, last prompt, queued/file-edit counts) — both centered, `esc` closes.
- **Companions**: `Background::new(&flow)` then `&mut flow` then `MiniMap::new(&flow)` (render order matters; Widget impl is on `&mut Flow`, companions take `&Flow` — separate render_widget calls avoid borrow conflicts).
- Keys: `q`/`ctrl-c` quit; `space` play/pause (resume from cursor); `s` toggle gap-compression (faithful vs skip-idle pacing); `o`/`f` camera Overview/Follow; `r` relayout (tidy); `[`/`]` step prompt eras; `End` or `g` go-live; `?`/`i` overlays; `esc` closes overlay / detail panel. Detail-panel scroll: `j`/`k`/PgUp/PgDn. Remaining nav/zoom/pan → `flow.handle_key_event` / `handle_controls_key_event` (whitelisted — the graph is read-only, destructive library bindings are blocked). Scrubber-row mouse press/drag → `App::seek_to_fraction`; other mouse → `flow.handle_mouse_event`. Consume `into_events()`; any flow event drops Follow (`process_flow_events`).

## inspect subcommand

`zoe inspect <file|id>`: open the session (`provider::open`, any provider, any file of it, or an id), fold every file, and print: session title, **session info** (the provider's labelled rows: mode · permission · queued · file edits · last prompt for Claude; app · version · cwd for Codex), agent/tool totals, then the agent tree (type, description, status, #tools, tokens). Exit non-zero on an unreadable or unrecognised file. **This is the headless smoke test** — CI-runnable end-to-end check of parser + session model + info extraction with no TTY.

## Testing (inline #[cfg(test)], no tests/ dir)

Worth testing: transcript line parsing against real-format fixture strings (every entry type incl. flat metadata, polymorphic content, missing is_error, Unknown), sanitization rule, partial-line buffering + truncation reset (tailer state machine over an in-memory/tempfile sequence), session model status transitions (spawn → running → done/failed; the async layer — spawn-ack supersession, `<task-notification>` terminal report, `end_of_stream`; workflow journal completion; pending-tool liveness), graph sync idempotency (same update twice = no duplicate nodes; selection preserved), and the chip reconcile behaviors (aggregation, afterglow, pending reconstruction). Not worth testing: render output, getters.

**Order-independence is guarded by property tests** (the load-bearing invariant — ARCHITECTURE.md §1.1): `live_delivery_converges_to_bulk_ordering` (timeline.rs — 400 random per-file interleavings land the same ts sequence as the bulk sort, nothing left undated) and the model shuffle-invariance test (session.rs — final model state is a pure function of the fact set). the test suite, all inline; no `tests/` dir.

## Pitfalls checklist (from research — verify before calling done)

- [ ] `rataflow::Error` (no FlowError); `add_edge_from_connection(conn, content)` two args (unused in v1 — read-only graph)
- [ ] Draw every loop iteration; post-select try_recv drains; unbounded crossterm channel
- [ ] `tick_animation` wired (rwy reference loop lacks it)
- [ ] Partial trailing line buffered; `len < offset` → reset + SessionReset
- [ ] `parentUuid` present-and-null (root) vs absent (metadata) — Option handling, lean variants for flat types
- [ ] `is_error` missing = success
- [ ] user content + tool_result content polymorphic string|array
- [ ] Only `<uuid>.jsonl` in project dir + `subagents/**/agent-*.jsonl`; never skill-injections.jsonl/journal as transcript
- [ ] camera modes per the Camera section (Overview auto-fit / Follow tracking / Manual); min_zoom raised
- [ ] No network deps anywhere in the tree
- [ ] First render has zero canvas size — `request_fit_view` (deferred) not `fit_view`
