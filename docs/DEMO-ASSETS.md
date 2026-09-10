# Demo assets — how the GIF and screenshots are made

Every visual in the README and on the landing page is **reproducible from the
repo**. Nothing here is a one-off screen capture, and that is deliberate: the
same fixture is both the marketing asset and the end-to-end smoke test, so the
two cannot silently drift.

## The tool: VHS

[charmbracelet/vhs](https://github.com/charmbracelet/vhs) — installed at
`/opt/homebrew/bin/vhs` (v0.11.0 as of writing; `brew install vhs`).

VHS records a terminal session from a declarative script (a "tape"), so the
recording is a build artifact rather than something a human performed once.
It also takes keystrokes as data, which is what lets the demo *show* the detail
panel opening and the timeline scrubbing instead of just replaying a graph.

`TODO.md` originally proposed "VHS/asciinema" for this. VHS won on the
keystroke-choreography point.

## The four recordings

Each answers a different question, and the landing page shows them in this
order with a caption under each:

| GIF | Tape | Answers |
| --- | --- | --- |
| `zoetrope-demo.gif` | `assets/demo.tape` | *What is this?* — the whole tree in Overview as the session builds |
| `zoetrope-follow.gif` | `assets/follow.tape` | *What does watching a live run feel like?* — `f` hands the camera to the action |
| `zoetrope-tour.gif` | `assets/tour.tape` | *What is it like to use?* — pan, zoom, inspect, scrub, driven by a pointer |
| `zoetrope-codex.gif` | `assets/codex.tape` | *Does it do Codex?* The CLI capture (`assets/codex/cli-0.153.4/`), a subagent's panel, the info overlay |

One more sits beside them, made a different way:

| GIF | Made by | Answers |
| --- | --- | --- |
| `zoetrope-herdr.gif` | hand capture | *What is it like inside Herdr?* A key press in an agent pane, and that pane's session opens as a graph over it |

VHS cannot record it. The plugin runs inside a live multiplexer with an agent
working in the pane next door, which is a session no tape can script. So it is
captured by hand, replaced by hand, and listed in `HAND` in `assets/build.sh`,
which exempts it from the orphan and MP4 checks. The README links it directly;
the landing page does not use it.

Everything goes through one script, so there is one list of recordings rather
than one per tool. Adding a recording is a line in `DEMOS` inside it plus a
matching tape.

```bash
./assets/build.sh all       # tapes + mp4 + social card + copies into web/public
./assets/build.sh check     # verify without changing anything
```

`check` is the one to run before publishing. It catches a missing MP4, an MP4
older than its GIF, a `web/public` copy that drifted from its source, any file
in `assets/` that no tape produces, and a tape that sleeps for less time than
the pointer script runs.

That last one used to be a comment in `tour.tape` saying the sleep "MUST equal
the script's total (≈10.8s)", which nothing enforced — and a short sleep cuts
the recording mid-gesture. The length now lives only in the `Step` list, and the
binary reports it:

```bash
ZOETROPE_DEMO=duration target/release/zoe   # -> 10.80
```

`tour_steps` is separated from the `TourMarks` it aims at so this can be
answered without an app, a layout or a terminal: the marks decide where the
pointer goes, never how long it takes.

The copies are not optional — Astro's image pipeline (sharp) **flattens
animation to a single frame**, so nothing here can go through `<Image>`; it is
served from `public/` by hand. Keep them in sync; check with:

```bash
md5 -q assets/zoetrope-demo.gif web/public/zoetrope-demo.gif | uniq | wc -l   # want 1
```

## MP4 for the site, GIF for the README

The landing page was serving **5.5 MB of GIF** in three `<img>`s. The same three
as H.264 are **1.7 MB**, which is most of the page's weight for no visible loss:

| | GIF | MP4 |
| --- | --- | --- |
| `zoetrope-demo` | 1.4 MB | 580 KB |
| `zoetrope-follow` | 2.6 MB | 676 KB |
| `zoetrope-tour` | 1.4 MB | 440 KB |

The GIFs stay because the README needs them. GitHub strips `<video>` from
markdown; the auto-embedded player some projects use (a bare
`user-attachments` URL on its own line) needs a file uploaded through the web
UI, which no script can regenerate. A page has no such limit, so the page gets
video and the repo keeps GIFs.

On the page, `autoplay muted loop playsinline` are all load-bearing: mobile
Safari refuses to autoplay anything unmuted, and without `playsinline` it takes
the video fullscreen instead of playing it in place. A `<video>` has no `alt`,
so the accessible name is given with `aria-label`. The `<img>` nested inside is
reached only where `<video>` itself is unsupported — which is why the GIF copies
remain in `web/public`. If that fallback is ever judged not worth its deploy
size, delete both the copies and the nested `<img>`, not one of them.

**These recordings do not have the chroma problem** that rataflow's hero had.
VHS renders frames to RGB and writes the GIF directly, so nothing is ever
subsampled. That failure mode belongs to screen recorders: `screencapture -v`
writes `yuv420p`, which turns one-pixel coloured lines grey and inflated
rataflow's hero from under a megabyte to 3 MB. Worth knowing if a tape is ever
replaced by a real screen capture here.

### What the tape records

It replays the bundled fixture (`assets/claude/demo.jsonl` + `assets/claude/demo/subagents/`)
— the *same session* the browser frontend at `/app` boots into, so the GIF and the
browser demo show the same thing. `--speed 14` compresses the fixture's ~200s of
content into ~15s, then the keystrokes demonstrate:

| Keys | Shows |
| --- | --- |
| `Down`, `Right` | spatial-nav selects an agent → detail panel opens (spawning prompt, reasoning, tool list) |
| `i` / `Escape` | session-info overlay (title, mode, permission, queued, file edits) |
| `[` | step back one prompt era |
| `g` | snap back to the live edge |

Current output: 1728×944, 24 fps — demo ~1.4 MB / 30s, follow ~2.6 MB / 18s,
tour ~1.4 MB / 11s.

## VHS gotchas

Two that cost real time, both worth remembering:

1. **`Output` is mandatory, even for a still.** VHS will not run a tape without
   it, so asking only for a `Screenshot` still writes a GIF you did not want.
   (This is where the stray 794 KB `shot.gif` in the repo root came from.)
2. **The path parser is fussy.** It rejected an absolute path containing many
   dashes and digits (`/private/tmp/claude-501/-Users-furkan-…/shot.png`) with
   `Invalid command`. Use short relative paths and move the output afterwards.

## The second GIF: the interaction tour

`assets/zoetrope-tour.gif` (tape: [`assets/tour.tape`](../assets/tour.tape))
shows the app being *driven*: panning, wheel-zoom, opening an agent's panel, and
dragging the scrubber to time-travel. It is embedded in the Usage guide next to
the key table.

### The autopilot

VHS is keyboard-only and films a headless terminal with no OS pointer, so a
mouse-driven recording is impossible to capture directly. `src/autopilot.rs`
closes that from the other side: the app draws its own pointer and runs a script
of waypoints, gated behind `ZOETROPE_DEMO=1`. (Ported from the same idea in
`rataflow/examples/shared/src/autopilot.rs`.)

Two rules make it worth having:

- **Synthesize real events, don't animate state.** The script emits
  `Event::Mouse`/`Event::Key` through `handler::handle_event` — the same entry a
  terminal drives — so hit testing, the drag threshold and the scrubber-row
  intercept all run. Moving the camera or the playhead directly would look
  similar on screen and prove nothing.
- **Keep dwells short.** Pauses before a click and after a drop are what read as
  a hand; long ones read as a screensaver.

**Waypoints are found, not hardcoded.** `tour()` reads them off the live layout
via `node_terminal_rect` / `canvas_size` / `App::scrubber_area`, so the script
survives a relayout or a resize. Hardcoded cells failed twice here: first at the
wrong grid size (the terminal is 189×58 at this geometry, not the ~205×45 that
seemed right), then by pressing one cell off a card — which still hits the card,
so the "pan" beat dragged a node instead.

Pick targets by **meaning**, too. Choosing the click target by rect size picked
the workflow group: widest card on screen, but it has no prompt, no thought and
no tools, so the panel opened on three empty sections. It now picks the subagent
with the most tool calls, which is how the panel ends up showing a failure and
its retry.

The pilot is armed by `ZOETROPE_DEMO=1` but **fired by pressing `t`**, so the
tape decides the moment — that lets `tour.tape` hide the replay entirely and open
on the finished graph. Both the `select!` arm and the post-select drain in
`tui::run` route through one `route()` fn; when the trigger check lived in only
one of them, whether the demo fired depended on which path the keypress happened
to arrive by.

Keep each tape's trailing `Sleep` equal to its script's total duration (sum the
steps). rataflow's tape learned this the hard way: spare seconds at the end are a
still frame, which is the worst thing in a looping recording.

### What actually needs the mouse

Less than you would guess, and worth stating plainly so the tour is not oversold:

| Interaction | Keyboard? |
| --- | --- |
| Pan | ✅ <kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd> |
| Zoom | ✅ <kbd>+</kbd> <kbd>-</kbd> <kbd>0</kbd> — but viewport-centred, where the wheel anchors on the pointer |
| Select an agent / open its panel | ✅ arrows, <kbd>Tab</kbd> |
| **Seek by dragging the scrubber** | ❌ **mouse only** — keys step era-to-era (`[` `]`), the drag seeks continuously |

So the tour earns its place by showing the app being *used*, and by covering the
one genuinely mouse-exclusive interaction — not because the rest is unshowable.

## Still frames

- **Terminal stills** — VHS's `Screenshot <path>.png` directive, inserted into a
  throwaway copy of the tape at the moment you want captured. Handy for
  verifying choreography without eyeballing a GIF frame by frame.
- **Browser stills** — Playwright, against `astro preview` (**not** `astro dev`:
  the dev server injects its floating dev-toolbar island into the frame).

## Why the fixture is synthetic

`assets/claude/demo.jsonl` is a hand-authored session, not a real transcript. That is a
choice, not a shortcut:

- **Reproducible** — anyone can regenerate the GIF byte-comparably. A real
  session is a one-shot capture nobody can reproduce, including you later.
- **No redaction surface** — no real paths, prompts, or repo contents get
  published.
- **Curated** — one clean fan-out to four visibly distinct agent types plus a
  workflow group. Real sessions are lopsided and demo worse.
- **It is also a test** — `demo_session_conforms` (`src/provider/claude/mod.rs`)
  discovers it the way a live session is discovered and checks order invariance
  plus the two goldens beside it, `demo.model.txt` and `demo.timeline.txt`
  (regenerate with `UPDATE_GOLDEN=1`). `zoe inspect assets/claude/demo.jsonl`
  prints the same model view by hand. It is what caught the
  `assets/subagents/` path bug and the wasm workflow gap.

It deliberately exercises the features that were otherwise dark: a failed tool
call with a retry, a workflow group with a journal, two prompt eras, and the
session metadata behind the `i` overlay. Where real data has no better value —
workflow subagents genuinely carry only `{"agentType":"workflow-subagent"}` —
the fixture matches reality rather than inventing nicer labels.

## The Codex fixtures are real captures

`assets/codex/` is the other kind of fixture: three real sessions, as Codex
wrote them, each a directory shaped like `~/.codex/sessions` so discovery
finds it the way it finds a live one:

| Fixture | Frontend, version | Shape |
| --- | --- | --- |
| `cli-0.153.4` | Codex CLI 0.153.4, run as `codex exec` (the same binary as the interactive TUI; the file says `codex_exec`) | root + 4 subagents on the dark-mode brief the Claude demo uses; the only capture with `token_usage_record`. **The demo-grade one.** |
| `cli-0.149.1` | Codex CLI 0.149.1 (`codex-tui`) | root + 3 subagents; no `SubAgentActivity{completed}` anywhere, so the children end by time-derived liveness. Kept as evidence for that fallback. |
| `desktop-0.150.0` | Codex Desktop 0.150.0-alpha.8 | root + 6 subagents, the dark-mode-toggle scenario; paired `started`/`completed`; web searches and file patches both |

Real, not synthetic, on purpose: the format is undocumented and shifts between
versions, and the three fixtures are the evidence for every claim the provider
makes (`src/provider/codex/`). They are trimmed of what the provider never reads
and anonymized, never restructured. Only `cli-0.153.4` doubles as a demo asset
(`codex.tape` records it); the other two are test evidence and nothing else.

`captures_conform` (`src/provider/codex/mod.rs`) runs every directory under
`assets/codex/` through the same conformance check as the Claude demo, with
goldens beside each fixture. Adding a capture is a directory and
`UPDATE_GOLDEN=1`.

