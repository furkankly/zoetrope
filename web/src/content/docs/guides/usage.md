---
title: Usage & keys
description: How to launch zoetrope at a live Claude Code or Codex session or a saved transcript, and the full key map for scrubbing, camera, and overlays.
---

The launch only picks the **defaults**: *what* to open and *where the playhead
starts*. Once it's running, scrub / follow / pause / go-live are all available no
matter how you launched.

## Launching (native)

```text
zoe                          follow the current project's live session
zoe <dir>                    follow another project's live session
zoe <file.jsonl>             replay a recording from the start, paced (any file of a session)
zoe <id>                     replay a session by id, or a unique prefix of one
zoe <file.jsonl> --follow    open a recording at its live edge instead
zoe <file.jsonl> --speed N   playback speed multiplier (default 8.0)
zoe --provider codex ...     force the format instead of detecting it from the file
zoe inspect <file|id>        print the session tree and exit (no TUI)
```

A **file** target bulk-loads then tails it; a **directory** (or none → the current
project) discovers the latest session and follows it live. `--follow` only changes
where the playhead starts (the live edge instead of the beginning).

## Launching (browser)

The [browser app](/app) boots into a bundled demo. To watch your own session:

- **Sessions** (Chromium browsers): click **Sessions**, point it at
  `~/.claude/projects` (Claude Code) or `~/.codex/sessions` (Codex), pick a
  session, and follow it live. zoetrope reads every file of the session, then
  tails the folder for new activity, including agents spawned after you picked.
  This is the same "follow a running session" flow as the native app, built on
  the File System Access API. Nothing is uploaded.
- **Sessions** (other browsers): the same button falls back to a folder picker,
  so browsing and replaying work everywhere. **Following live does not** — without
  the File System Access API the browser hands over an immutable *snapshot* of
  each file, so writes that happen after you pick never arrive. The picker says
  so before you choose. Live-follow needs Chrome or Edge (or the native TUI).
- **Drag and drop** a transcript (any browser). A drop carries only what you
  dropped, and nothing in a transcript points at the session's other files, so
  drag a Claude Code `<uuid>.jsonl` **and** its `<uuid>/` folder together to get
  subagents and workflows, or a Codex root `rollout-*.jsonl` together with its
  subagents' rollouts. Drop the root alone and you get the main agent only;
  zoetrope will say so rather than pretending the session had no subagents.

## Launching (inside Herdr)

[Herdr](https://herdr.dev) runs coding agents in panes and knows which session
each one is. The plugin in `herdr-plugin/` asks it about the focused pane and
launches `zoe` on that session, so nothing has to be discovered or named:

```bash
herdr integration install claude          # and/or codex, so Herdr learns session ids
herdr plugin install furkankly/zoetrope/herdr-plugin
herdr plugin action invoke setup-keys --plugin furkankly.zoetrope
```

Then `prefix+shift+z` in an agent pane opens the graph over it, following live,
and closes it again. The session id comes from the agent's `SessionStart` hook,
which is what `herdr integration install` adds, so a session that was already
running when you installed it has to be started again before Herdr can name it.

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
`zoe inspect <file|id>`.
