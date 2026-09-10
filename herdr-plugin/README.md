# herdr-plugin-zoetrope

Open the focused agent's session as a live flow graph, without leaving Herdr.
Works for Claude Code and Codex panes.

## First run

```bash
herdr integration install claude            # and/or: herdr integration install codex
herdr plugin install furkankly/zoetrope/herdr-plugin
herdr plugin action invoke setup-keys --plugin furkankly.zoetrope
```

Then start the agent in a pane, and press `prefix+shift+z` (`prefix` is
`ctrl+b` unless you changed it). The graph opens as an overlay, following the
session live.

To get back to the agent, press `q` in the graph, or `prefix+shift+z` again:
with the graph in front the key closes it. `?` in the graph lists zoetrope's
own keys.

Pressing the key with the graph in front closes it, so one key opens and puts
away.

Where it opens is a preference you make once, so `setup-keys` binds one key and
writes the other two placements into the same block as comments:

| Placement | Bound to | Use it when |
| --- | --- | --- |
| overlay | `prefix+shift+z` | a glance. Takes the tab, restores your focus and zoom on close |
| split | commented | both on screen. Splits the agent pane, so the graph gets half of it |
| tab | commented | keeping it open. Full area, one tab switch from the agent |

To switch, uncomment one in your Herdr config, or point the live binding at
`open-split` or `open-tab`, then `herdr server reload-config`.

The **integration** is what makes any of this work: it adds a `SessionStart`
hook to the agent so Herdr learns the native session id of whatever runs in a
pane. Without it Herdr knows a pane holds Claude Code but not which session,
and the plugin has nothing to open. Agents already running when you install it
keep reporting nothing, so start them again. `herdr integration status` lists
what is installed.

The **plugin install** checks that a `zoe` new enough to open a session by id
is on `PATH`, and installs one with Homebrew, or cargo, when it is missing.
Nothing is vendored: `zoe` is a standalone tool with its own releases, and the
plugin drives the one you have rather than keeping a private copy on a separate
update schedule. An older `zoe` is reported with the upgrade commands, never
replaced. `jq` is needed as well.

The **key step** binds `prefix+shift+z`, next to Herdr's own `prefix+z` for
zoom, and offers `prefix+shift+v` and `prefix+shift+c` as comments beside
`prefix+v` for split and `prefix+c` for tab. It writes one marked block into the Herdr config it finds (the
same lookup Herdr uses: `HERDR_CONFIG_PATH`, then `$XDG_CONFIG_HOME/herdr`,
then `~/.config/herdr/config.toml`), backs the file up first, refuses if
`prefix+shift+z` is already bound, and reloads the running Herdr. The outcome
shows as a Herdr notification and in `herdr plugin log list`. `remove-keys`
deletes the block again. To use another key, skip `setup-keys` and bind the
action yourself:

```toml
[[keys.command]]
key = "prefix+shift+z"
type = "plugin_action"
command = "furkankly.zoetrope.open"
description = "zoetrope: session graph"

[[keys.command]]
key = "prefix+shift+v"
type = "plugin_action"
command = "furkankly.zoetrope.open-split"
description = "zoetrope: session graph (split)"

[[keys.command]]
key = "prefix+shift+c"
type = "plugin_action"
command = "furkankly.zoetrope.open-tab"
description = "zoetrope: session graph (tab)"
```

A key already bound to something else is left alone and named in the result,
so `setup-keys` never takes a key you are using.

Without a key, the actions are still there from the command line:

```bash
herdr plugin action invoke open --plugin furkankly.zoetrope
herdr plugin action invoke open-split --plugin furkankly.zoetrope
herdr plugin action invoke open-tab --plugin furkankly.zoetrope
```

From a checkout:

```bash
herdr plugin link "$(pwd)"    # link runs no build step, so have zoe on PATH
```

## How it works

Herdr plugin panes run an ordinary argv command in a real TTY, so a ratatui
program is a first-class plugin UI. Nothing is embedded into Herdr's render
loop: zoetrope's own TUI is the plugin UI, and the plugin is three shell
scripts.

Three scripts:

1. `herdr/pane.sh` is all three actions, which differ only in the placement
   they pass. It opens the `graph` pane, or closes it when
   the graph is the focused pane, which makes the key a toggle. Herdr labels a
   plugin pane with its manifest title, so a focused pane labelled `zoetrope`
   holding no agent is the plugin's own.
2. `herdr/open.sh` is the pane. It resolves the session and runs
   `zoe --provider <agent> --follow <id>`.
3. `herdr/resolve.sh` asks `pane.get` about the **focused** pane and prints
   `<agent> <session-id>`.

