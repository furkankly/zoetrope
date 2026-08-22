<p align="center">
  <img src="https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/icon.svg" alt="" width="80">
</p>

<h1 align="center">zoetrope</h1>

<p align="center">
  <em>Watch a Claude Code session as a live flow graph, in your terminal or your browser.</em>
</p>

<p align="center">
  <a href="https://crates.io/crates/zoetrope"><img src="https://img.shields.io/crates/v/zoetrope.svg?style=flat&labelColor=121212&color=d7af00&logo=Rust&logoColor=white" alt="crates.io"></a>
  <a href="https://docs.rs/zoetrope"><img src="https://img.shields.io/docsrs/zoetrope?style=flat&labelColor=121212&color=d7af00&logo=docs.rs&logoColor=white" alt="docs.rs"></a>
  <a href="https://crates.io/crates/zoetrope"><img src="https://img.shields.io/crates/d/zoetrope.svg?style=flat&labelColor=121212&color=d7af00" alt="downloads"></a>
  <a href="https://github.com/furkankly/zoetrope/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/furkankly/zoetrope/ci.yml?branch=main&style=flat&labelColor=121212&color=d7af00&logo=GitHub%20Actions&logoColor=white" alt="build status"></a>
  <a href="https://crates.io/crates/zoetrope"><img src="https://img.shields.io/crates/msrv/zoetrope?style=flat&labelColor=121212&color=d7af00&label=MSRV" alt="minimum supported Rust version"></a>
</p>

<p align="center">
  <a href="https://zoetrope.furkankly.dev"><b>zoetrope.furkankly.dev</b></a> · the whole app in your browser, the same binary compiled to WASM
</p>

<p align="center">
  <img src="https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope.svg" alt="A session drawn as a flow graph: a main agent above the subagents it spawned, over a timeline of tool activity" width="620">
</p>

Claude Code writes a JSONL transcript for every session under `~/.claude/projects/`.
zoetrope reads it and draws the session as a graph in your terminal: the main agent, the subagents and
workflows it spawns, and the tools each one runs, updating live as it goes. Point it
at a finished run and it replays, paced by the session's own timestamps. Point it at a
running one and it follows along. It's read-only, and nothing leaves your machine.

