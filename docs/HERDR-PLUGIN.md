# The Herdr plugin

A bridge, not a second frontend. [Herdr](https://herdr.dev) already knows which
agent occupies a pane and the native id of the session running there, which is
the pair `zoe` would otherwise have to infer. The plugin asks for it and hands
it over. The user-facing half is [`herdr-plugin/README.md`](../herdr-plugin/README.md);
this is the rest.

It lives in `herdr-plugin/`, ships by git clone rather than with the crate, and
is a manifest plus three shell scripts. Herdr plugin panes run an ordinary argv
command in a real TTY, so `zoe` itself is the plugin UI. Nothing is embedded in
Herdr's render loop, and nothing about the graph is written twice.

---

## 1. Where it sits on the boundary

[DISCOVERY.md](DISCOVERY.md) fixed the input side: a provider states what a file
is, the core decides what a session is, and a feeder reaches it through `open`.
The plugin is the case `Target::Id` was written for. The id is known, the file
is not, and finding the file is the core's job across every provider's roots.

That is the whole reason the bridge stays small. It never reads a transcript,
never learns a layout, and never guesses a project from a working directory. It
resolves one pair, `(agent, session id)`, and spends it on one command:

```
zoe --provider <agent> --follow <id>
```

`--provider` narrows the lookup to the agent Herdr named. `--follow` is right
by definition here, since the pane's agent is running.

## 2. The three scripts

| Script | Role |
|---|---|
| `herdr/pane.sh` | all three actions (`open`, `open-split`, `open-tab`), differing only in the placement they pass. Opens the graph pane, or closes it when the graph is the focused pane, which is what makes the key a toggle |
| `herdr/open.sh` | the pane command. Resolves the session and execs `zoe` |
| `herdr/resolve.sh` | asks `pane.get` about the focused pane, prints `<agent> <session-id>`, or exits with the reason |
| `herdr/keys.sh` | the `setup-keys` / `remove-keys` actions |
| `herdr/ensure-zoe.sh` | the `[[build]]` step: is a usable `zoe` on `PATH` |

Two decisions in there are worth keeping.

**Resolution reads `focused_pane_id`, never `HERDR_PANE_ID`.** In a pane
command, `HERDR_PANE_ID` is the plugin's own newly created pane, so asking Herdr
about it returns a pane with no agent. `focused_pane_id` in
`HERDR_PLUGIN_CONTEXT_JSON` is the pane the plugin was invoked from, and it is
the same field for an action and for a pane command.

**Every message is printed in the pane, and held until the user presses enter.**
An action runs headless with its output going to `herdr plugin log`, and Herdr
notifications can be switched off, in which case a notification is silently
dropped. The pane's own terminal is the only surface that cannot be turned off.
Resolution therefore happens in the pane, where its failures are visible, and
not in the action, where they would not be.

**Closing recognises the plugin's own pane by label.** Herdr labels a plugin
pane with its manifest title, so a focused pane labelled `zoetrope` that holds
no agent is ours, and the key closes it instead of trying to graph it. A pane a
user renamed `zoetrope` still holds an agent, so it is not mistaken for ours.

## 3. Keys

Plugin v1 declares actions, event hooks, panes and link handlers, and nothing
else. Keybindings live in the user's own config, and Herdr 0.8.2 has no command
palette, so an action with no key is reachable only from the command line. Of
the plugins surveyed, most document a snippet to paste and three ship an action
that writes it; `setup-keys` is the latter.

It resolves the config path the way Herdr does (`HERDR_CONFIG_PATH`, then
`$XDG_CONFIG_HOME/herdr/config.toml`, then `~/.config/herdr/config.toml`),
refuses to touch a file that does not already pass `herdr config check`, backs
it up, writes one marker-fenced block, validates the result and restores the
backup if it does not parse, then reloads the running Herdr.

It binds one key. Where the graph opens is a standing preference rather than a
per-press decision, so claiming three chords of a shared keyspace to express it
would be a poor trade. The other two placements are written into the same block
as commented bindings, which documents them where the user will look.

## 4. Versions

Three numbers, three reasons to change, and none of them follows a zoetrope
release:

| Number | Where | Bumped when |
|---|---|---|
| `version` | `herdr-plugin.toml` | the files in `herdr-plugin/` change. Displayed by Herdr, resolved by nothing |
| `min_herdr_version` | `herdr-plugin.toml` | a script starts using a newer Herdr API. The only one that can block an install |
| `ZOE_SINCE` | `herdr/ensure-zoe.sh` | the `zoe` command line the scripts call changes. A label for the error message, not a check |

`herdr plugin install` clones the repo at its default branch (or at `--ref`), so
what people get is main, and reinstalling is how it updates. There is no
registry, no tag resolution and no `plugin update`.

**Why the manifest version does not mirror the crate.** Plugins that keep the
two in step have a reason to: their installer builds a release download URL out
of the version, because the plugin is how their binary is distributed. Nothing
here downloads anything. `zoe` reaches people through Homebrew and crates.io,
and the bridge drives whatever is on `PATH`, so a mirrored number would be a
hand-edited label with no reader, touched on every release including the ones
that never came near this directory.

**Why the binary is gated on capability, not version.** The build step asks
`zoe --help` whether it takes `--provider`. A version string is a label: a
binary built from a checkout carries the crate version of its base release, so
a floor would refuse a `zoe` that has the flag, while a future release that
renamed the flag would pass a floor and then fail at the first key press.

**The one rule tying the two release channels together.** A script here may only
call a `zoe` command line that is already published, because installs take the
branch while the binary comes from a release. Calling something unreleased would
break every fresh install until the crate ships.

## 5. Notes on Herdr's API

Read from `herdr api schema --json` on Herdr 0.8.2 (protocol 20), the plugin and
socket docs at v0.9.0, and live responses.

- `PaneInfo` is `pane_id` (not `id`), `agent`, `agent_session`, `agent_status`,
  `cwd`, `foreground_cwd`, `display_agent`, `focused`, `tokens`, `workspace_id`.
- `pane.get` nests that record one level down, as
  `{"result": {"pane": {...}, "type": "pane_info"}}`; `pane.list` answers
  `.result.panes`. Every CLI response is `{id, result}` or `{id, error}`.
- `AgentSessionInfo` is `{source, agent, kind, value}`, all required, and the
  whole object is absent until an integration reports a session.
- `AgentSessionRefKind` is `"id"` or `"path"`. Herdr maps `herdr:claude` and
  `herdr:codex` to an id, reported by each agent's `SessionStart` hook; `pi` and
  `omp` store a path, and zoetrope does not read those agents.
- `PluginInvocationContext` is flat: `focused_pane_id`, `focused_pane_agent`,
  `focused_pane_cwd`, `focused_pane_status`, `workspace_cwd`, and so on.
- Action `contexts` are `global`, `workspace`, `tab`, `pane`, `selection`,
  stored but not rendered in 0.8.2. Pane `placement` is `overlay`, `popup`,
  `split`, `tab` or `zoomed`; `overlay` is a temporary zoom that restores the
  previous focus and zoom on close, `popup` is session-modal, has no pane id and
  sits outside the pane APIs.
- Runtime environment: `HERDR_SOCKET_PATH`, `HERDR_BIN_PATH`, `HERDR_PLUGIN_ID`,
  `HERDR_PLUGIN_ROOT`, `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR`,
  `HERDR_PLUGIN_CONTEXT_JSON`, plus `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`,
  `HERDR_PANE_ID` where they apply. Actions also get `HERDR_PLUGIN_ACTION_ID`,
  pane commands `HERDR_PLUGIN_ENTRYPOINT_ID`.

## 6. Working on it

```bash
herdr plugin link "$(pwd)/herdr-plugin"        # link runs no build step: have zoe on PATH
herdr plugin action list --plugin furkankly.zoetrope
herdr plugin log list --plugin furkankly.zoetrope   # where an action's output goes
herdr pane list | jq '.result.panes[] | {pane_id, agent, agent_session}'
```

`herdr plugin link` re-reads the manifest, so run it after editing
`herdr-plugin.toml`. The scripts read Herdr only through `$HERDR_BIN_PATH` and
`jq`, so they can be exercised without Herdr by putting a stub `herdr` on
`PATH` that prints a canned `pane.get` response and setting
`HERDR_PLUGIN_CONTEXT_JSON` to `{"focused_pane_id":"w1:p1"}`.

Anything asserted here about Herdr's shapes came from the schema or a live
response, never from memory. Keep it that way: the field names are close enough
to plausible-but-wrong that guessing has cost a debugging session already.
