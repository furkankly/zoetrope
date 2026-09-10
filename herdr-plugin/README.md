# zoetrope in Herdr

[Herdr](https://herdr.dev) runs coding agents in panes and knows which session
each pane is running. This plugin asks it, then opens that session in `zoe` as
a live flow graph, without you naming a file or an id. Claude Code and Codex
panes both work.

## Installation

```bash
herdr integration install claude          # and/or: herdr integration install codex
herdr plugin install furkankly/zoetrope/herdr-plugin
herdr plugin action invoke setup-keys --plugin furkankly.zoetrope
```

**The integration** is what makes the rest possible. It adds a `SessionStart`
hook to the agent, so Herdr learns the native session id of whatever runs in a
pane. Without it Herdr knows a pane holds Claude Code but not which session,
and the plugin has nothing to open. An agent already running when you install
it never reports one, so start it again. `herdr integration status` lists what
is installed.

**The plugin install** checks that a `zoe` new enough to open a session by id
is on `PATH`, and installs one with Homebrew or cargo when it is missing. An
older `zoe` is reported with the upgrade commands, never replaced. Nothing is
vendored: `zoe` is a standalone tool with its own releases, and the plugin
drives the one you have. `jq` is needed as well.

**The key step** writes one marked block into the Herdr config, since plugins
cannot ship keys in their manifest. It finds the file the way Herdr does
(`HERDR_CONFIG_PATH`, then `$XDG_CONFIG_HOME/herdr`, then
`~/.config/herdr/config.toml`), backs it up, leaves a key that is already bound
alone and names it, validates the result and rolls back if it does not parse,
then reloads the running Herdr. `remove-keys` deletes the block again.

From a checkout instead:

```bash
herdr plugin link "$(pwd)"    # link runs no build step, so have zoe on PATH
```

## Usage

Focus an agent pane and press `prefix+shift+z`, where `prefix` is `ctrl+b`
unless you changed it. The graph opens over the pane and follows the session
live. Press `q` in the graph to close it, or the same key again, so one key
both opens and puts away.

Where it opens is a preference you make once, so `setup-keys` binds one key and
writes the other two placements into the same block as comments:

| Placement | Bound to | Use it when |
| --- | --- | --- |
| overlay | `prefix+shift+z` | a glance. Takes the tab, restores your focus and zoom on close |
| split | commented | both on screen. Splits the agent pane, so the graph gets half of it |
| tab | commented | keeping it open. Full area, one tab switch from the agent |

To switch, uncomment one in your Herdr config, or point the live binding at
`open-split` or `open-tab`, then run `herdr server reload-config`. To bind your
own keys instead, skip `setup-keys` and write them yourself:

```toml
[[keys.command]]
key = "prefix+shift+z"
type = "plugin_action"
command = "furkankly.zoetrope.open"
description = "zoetrope: session graph (overlay)"
```

The actions also run from the command line, with or without a key:

```bash
herdr plugin action invoke open --plugin furkankly.zoetrope
herdr plugin action invoke open-split --plugin furkankly.zoetrope
herdr plugin action invoke open-tab --plugin furkankly.zoetrope
```

## Keys inside the graph

Herdr sees the prefix chord first and everything else reaches the pane, in
every placement. So zoetrope keeps its own keys (`q`, `?`, `space`, arrows,
`hjkl`, `+` and `-`, `[` and `]`, `g` and `G`) and Herdr keeps `prefix+...`.
Herdr's own `hjkl` pane moves live inside navigate mode, which the prefix
opens, so they never shadow the graph's. The one way to collide is a custom
`[[keys.command]]` bound to a bare chord rather than a prefix chord, since
Herdr grabs those before any pane sees them.

## How it works

Herdr plugin panes run an ordinary argv command in a real TTY, so a ratatui
program is a first-class plugin UI. Nothing is embedded into Herdr's render
loop: zoetrope's own TUI is the plugin UI, and the plugin is three scripts.

1. `herdr/pane.sh` is all three actions, which differ only in the placement
   they pass. It opens the graph pane, or closes it when the graph is the
   focused pane, which is what makes the key a toggle. Herdr labels a plugin
   pane with its manifest title, so a focused pane labelled `zoetrope` holding
   no agent is the plugin's own.
2. `herdr/open.sh` is the pane. It resolves the session and runs
   `zoe --provider <agent> --follow <id>`.
3. `herdr/resolve.sh` asks `pane.get` about the focused pane and prints
   `<agent> <session-id>`.

Every failure is printed in the pane and held until you press enter. An action
runs headless with its output going to `herdr plugin log`, and Herdr
notifications can be switched off, so the pane's own terminal is the only place
a message is certain to be seen.

`pane.get` carries a stored session reference for the agent in the pane:

```json
{ "pane_id": "...", "agent": "codex", "cwd": "...",
  "agent_session": { "source": "herdr:codex", "agent": "codex",
                     "kind": "id", "value": "..." } }
```

Herdr's Claude Code and Codex integrations both report the native session id,
so `kind` is `"id"` for either, and `zoe <id>` is exactly what that is for. The
plugin passes `--provider` with the agent Herdr named, so the lookup stays
inside that agent's files, and `--follow`, since the pane's agent is running.

| Herdr reports | What happens |
| --- | --- |
| an id for a `claude` or `codex` pane | `zoe --provider <agent> --follow <id>` |
| an agent but no session id | how to install the integration, and why the agent has to be restarted |
| no agent in the pane | a line saying to focus an agent pane |
| an agent zoetrope does not read | the same, naming the agent |

There is no fallback through the working directory. Herdr knows the session, so
the plugin uses it or says what it got instead.

## Versions

The plugin has no release of its own. `herdr plugin install` clones the repo at
its default branch, so what people get is what is on main, and reinstalling is
how it updates. Three numbers, three reasons to change:

| Number | Where | Bumped when |
| --- | --- | --- |
| `version` | `herdr-plugin.toml` | the files in this directory change. Displayed, never resolved |
| `min_herdr_version` | `herdr-plugin.toml` | a script starts using a newer Herdr API. The only one that can block an install |
| `ZOE_SINCE` | `herdr/ensure-zoe.sh` | the command line the scripts call changes. A label for the error message, not a check |

None of them follows a zoetrope release. Zoe's own improvements reach people
through Homebrew and crates.io without the bridge moving at all, and the bridge
only has to move when it starts calling something new.

`zoe` is gated on what it accepts rather than on what it calls itself: the
build step asks `zoe --help` whether it takes `--provider`. A version string is
a label, and a binary built from a checkout carries the crate version of its
base release, so a floor would refuse a `zoe` that has the flag, while a future
release that renamed the flag would pass a floor and then fail at the first key
press.

One rule ties the two release channels together. A script here may only call a
`zoe` command line that is already published, because installs take the branch
while the binary comes from a release. Calling something unreleased would break
every fresh install until the crate ships.

## Notes on Herdr's API

Read from `herdr api schema --json` on Herdr 0.8.2 (protocol 20), the plugin
and socket docs at v0.9.0, and live responses:

- `PaneInfo` is `pane_id` (not `id`), `agent`, `agent_session`, `agent_status`,
  `cwd`, `foreground_cwd`, `display_agent`, `focused`, `tokens`, `workspace_id`
- `pane.get` nests that record one level down, as
  `{"result": {"pane": {...}, "type": "pane_info"}}`, and `pane.list` answers
  `.result.panes`
- In a pane command, `HERDR_PANE_ID` is the plugin's own new pane, so asking
  Herdr about it returns a pane with no agent. `focused_pane_id` in
  `HERDR_PLUGIN_CONTEXT_JSON` is the pane the plugin was invoked from, in both
  action and pane commands, and is the one to use
- `AgentSessionInfo` is `{source, agent, kind, value}`, all required, and the
  whole object is absent until an integration reports a session
- `AgentSessionRefKind` is `"id"` or `"path"`. Herdr maps `herdr:claude` and
  `herdr:codex` to an id; `pi` and `omp` store a path, and zoetrope does not
  read those agents
- Action `contexts` are `global`, `workspace`, `tab`, `pane` and `selection`,
  stored but not rendered, since 0.8.2 has no action menu. Pane `placement` is
  one of `overlay`, `popup`, `split`, `tab`, `zoomed`. Keybindings are not a
  manifest field, which is why `setup-keys` exists
- Every CLI response is `{id, result}` or `{id, error}`