Every failure is printed in the pane and held until you press enter. An action
runs headless, its output goes to `herdr plugin log`, and Herdr notifications
can be turned off, so the pane's own terminal is the only place a message is
certain to be seen.

`pane.get` carries a stored session reference for the agent in the pane:

```json
{ "pane_id": "...", "agent": "codex", "cwd": "...",
  "agent_session": { "source": "herdr:codex", "agent": "codex",
                     "kind": "id", "value": "..." } }
```

Herdr's Claude Code and Codex integrations report the native session id from a
`SessionStart` hook, so `kind` is `"id"` for both, and `zoe <id>` is exactly
what that is for. The plugin passes `--provider` with the agent Herdr named, so
the lookup stays inside that agent's files, and `--follow`, since the pane's
agent is running.

| Herdr reports | What happens |
| --- | --- |
| an id for a `claude` or `codex` pane | `zoe --provider <agent> --follow <id>` |
| an agent but no session id | how to install the integration and why the agent has to be restarted |
| no agent in the pane | a line saying to focus an agent pane |
| an agent zoetrope does not read | the same, naming the agent |

There is no fallback through the working directory. Herdr knows the session,
so the plugin uses it or says what it got instead.

## Keys, once the graph is up

Herdr sees the prefix chord first and everything else goes to the pane, in
every placement. So zoetrope keeps its own keys (`q`, `?`, `space`, arrows,
`hjkl`, `+`/`-`, `[`/`]`, `g`/`G`) and Herdr keeps `prefix+...`. Herdr's
`hjkl` pane moves live inside navigate mode, which the prefix opens, so they
never shadow the graph's own. The one way to collide is a custom
`[[keys.command]]` bound to a bare chord rather than a prefix chord: Herdr
grabs those globally, before any pane.

## Versions

The plugin has no release of its own. `herdr plugin install` clones the repo at
its default branch, so what people get is what is on main, and `herdr plugin
uninstall` then `install` is how it updates.

Three numbers, three reasons to change:

| Number | Where | Bumped when |
| --- | --- | --- |
| `version` | `herdr-plugin.toml` | the files in this directory change. Displayed, never resolved |
| `min_herdr_version` | `herdr-plugin.toml` | a script starts using a newer Herdr API. The only one that can block an install |
| `ZOE_SINCE` | `herdr/ensure-zoe.sh` | the command line the scripts call changes. A label for the error message, not a check |

None of them follows a zoetrope release on its own, and a `zoe` release that
leaves the command line alone changes nothing here. Zoe's own improvements
reach people through Homebrew and crates.io without the bridge moving at all.

The binary is gated on what it can do rather than on what it calls itself: the
build step asks `zoe --help` whether it takes `--provider`. A version string is
a label, and a binary built from a checkout carries the crate version of its
base release, so a floor would refuse a `zoe` that has the flag, while a future
release that renamed the flag would pass a floor and fail at the first key
press.

One rule ties the two release channels together: a script here may only call a
`zoe` command line that is already published, since installs take the branch
while the binary comes from a release. Adding a call to something unreleased
would break every fresh install until the crate ships.

## Provenance of the field names

Taken from `herdr api schema --json` on herdr 0.8.2 (protocol 20), the plugin
and socket docs at v0.9.0, and live responses, not from memory:

- `PaneInfo`: `pane_id` (not `id`), `agent`, `agent_session`, `agent_status`,
  `cwd`, `foreground_cwd`, `display_agent`, `focused`, `tokens`, `workspace_id`
- `pane.get` nests that record one level down, as
  `{"result": {"pane": {...}, "type": "pane_info"}}`; `pane.list` answers
  `.result.panes`
- In a **pane** command, `HERDR_PANE_ID` is the plugin's own new pane, so
  asking Herdr about it returns a pane with no agent. `focused_pane_id` in
  `HERDR_PLUGIN_CONTEXT_JSON` is the pane the plugin was invoked from, in both
  action and pane commands, and is the one to use
- `AgentSessionInfo`: `{source, agent, kind, value}`, all required; the whole
  object is absent until an integration reports a session
- `AgentSessionRefKind`: `"id" | "path"`; `src/agent_resume.rs` in Herdr maps
  `herdr:claude` and `herdr:codex` to `Id` (`pi` and `omp` store a path, and
  zoetrope does not read those agents)
- Manifest: action `contexts` are `global`, `workspace`, `tab`, `pane`,
  `selection` (stored, not rendered: 0.8.2 has no action menu); pane
  `placement` is one of `overlay`, `popup`, `split`, `tab`, `zoomed`;
  keybindings are not a manifest field, hence `setup-keys`
- Every CLI response is `{id, result}` or `{id, error}`
