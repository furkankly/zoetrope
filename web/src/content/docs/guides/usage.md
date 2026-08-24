---
title: Usage & keys
description: How to launch zoetrope at a live session or a saved transcript, and the full key map for scrubbing, camera, and overlays.
---

The launch only picks the **defaults**: *what* to open and *where the playhead
starts*. Once it's running, scrub / follow / pause / go-live are all available no
matter how you launched.

## Launching (native)

```text
zoe                          follow the current project's live session
zoe <dir>                    follow another project's live session
zoe <file.jsonl>             replay a recording from the start, paced
zoe <file.jsonl> --follow    open a recording at its live edge instead
zoe <file.jsonl> --speed N   playback speed multiplier (default 8.0)
zoe inspect <file.jsonl>     print the session tree and exit (no TUI)
```

A **file** target bulk-loads then tails it; a **directory** (or none → the current
project) discovers the latest session and follows it live. `--follow` only changes
where the playhead starts (the live edge instead of the beginning).

## Launching (browser)

The [browser app](/app) boots into a bundled demo. To watch your own session:

- **Sessions** (Chromium browsers): click **Sessions** to browse your Claude
  projects, pick one, and follow it live. zoetrope reads the main transcript plus its
  subagents, then tails the folder for new activity. This is the same "follow a
  running session" flow as the native app, built on the File System Access API.
  Nothing is uploaded.
- **Sessions** (other browsers): the same button falls back to a folder picker,
  so browsing and replaying work everywhere. **Following live does not** — without
  the File System Access API the browser hands over an immutable *snapshot* of
  each file, so writes that happen after you pick never arrive. The picker says
  so before you choose. Live-follow needs Chrome or Edge (or the native TUI).
- **Drag and drop** a `.jsonl` transcript (any browser). A drop carries only what
  you dropped, and nothing in a transcript points at its sidecar files — so drag
  the `<uuid>.jsonl` **and** its `<uuid>/` folder together to get subagents and
  workflows. Drop the transcript alone and you get the main agent only; zoetrope
  will say so rather than pretending the session had no subagents.

## Keys

| Key | Action |
| --- | --- |
| <kbd>space</kbd> | play / pause (resumes from the playhead) |
| <kbd>[</kbd> / <kbd>]</kbd> | jump to the previous / next prompt era |
| <kbd>End</kbd> / <kbd>g</kbd> | jump to the live edge |
| <kbd>s</kbd> | toggle skip-idle-gaps (compress dead air ↔ real-time) |
| mouse drag | seek along the scrubber |
| <kbd>o</kbd> / <kbd>f</kbd> | camera: Overview / Follow |
| <kbd>r</kbd> | relayout (tidy the graph) |
| arrows / <kbd>Tab</kbd> / <kbd>shift-Tab</kbd> | move between agents |
| <kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd> | pan the graph |
| <kbd>+</kbd> / <kbd>-</kbd> / <kbd>0</kbd> | zoom in / out / reset |
| <kbd>c</kbd> | center on the selected agent |
| click | open an agent's detail panel |
| <kbd>j</kbd> / <kbd>k</kbd> / <kbd>PgUp</kbd> / <kbd>PgDn</kbd> | scroll the detail panel |
| <kbd>i</kbd> | session info overlay |
| <kbd>L</kbd> | cycle the UI language |
| <kbd>?</kbd> | help overlay |
| <kbd>esc</kbd> | close an overlay / clear the selection |
| <kbd>q</kbd> / <kbd>ctrl-c</kbd> | quit (native) |

<kbd>j</kbd> / <kbd>k</kbd> scroll the detail panel when an agent is selected, and
pan the graph otherwise.

### With a mouse

Almost everything above has a key, but the mouse is how most of it feels natural
— and **dragging the scrubber is mouse-only**: it is the one interaction with no
keyboard equivalent (the keys step era-to-era; the drag seeks continuously).
Wheel-zoom also differs from <kbd>+</kbd>/<kbd>-</kbd>: it anchors on the pointer
rather than the viewport centre.

Drag empty canvas to pan · wheel to zoom where you point · click an agent for its
provenance · drag the scrubber to travel through the session. There's a
[recording of all four on the front page](/#).

## Transport states

zoetrope never stores a "mode". The transport badge is *derived* from where the
playhead sits relative to the live edge:

- **Live:** following the edge, with appends arriving right now.
- **Playing:** paced replay moving forward through buffered events.
- **Paused:** paced replay, halted with <kbd>space</kbd>.
- **History:** parked in the past, scrubbed back off the edge.
- **Idle:** at the edge with no fresh activity, such as a finished or quiet session.

## Session info

Press <kbd>i</kbd> for the session overlay: mode, permission mode, queued
operations, file edits, and the last prompt. This data stays off the timeline and
shows only when you ask for it. The same data is available headless via
`zoe inspect <file.jsonl>`.

## UI language

The interface follows your system language by default (the terminal reads
`LANG`/`LC_ALL`, the browser reads `navigator.language`), and can be set
explicitly: pass `--lang zh` at launch (or set `ZOETROPE_LANG`), or press
<kbd>L</kbd> at runtime to cycle languages. English and Simplified Chinese are
currently available; `zoe --help` prints its usage text in the active language
too.