Built on [ratatui](https://ratatui.rs) and [rataflow](https://github.com/furkankly/rataflow).

![zoetrope replaying a Claude Code session as a flow graph](https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope-demo.gif)

## Installation

**Homebrew** — macOS and Linux:

```bash
brew install furkankly/tap/zoetrope
```

**Cargo** — needs a Rust toolchain:

```bash
cargo install zoetrope
```

**Prebuilt binaries** — no toolchain needed. Every
[release](https://github.com/furkankly/zoetrope/releases) carries archives for
macOS (Apple Silicon and Intel), Linux (`musl`, arm64 and x86_64) and Windows
(x86_64). Unpack one and put `zoe` on your `PATH`.

Whichever route you take, the command is `zoe`. Or build from source:

```bash
git clone https://github.com/furkankly/zoetrope
cd zoetrope
cargo build --release
./target/release/zoe
```

No install at all: **[try it in your browser](https://zoetrope.furkankly.dev/app)**.
Drop a transcript on the page and get the same graph.

## Usage

```bash
zoe                          # follow the current project's live session
zoe <dir>                    # follow another project's session
zoe <file.jsonl>             # replay a recording from the start
zoe <file.jsonl> --follow    # open a recording at its live edge
zoe <file.jsonl> --speed N   # playback speed (default 8.0)
zoe inspect <file.jsonl>     # print the session tree and exit (no TUI)
```

Give it a file and it reads the whole transcript, then keeps watching for new lines.
Give it a directory, or no argument at all, and it finds the newest session in that
project and follows it live. Whichever way you start, the controls are the same:
scrub, follow, pause, jump back to live.

The same engine also runs [in the browser](https://zoetrope.furkankly.dev/app),
compiled to WebAssembly via [ratzilla](https://github.com/ratatui/ratzilla). Open a
session from disk, or drop a transcript on the page. It stays local there too.

## Features

**The graph**
- A node per agent: the main session, its subagents, and workflow groups with their
  children nested underneath
- Status, current tool, tool count and output tokens on every card
- Edges animate while an agent is working, and settle when it finishes
- Tool calls surface as chips beneath their agent (`⚒ bash ×5`, or `⚒ bash 0.5s`
  ticking during a single call), resolving to `✓` or `✗`
- A minimap showing where your viewport sits once the graph outgrows the screen

**Time travel**
- One scrubbable timeline over both live and replayed sessions
- Indexed by event rather than wall-clock, so a busy minute gets room instead of
  collapsing into a sliver
- Scrub, pause, step between prompt eras, or snap back to the live edge
- Seek backwards and you see the session exactly as it stood at that moment. Agents
  un-finish, tool counts fall, the graph shrinks back
- Optional gap compression, to skip dead air or keep faithful real-time pacing

**Inspection**
- Click any agent for its provenance: the prompt that spawned it, the reasoning
  around it, its model, and every tool call it made with timings
- Session info overlay: mode, permissions, queued ops, file edits, last prompt
- `zoe inspect` prints the whole tree headlessly, so it runs anywhere without a TTY

**Reading your sessions**
- Follows a running session live, or replays a finished one
- Reads everything a session writes: the main transcript, its subagents, and
  workflows with their own children, so the graph is the whole picture
- Keeps going when Claude Code writes something it hasn't seen: unfamiliar records
  are skipped, never fatal
- Read-only, and no network at all (see below)

## Keys

`space` play/pause · `[` `]` prev/next prompt · `End` or `g` jump to live · drag the
bar to seek · `?` for everything else.

<details>
<summary>Full keymap</summary>

| Key | Action |
| --- | --- |
| `space` | play / pause (resumes from the playhead) |
| `[` / `]` | previous / next prompt era |
| `End` / `g` | jump to the live edge |
| `s` | toggle gap compression (faithful pacing vs. skip idle stretches) |
| mouse drag | seek along the scrubber |
| `o` / `f` | camera: Overview / Follow |
| `r` | relayout (tidy the graph) |
| arrows / `Tab` / `Shift-Tab` | move between agents |
| `h` `j` `k` `l` | pan the graph |
| `+` / `-` / `0` | zoom in / out / reset |
| `c` | center on the selected agent |
| click | open an agent's detail panel |
| `j` / `k` / `PgUp` / `PgDn` | scroll the detail panel |
| `i` | session info overlay |
| `?` | help overlay |
| `esc` | close an overlay / clear the selection |
| `q` / `ctrl-c` | quit |

`j` / `k` scroll the detail panel when an agent is selected, otherwise they pan the graph.

</details>

Hand the camera to the action with `f` and it glides to whichever agent just did
something. This is what watching a live run looks like:

![zoetrope in follow mode, the camera tracking whichever agent is working](https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope-follow.gif)

Or drive it yourself: pan, zoom where you point, open an agent's panel, and drag the
scrubber to travel back through the session.

![Panning, zooming, opening a detail panel, and dragging the scrubber](https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope-tour.gif)

## Under the Hood

zoetrope treats the transcript as an **append-only event log**. It tails the files,
parses each line defensively, and folds them into a derived model of agents, tool
calls, and prompts. Nothing is mutated in place. The model is a pure function of the
facts seen so far, so seeking backwards is exact.

A few of the pieces that turn a log into a watchable session:

- **Two clocks, kept apart.** Content time comes from the transcript's own timestamps;
  presentation time is the playhead you control. Every pacing decision (speed, gap
  compression, scrubbing) touches only the second one, so no display choice can ever
  alter what the session says happened.
- **One timeline for live and replay.** Behind the live edge it paces forward; at the
  edge it pins and folds in new appends as they land. A live session and a saved
  recording differ only in where the playhead starts. Same engine, same controls.
- **Ground truth over heuristics.** Parentage, completion and naming come from what the
  format actually records (a subagent's `toolUseId`, a workflow's `runId`, a journal
  `result`) rather than from guessing at strings or timing.
- **You own the camera, not the history.** The recording is immutable and the graph
  never rearranges itself under you (`r` to relayout). The camera follows the action
  until you touch it, then stays where you put it.
- **Zero network, provably.** There is no HTTP client anywhere in the dependency tree,
  and `tokio` is pulled in without its `net` feature. `cargo tree` is the proof. This is
  a property you can check, not a promise.

The model, the timeline and the rendering all live in the **portable core**, a
library with no IO that compiles for any target, including WebAssembly. The native
frontend (the `zoe` binary in this
crate) and the browser frontend (the `zoetrope-web` crate under `web/wasm/`, which
runs the hosted app) sit on top of it and differ only in their IO and event loop.

See [`docs/DESIGN.md`](https://github.com/furkankly/zoetrope/blob/main/docs/DESIGN.md) for the module map and the transcript format,
and [`docs/ARCHITECTURE.md`](https://github.com/furkankly/zoetrope/blob/main/docs/ARCHITECTURE.md) for the invariants above in full.

## A note on the transcript format

The JSONL format zoetrope reads is undocumented and internal to Claude Code, so it can
change without warning. zoetrope is built to degrade rather than break: unrecognized
records are skipped, missing fields fall back, and a malformed line never takes down
the session. If a new Claude Code release makes something render oddly, please
[open an issue](https://github.com/furkankly/zoetrope/issues).

## Contributing

Pull requests are welcome.

- This project follows [Conventional Commits](https://www.conventionalcommits.org/) for all commit messages (e.g. `feat(timeline): index the playhead by event instead of wall-clock`, `fix(tailer): fold appends at the live edge without rebuilding`). The changelog is generated from them with [git-cliff](https://github.com/orhun/git-cliff), and non-conforming commits are dropped.
- Run `cargo fmt`, `cargo clippy` and `cargo test` before opening a PR.
- Those cover the portable core and the native frontend. The browser frontend is a
  second crate (`zoetrope-web`, in `web/wasm/`) that only builds for wasm32, so it is
  excluded from the root workspace and no root `cargo` command touches it. From the
  repo root, build it with `bash web/scripts/build-wasm.sh` and lint it with
  `cd web/wasm && cargo clippy` (its `.cargo/config.toml` defaults the target to wasm32).

## License

[MIT](https://github.com/furkankly/zoetrope/blob/main/LICENSE).

## Acknowledgements

- [ratatui](https://github.com/ratatui/ratatui) for the terminal UI framework
- [rataflow](https://github.com/furkankly/rataflow) for the node-graph widget
- [ratzilla](https://github.com/ratatui/ratzilla) for the WebAssembly backend
