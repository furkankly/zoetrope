# zoetrope — Design Document (v1)

**zoetrope** is a terminal UI that visualizes Claude Code agent sessions as a live flow graph: the main agent, its subagents, workflows, and tool activity — rendered with [rataflow](../../rataflow). Synthesized from a multi-agent research pass over rwy (`/Users/furkan/personal/projects/rwy`), rataflow (`/Users/furkan/personal/projects/rataflow`), and real transcripts under `~/.claude/projects/`. Full research: see the workflow output referenced in the repo history.

> **This is the v1 structural spec** (module map, transcript format, type shapes). For the *invariants and principles* the implementation now follows — order-independence, the content-vs-presentation clocks, ground-truth-over-heuristics, and the derived-state heuristics catalogue — see [`ARCHITECTURE.md`](ARCHITECTURE.md).

## Hard constraints

1. **No network IO, ever.** All IO is local filesystem. No reqwest/hyper/etc. in the dep tree (dev-deps included). "Your transcripts never leave your machine" goes in the README.
2. **Defensive parsing.** The transcript format is undocumented/internal. Unknown entry types, missing fields, malformed lines: skip, never panic. Mirrors Claude Code's own resilience.
3. **rwy's async architecture**, ported: single-task UI loop owns all state; background tokio tasks feed typed messages over bounded mpsc; no `Arc<Mutex>`.
4. rataflow is a **path dep** (`../rataflow`). Its error type is `rataflow::Error` (NOT `FlowError` — that name doesn't exist).

## CLI

One TUI command over the unified timeline engine (see the **Timeline** section) plus the headless `inspect`. The launch only picks **defaults** — *what* to open and *where the playhead starts*; once open, scrub / follow / pause / go-live all work regardless.

```
zoe                      # follow the current project's live session
zoe <file.jsonl>         # replay a recording, played from the start
zoe <dir>                # follow another project's live session
zoe <file> --follow      # ride a file's live edge instead of replaying it
zoe <file> --speed 8     # playback speed (default 8.0)
zoe inspect <file.jsonl> # no TUI: print session info + parsed tree (smoke-test)
zoe sessions             # no TUI: list every session active in a window, with sweep timings
zoe sessions --since 120 --root DIR
```

Resolution: a **file** target bulk-loads + tails (replay feeder); a **dir** (or none → cwd) discovers the latest session and live-tails. `--follow` only changes the start position (head vs beginning) via `Mode`. `Cli = View { target: Option<PathBuf>, follow: bool, speed: f64 } | Inspect { file } | Sessions { window, root }`. Arg parsing: hand-rolled over `std::env::args` (no clap; keep deps lean).

`ZOE_PROJECTS_ROOT` overrides `~/.claude/projects` for both discovery and `project_dir`. It exists because the multi-session rail is otherwise only exercisable by running several real Claude Code sessions at once; with it, a fixture workspace is a directory. `zoe sessions --root` does the same for the headless sweep.

## Workspace layout

Two crates. `zoetrope` (repo root) is the published one: the portable core plus the native frontend, split by a Cargo feature (`default = ["native"]`). `zoetrope-web` (`web/wasm/`) is the browser frontend — `publish = false`, built only for `wasm32`.

The split keeps the browser frontend *out of the published crate*: nothing wasm ships to crates.io, and depending on the `zoetrope` library from a wasm target imposes no ratzilla/getrandom choices on you.

The browser frontend is **excluded** from the root workspace rather than being a member of it, so it resolves as its own workspace with its own lockfile and target dir. This is the mainstream shape for a wasm frontend, not a workaround of last resort: putting one in a workspace is what produces the well-known trunk/`wasm-bindgen` version-mismatch failures, because membership forces a single `wasm-bindgen` across crates that have no reason to agree on one. The reason is that it cannot be compiled for the host *at all*: rataflow gates its ratzilla `From` impls on `all(feature = "ratzilla", target_arch = "wasm32")`, so a host-target check fails to typecheck. `default-members` would have kept it out of a bare `cargo build`, but not out of `cargo check --workspace` — and not out of rust-analyzer, which would sit on two permanent phantom errors. Excluding it is the only thing that makes the editor honest. `web/wasm/.cargo/config.toml` sets `[build] target = "wasm32-unknown-unknown"` so cargo and rust-analyzer both default to the right target there — it sits next to `Trunk.toml` in the crate root, which is where trunk runs from, so one copy serves everything; cargo's own answer to this, `per-package-target`/`forced-target`, is still nightly-only.

Two lockfiles means the two frontends can drift apart. Most of that drift is harmless and even desirable — `wasm-bindgen`, `getrandom` and the ratzilla stack should track the browser build's needs, not the terminal's. What must **not** drift is anything that decides what gets drawn: the ratatui tree (`ratatui`, `ratatui-core`, `ratatui-widgets`, and their `unicode-width` / `lru` / `line-clipping` / `instability`) plus rataflow's `rust-sugiyama`. Those are pinned to the same versions in both lockfiles on purpose. If you `cargo update` one, re-check the other.

```text
Cargo.toml          # [workspace] exclude = ["web/wasm"]  — its own workspace, own lockfile
src/                # the zoetrope library + the `zoe` bin (src/main.rs)
web/wasm/           # the zoetrope-web crate: Cargo.toml, index.html (trunk entry), src/main.rs
```

## Dependencies (Cargo.toml)

`edition = "2024"`, `license = "MIT"`.

```toml
# Portable core (native + wasm): model, timeline, graph, UI rendering, parsing.
# imbl — persistent collections. SessionModel is built from them so cloning it is
#   O(1) and can be used as a snapshot (see Timeline / ARCHITECTURE.md §6.1).
#   Arc-backed by default, so the model stays Send for the tokio tasks.
# memchr, memmap2 — native-only, behind the `native` feature: the skeleton index's
#   SIMD probes and its no-copy re-scan of already-indexed files.
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
├── transcript.rs  # serde model for JSONL entries + meta.json sidecars + project-dir discovery/sanitization
├── index.rs       # native-only: skeleton index — 32 bytes/line of facts, no body parsing; append-incremental, cached
├── sessions.rs    # native-only: multi-session discovery (stat sweep) + Summary folded from index records
├── monitor.rs     # native-only: the monitor fleet — one tailer per live unfocused session; publishes rail rows
├── state/
│   ├── mod.rs     # App: owns the Flow + SessionModel + Timeline + SessionInfo + UI state; handle_ui_event, seek, camera
│   ├── session.rs # SessionModel: the pure domain model (agents, statuses, tool calls) derived from parsed updates
│   ├── timeline.rs# Timeline: the ts-ordered item list + playhead (time-travel); pacing, gap-compression, seek, floor
│   ├── graph.rs   # incremental SessionModel → Flow projection; session-scoped node ids; shelf-packs session trees
│   ├── rail.rs    # portable session-rail state: rows, focus derivation, visibility (fed by native discovery)
│   └── info.rs    # SessionInfo: untimed session metadata, folded off the timeline (i overlay + inspect header)
├── tailer/        # background FEEDER (pure: no pacing/seeking — the App owns the playhead)
│   ├── mod.rs     # task entry + shared wire types (TailRequest / UiEvent / Update / Source)
│   ├── live.rs    # live tailing — one poll loop per session, emits UiEvent::Batch
│   ├── replay.rs  # replay assembly (native): parse all files up front, merge by ts, then keep tailing
│   ├── item.rs    # portable replay-stream pieces — ReplayItem + Timing (dating undated items); IO-free (wasm)
│   └── bytes.rs   # incremental byte reader: stat / read-appended / split-on-\n / buffer-partial (pure, testable)
└── ui/
    ├── mod.rs     # draw: canvas + scrubber + status bar; help/info overlays
    ├── nodes.rs   # AgentNode: the agent-card NodeContent (semantic zoom: Card vs Cell)
    ├── edges.rs   # AgentEdge: parent→agent EdgeContent (animated while target Running)
    ├── chips.rs   # ephemeral tool-call chips — one reconcile pass, anchored to agent nodes (see ARCHITECTURE.md §5)
    ├── rail.rs    # the session rail pane: one row per discovered session, themed from the flow palette
    └── panel.rs   # detail panel for the selected agent
```

## Multi-session

The canvas shows **every live session**, each as a root card with its agents
beneath it. Only one session is *focused*; the rail picks which.

```text
discovery (2s sweep, blocking pool)
  sessions::discover   stat every ~/.claude/projects/*/<uuid>.jsonl + its sidecars
  sessions::Summary    fold index records → counts, first/last activity, cwd
      │
      ├── UiEvent::Sessions(Vec<RailRow>) ──────────────► App.rail        (the rail pane)
      └── monitor::supervise: one tailer per live UNFOCUSED session
             └── UiEvent::MonitorBatch / MonitorReset ──► App.others      (model only, no timeline)

focus tailer (unchanged)
      └── UiEvent::Batch / SessionReset ────────────────► App.session     (model + Timeline)
```

- **`App.session` vs `App.others`.** The focused session owns a `Timeline` and can
  be scrubbed; monitored ones are folded straight off their live edge and only
  render. One collection holding both would either give every session a timeline
  it never uses, or pretend the focused one is not special when every seek path
  says it is.
- **Node ids are session-scoped** (`<session>/<agent>`, `graph::node_id`). Every
  `SessionModel` names its root `"main"`, so a shared flow needs the session in
  the id or the roots collide — silently merging two sessions into one tree.
- **Focus is derived, never stored twice.** The rail marks whichever row matches
  `App.current_session_id`, which `SessionReset` updates when a switch actually
  lands. A parallel focus field could point at a session the tailer is not
  watching.
- **Only *running* sessions get a monitor** (activity within `RAIL_IDLE_SECS`),
  capped at `monitor::MAX_MONITORED`. The rail lists everything from the last day;
  the canvas is for what is happening now.

## Skeleton index (index.rs)

A second reader of the same format, deliberately: the timeline needs a few dozen
bytes per line (when, what kind, whose agent, how much tool activity) out of a
line that is mostly payload.

- One fixed **32-byte `Rec` per line** — offset, length, timestamp, kind, agent
  hash, tool/spawn/failure counts, flags. Bodies stay on disk.
- **Envelope fields are read from the TOP LEVEL only.** Claude Code does not emit
  envelope keys first, and nested objects reuse the names — an `attachment` entry
  serializes `"attachment":{"type":…}` *before* its own `"type"`. Taking the first
  match in the line reads the payload's type as the entry's kind.
- **Activity counts come from real `message.content` blocks**, scoped by entry kind
  the way the parser scopes them. A `user` line quoting output that mentions
  `"name":"Agent"` is not a spawn.
- **It never guesses.** Where the format is genuinely ambiguous (a legacy `user`
  line with no `origin`) the record says so via `flags::AMBIGUOUS_PROMPT` and the
  caller parses that one line properly.
- **Append-only ⇒ extendable.** A grown file is scanned from `scanned_len`; the
  result is cached under `$XDG_CACHE_HOME/zoetrope/idx/`, guarded by a head hash
  so rotation is detected rather than misread. Bump `MAGIC` when any probe's
  meaning changes.
- `index::tests::skeleton_agrees_with_parser_on_a_real_corpus` (opt-in, via
  `ZOE_DIFF_CORPUS`) asserts record-for-record agreement with `transcript::parse_line`.
  That test is the only thing keeping two readers of one format honest — both
  format-drift bugs above were found by it, not by review.

## Transcript format (verified against real data, Claude Code 2.1.153–2.1.165)

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

```rust
pub struct SessionModel {
    pub session_id: String,
    pub agents: BTreeMap<String, AgentInfo>,  // keyed by node id ("main", the 17-hex agentId, or the wf-id)
    pub spawn_order: Vec<String>,             // stable discovery order (BTreeMap key order ≠ spawn order)
    pub last_activity: Option<DateTime<Utc>>,
    // Order-independent join stores — a fact attaches whether it arrives before or
    // after the thing it refers to (ARCHITECTURE.md §1.1):
    //   completed_spawns: tool_use_id → (is_err, ack_ts)  — the Agent-tool result, a SPAWN ACK
    //   task_terminal:    agent_id    → AgentStatus        — <task-notification> report (authoritative)
    //   journal_done:     {agent_id}                       — workflow journal `result` completion
    //   spawn_context:    tool_use_id → SpawnContext       — provenance (prompt / reasoning)
    //   prompts:          Vec<PromptInfo>                  — prompt eras (prompt_for_ts attribution)
    //   last_main_text:   Option<String>                   — cross-line reasoning fallback
}
pub struct AgentInfo {
    pub kind: AgentKind,                 // Main | Subagent | WorkflowGroup
    pub interactive: bool,               // main/fork — no completion signal; selects the liveness branch
    pub agent_type: Option<String>,      // "claude-code-guide", "workflow-subagent", "fork", …
    pub description: Option<String>,     // meta.description or Agent tool_use input.description
    pub parent: Option<String>,          // node id of parent (main or wf-id)
    pub spawned_by_tool_use: Option<String>, // toolUseId — the spawn/completion join key
    pub status: AgentStatus,             // Running | Idle | Done | Failed | Stopped
    pub(crate) terminal: bool,           // authoritative completion — pins against time-derived revival
    pub model: Option<String>,
    pub tool_calls: Vec<ToolCallInfo>,   // {id, name, summary: Option<String>, ts, state: Pending|Ok|Err}
    pub output_tokens: u64,              // summed from usage (deduped per requestId)
    pub first_ts, last_ts: Option<DateTime<Utc>>,
    // + internal indices: tool_index (tool_use_id → slot), seen_request_ids (token-dedup)
}
```

**Untimed session metadata → `SessionInfo` (not the timeline).** The lean flat-metadata the model never read (`mode`, `permission-mode`, `last-prompt`, `queue-operation`, `file-history-snapshot`) carries no timestamp, so it would otherwise clump at the front of the sorted timeline. It's routed into `SessionInfo { title, permission_mode, mode, last_prompt, queued_ops, file_snapshots }` (`apply` is latest-wins by file = chronological order) and shown in the `i` overlay + `inspect`. Lives on `App` (not `SessionModel`), so it survives backward-seek rebuilds — it's session-constant. **`ai-title` folds into `SessionInfo.title`** (the header bar) — it is NOT on `SessionModel` (moved off it so the model holds only timed, foldable state).

**Graph topology (v1): nodes are agents, not messages.** One node per agent + one group node per workflow run. Edges: `main → direct subagent` (edge id = toolUseId), `main → workflow node` (edge id = wf-id), `workflow → its subagents`. Sessions have 800+ lines — per-message nodes would be noise; agents are the story.

**Status rules** — the concrete derivations; the *principles* they follow (ground-truth-over-heuristics, reversibility, the async completion model) are in [`ARCHITECTURE.md`](ARCHITECTURE.md) §2–4.

- **The `Agent` tool result is a SPAWN ACK, not a completion** (`"Async agent launched successfully"`). A direct subagent is completed by the main-transcript `tool_result` (`tool_use_id == spawned_by_tool_use`, `is_error` → Failed) **only if the ack is not superseded** by the agent's own later activity — `resolve_spawn_status`: `last_ts > ack_ts` ⇒ still `Running`, non-terminal. A superseded (async) subagent stays `Running` and settles to `Done` at `end_of_stream`.
- **`<task-notification>` is the authoritative terminal report** for a background agent (`apply_task_notification` → the `task_terminal` store: `completed`→`Done`, `stopped`→`Stopped`, `failed`→`Failed`). It **outranks** the ack and time-derived liveness, and pins `terminal`. (`meta.stoppedByUser` is deliberately **not** applied — the meta folds at the agent's *first* activity, so applying it would strand the agent `Stopped` for the whole replay; only the timestamped notification is trusted.)
- **Workflow subagent**: `Done` when `journal.jsonl` has a `result` naming its `agentId` (`complete_journal_result`; pins `terminal`). Workflow node: an all-children-**terminal** rollup (`Failed` if any child failed, else `Done`; `Done`/`Stopped` both count as terminal; a childless group stays `Running`; re-derived every call, so it reverts if a running child is discovered late). The Workflow tool_use's `tool_result` is a *launch ack* ("Workflow launched in background…"), NOT a completion — never complete groups from it.
- **Liveness** (`recompute_liveness`, against the timeline's **`now` reference** — wall clock at a live edge, the playhead otherwise, so a scrubbed/paced view shows the as-of-then state with no wall-clock bleed): "active" = `now − last_ts ≤ INTERACTIVE_IDLE_SECS` (~2 min) **OR the agent holds a pending tool_call** (an unresolved tool is direct proof it's working — §2.2/§4). Interactive → `Running`/`Idle` (never claims completion); non-interactive non-terminal → `Running`/`Done`, **reversible**; non-interactive terminal → keeps its status. `end_of_stream` settles interactive agents to `Idle` and any still-`Running` async agent to `Done`.
- Edge `animated = target agent Running`.

## Tailer — the feeder (tailer/)

**Decision: poll-based, no `notify` dep** (poll is simpler, WASM-trait-friendly, 200ms is imperceptible). The tailer is a **pure feeder** — it no longer paces or seeks (that moved to the App's `Timeline`); it only produces an ordered update stream and keeps the files watched.

```rust
pub enum TailRequest { Watch(PathBuf) }                  // switch session; only request now
pub enum UiEvent {
    ReplayLoaded { session_id, items: Vec<ReplayItem>, speed, info: SessionInfo }, // bulk hand-off
    Batch { session_id: String, updates: Vec<Update> },  // appends, per poll tick (live + post-load tailing)
    SessionReset { session_id: String },                 // truncation/rotation/auto-switch
    Error(String),
}
pub enum Update {
    Entry { source: Source, entry: transcript::Entry },  // Source::Main | Source::Sub(agentId) | Source::Journal(wfId)
    SubagentMeta { agent_id, workflow: Option<String>, meta },
}
pub struct ReplayItem { timing: Timing, pub update: Update }   // .ts() → Some only when Dated
pub enum Timing {                     // how an item is placed on the timeline (tailer/item.rs)
    Dated(DateTime<Utc>),             // has, or has derived, a real timestamp
    Pending(String /* agent */),      // externally-dated (meta / journal) — awaits a cross-file join on that agent
    Leader,                           // genuinely undated — rides at the head permanently
}
```

**Two load strategies, one tail loop.** Both feeders end in the shared `tail_loop`, so EVERY session keeps tailing for appends (a replayed file that grows just "goes live" on its own — completion is unknowable, so nothing is ever assumed finished):
- **File target** (`run_replay`): `build_replay` parses every session file, dates untimed metas/journals/`ai-title` (`date_and_sort`), **routes the untimed flat-metadata into `SessionInfo`** (off the timeline via `is_timeline_noise`), and merges the rest into a ts-sorted `Vec<ReplayItem>` → one `ReplayLoaded`. Then enter `tail_loop`, resuming each file's tail from the **byte offset the parse consumed** (a *snapshot seed* — not live EOF — so lines appended *during* the parse aren't dropped). Auto-switch disabled (`project_dir = None`; you asked for this file).
- **Dir/none target** (`run_live`): announce `SessionReset` (id adoption), then `tail_loop` — the first poll backfills the existing file (arrival order); subsequent polls emit appends; the project dir is re-scanned for a *newer* session (throttled auto-switch: `SWITCH_SCAN_EVERY`~2s, only after `SWITCH_IDLE_TICKS`~30s idle, dir targets only).

Per-file tail state `{ offset, partial, overflowed, identity: (dev, ino) }`. Each tick: stat the file; a shrink (`len < offset`) **or an inode swap** (rotation — a different `(dev,ino)` even if not shorter) → reset + `SessionReset` and re-attach; grown → read appended bytes, split on `\n`, parse complete lines, buffer the trailing partial (a runaway line past `MAX_PARTIAL`=8 MiB is dropped, not buffered forever). Scan `subagents/**` each tick for new files (cheap readdir; absent dirs are fine).

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
    pub compress_gaps: bool,      // skip idle dead air (default on); `g` toggles faithful pacing
    pub generation: u64,          // bumped whenever an item INDEX changes meaning (re-sort / bulk load)
    // + cached head, gap-pacing anchor/elapsed, `undated_agents` (Pending-item join set), ended latch
}
```

- **Pin-vs-pace is decided by the edge, not the mode.** `advance`/`append_live` compare cursor-vs-head: behind the edge the cursor always **paces forward**; only at the edge does it **pin** (and live appends snap in). So `space` resumes from the playhead in *both* modes, and a scrubbed-back live session **catches up** to the edge then follows — there's no "play = jump to live." `End`/`go_live` is the explicit jump.
- **`replay`** is the one surviving "mode" bit — the **launch intent**: are you replaying a recording, or following a live session? Set from the launch `Mode`, and **not runtime-derivable** (a quiet live session is byte-identical to a finished recording, so the flag can't be eliminated). It is NOT a claim the file is complete — a replay can grow and go live (the feeder always tails; nothing is assumed finished), which is why it's named for the intent, not a "bounded/complete" property. It does NOT gate pacing (the edge does); it gates only the `now` reference and the end-settle latch.
- **Pacing** (`advance`, per 16ms frame): paces the cursor toward the next event, **compressing dead air** — but not with a flat cap. `compress_gap` is a **log-compression** curve (`GAP_FAITHFUL_KNEE`=0.8s, `GAP_COMPRESS_SCALE`=0.6): real-time below the knee, then `knee + scale·ln(1 + (t−knee)/knee)` above it — *graded*, so a 5-minute wait still reads longer than a 5-second one (an hour of dead air crosses in <10s). The `g` key sets `compress_gaps = false` for faithful real-time pacing. The App folds the prefix `items[0..fold_target()]` (`App::fold_to`); the live append and replay paths share it.
- **`now` reference** = wall clock only at a *live* edge (`!replay && follow_head && at_edge`), the cursor otherwise (incl. live catch-up) — so a replay always judges liveness as-of-the-playhead (its timestamps are a past recording, unrelated to wall time). See Status rules.
- **Seek / scrub** (`App::seek`, `seek_to_fraction`, `seek_prompt`, `go_live`): forward → fold in place (cheap); backward → `App::rebuild_to` restores the nearest **snapshot ladder** rung and folds forward from it, re-syncing and carrying view across by id (`graph::restore_positions` + `select_node`).
- **The snapshot ladder** (`App::snapshots`, `SNAPSHOT_STRIDE`): a rung every N folded items, taken *inside* the fold loop — a jump to the live edge folds the whole timeline in one call, so a ladder recording only where each fold *ended* would hold one useless rung at the edge. Restoring a rung is `SessionModel::clone`, which is O(1) because the model is built from persistent (`imbl`) collections. A rung stores **fold state only**; liveness and workflow rollups are playhead-dependent projections and are re-run by `resync` after every restore.
- **`generation` guards the ladder.** Item indices are NOT stable: `append_live` re-sorts the whole list whenever a batch carries untimed items or dates a previously-`Pending` one, and undated items sort ahead of dated ones — so a resolving item jumps right and shifts every index in between. A rung keyed to `items[0..N]` would then describe a different prefix. Rungs record the generation they were taken under and are discarded wholesale when it moves. `SessionReset` clears them outright, since a fresh `Timeline` restarts the counter and they could not be told apart by it. A seek is discontinuous → ephemerals reset (chips re-baseline via `adopt_baseline`, then the per-frame `reconcile` reconstructs in-flight runs from state; glide cancels — see [`ARCHITECTURE.md`](ARCHITECTURE.md) §5). `space` is a unified play/pause that resumes from the current cursor; `End`/`go_live` re-pins to the edge.
- **Scrubber position is event-indexed, not time-linear** — real sessions cluster work then sit idle (the rwy sample: ~11 min across 10.65 h), so a time-linear bar would bury all action in a sliver. `progress` / `fold_at_fraction` map the bar over `[floor, len]` where `floor` is the unavoidable start clump (same-timestamp ties + dated metadata that can only fold atomically), so the leftmost click reaches position 0. `gap_markers` (≥`GAP_MARKER_SECS`=60s) place the fast-forward `»` markers on the marker strip. Because the axis is event-indexed, a raw event-count would be flat — so the track is a **tool-activity sparkline** (per-column sum of `Entry::tool_use_count` over its item range) which peaks where the work happened; see UI.
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

- **Never rebuild — except a backward seek.** Forward (live/replay playback): `flow.node_content_mut(id)` → mutate in place; else `flow.add_node(...)` + `flow.add_edge(...)` (duplicate-id `Err` is an idempotent no-op; add nodes before edges). The ONE exception is scrubbing into the past: folding is forward-only, so `App::rebuild_to` restores a ladder rung and rebuilds the `Flow` from the prefix, with `graph::restore_positions` carrying node positions across by id (selection too) so the layout doesn't jump.
- **Node ids are `<session>/<agent>`** (`graph::node_id` / `split_node_id`, separator `graph::ID_SEP`). One flow holds every watched session and every `SessionModel` calls its root `"main"`, so the session has to be in the id. `sync` is the only place the model's session-local ids and the flow's ids meet.
- **Session trees are shelf-packed** (`graph::pack_sessions`): rataflow applies each connected component's Sugiyama coordinates verbatim — its layout loop discards exactly the per-component offsets that would separate them — so without a packing pass every session's tree stacks at the origin. Rows rather than one endless line, with the row count chosen for the best **screen** aspect (world units are cells, and a cell is ~2× taller than wide, so `CELL_ASPECT` converts before comparing). Packing moves whole sessions only; positions *within* a session are untouched, which is what lets `repack_if_overlapping` run on every structural change — a session that grows into its neighbour is separated, while a session the user dragged somewhere clear is left alone.
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
  - **Marker strip (1 row, on top)**: **fast-forward `»`** at idle-gap columns (≥`GAP_MARKER_SECS`, where playback compresses dead air; full-session; drawn only when gap-compression is on); **spawn `❋`** (the Claude sunburst, in Claude coral ≈ xterm 173 — `Entry::spawn_count`) and **failure `✗`** (red, `Entry::tool_failure_count`), **past-only** (`c < head`) so they reveal as the playhead reaches them (in sync with the graph's chips).
  - **Activity bars (2 rows)**: a tool-call sparkline via ratatui's `Sparkline` — per-column height = tool calls in that slice (`Entry::tool_use_count` summed over the column's item-index range, binned on the event-index axis). Counts normalized to the available eighths (`rows × 8` = 16) with a **floor of 1 for any nonzero column** (`ceil(count/max × levels)`) — else the busiest column scales the rest down and a low-activity tick rounds to 0 (invisible). Played/unplayed fill: bright accent left of the playhead, dim right.
  - **Playhead**: a gold vertical line `│` over a translucent (`muted`-bg) column, spanning the marker strip + both bar rows.
  - **Info row**: playhead date+time (left), transport tag (right). Full-width so changing labels can't reflow it; the whole row is the seekable area, so a click maps to the exact width the playhead is drawn over. Row 2 is an info line: the playhead's local date+time (left) and the emergent transport tag (right). `App.scrubber_area` is recorded each frame for hit-testing mouse drags.
- **Status bar**: gold `zoetrope` wordmark, emergent transport badge (● LIVE / ▶ PLAY / ⏸ PAUSE / ⏮ PAST / ■ IDLE), session title, agent & tool counts, camera mode, last error, key hints. (`q` quit, `? `help.)
- **Overlays**: `?` help (full key reference) and `i` session info (the untimed `SessionInfo`: mode, permission, last prompt, queued/file-edit counts) — both centered, `esc` closes.
- **Companions**: `Background::new(&flow)` then `&mut flow` then `MiniMap::new(&flow)` (render order matters; Widget impl is on `&mut Flow`, companions take `&Flow` — separate render_widget calls avoid borrow conflicts).
- Keys: `q`/`ctrl-c` quit; `space` play/pause (resume from cursor); `g` toggle gap-compression (faithful vs skip-idle pacing); `o`/`f` camera Overview/Follow; `r` relayout (tidy); `[`/`]` step prompt eras; `End` go-live; `?`/`i` overlays; `esc` closes overlay / detail panel. Detail-panel scroll: `j`/`k`/PgUp/PgDn. Remaining nav/zoom/pan → `flow.handle_key_event` / `handle_controls_key_event` (whitelisted — the graph is read-only, destructive library bindings are blocked). Scrubber-row mouse press/drag → `App::seek_to_fraction`; other mouse → `flow.handle_mouse_event`. Consume `into_events()`; any flow event drops Follow (`process_flow_events`).

## inspect subcommand

`zoe inspect <file.jsonl>`: parse the session fully (transcript + subagents + journal) and print: session title, **session info** (mode · permission · queued · file edits · last prompt, via `read_session_info`), agent/tool totals, then the agent tree (type, description, status, #tools, tokens). Exit non-zero on unreadable file. **This is the headless smoke test** — CI-runnable end-to-end check of parser + session model + info extraction with no TTY.

## Testing (inline #[cfg(test)], no tests/ dir)

Worth testing: transcript line parsing against real-format fixture strings (every entry type incl. flat metadata, polymorphic content, missing is_error, Unknown), sanitization rule, partial-line buffering + truncation reset (tailer state machine over an in-memory/tempfile sequence), session model status transitions (spawn → running → done/failed; the async layer — spawn-ack supersession, `<task-notification>` terminal report, `end_of_stream`; workflow journal completion; pending-tool liveness), graph sync idempotency (same update twice = no duplicate nodes; selection preserved), and the chip reconcile behaviors (aggregation, afterglow, pending reconstruction). Not worth testing: render output, getters.

**Order-independence is guarded by property tests** (the load-bearing invariant — ARCHITECTURE.md §1.1): `live_delivery_converges_to_bulk_ordering` (timeline.rs — 400 random per-file interleavings land the same ts sequence as the bulk sort, nothing left undated) and the model shuffle-invariance test (session.rs — final model state is a pure function of the fact set). ~164 tests, all inline; no `tests/` dir.

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
