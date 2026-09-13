#!/usr/bin/env bash
# Print "<agent> <session-id-or-path>" for the pane the plugin was invoked
# from, or exit non-zero with the reason on stderr.
#
# The pane id comes from `focused_pane_id` in HERDR_PLUGIN_CONTEXT_JSON, never
# from HERDR_PANE_ID: in a pane command HERDR_PANE_ID is the plugin's own new
# pane, and asking Herdr about that one returns a pane with no agent. The
# context names the pane that was focused when the plugin was invoked, and it
# is the same field for an action and for a pane command.
#
# Herdr's Claude Code and Codex integrations report the native session id
# from a SessionStart hook (`pane.report_agent_session`), so `pane.get`
# carries `agent_session: {source, agent, kind: "id", value}` for both. omp
# and pi report the same shape but with `kind: "path"`: their integration
# already knows the transcript file, so the value is the absolute path zoe
# needs, not something to resolve further. Either kind is a pair the plugin
# hands straight to zoe; nothing is guessed from the working directory, and
# when Herdr has neither, the caller says so.
set -euo pipefail

herdr="${HERDR_BIN_PATH:-herdr}"
ctx="${HERDR_PLUGIN_CONTEXT_JSON:-{\}}"

command -v jq >/dev/null 2>&1 || { echo "jq is not on PATH, and the plugin reads Herdr's JSON with it" >&2; exit 1; }

pane_id=$(printf '%s' "$ctx" | jq -r '.focused_pane_id // empty')
[ -n "$pane_id" ] || { echo "no focused pane in the invocation context" >&2; exit 1; }

resp=$("$herdr" pane get "$pane_id" 2>&1) || { echo "herdr pane get failed: $resp" >&2; exit 1; }
err=$(printf '%s' "$resp" | jq -r '.error.message // empty' 2>/dev/null || true)
[ -z "$err" ] || { echo "herdr: $err" >&2; exit 1; }

# `pane.get` answers `{"result": {"pane": {...}, "type": "pane_info"}}`: the
# record is under `.result.pane`, not `.result` itself.
pane=$(printf '%s' "$resp" | jq '.result.pane // .result')
agent=$(printf '%s' "$pane" | jq -r '.agent_session.agent // .agent // empty')
kind=$(printf  '%s' "$pane" | jq -r '.agent_session.kind  // empty')
value=$(printf '%s' "$pane" | jq -r '.agent_session.value // empty')

case "$agent" in
  claude | codex | omp | pi) ;;
  "") echo "pane $pane_id has no agent: focus a Claude Code, Codex, omp or pi pane" >&2; exit 1 ;;
  *)  echo "agent '$agent' in pane $pane_id is not one zoe reads (Claude Code, Codex, omp, pi)" >&2; exit 1 ;;
esac

case "$kind" in
  id) ;;
  # omp and pi already know their transcript file, so herdr hands over the
  # absolute path instead of an id to resolve. It only needs checking, since
  # a rotated or deleted transcript is the realistic way this goes stale.
  path) [ -e "$value" ] || { echo "herdr reports $value for this $agent pane, but that file does not exist (rotated, or deleted since?)" >&2; exit 1; } ;;
  "") cat >&2 <<MSG
Herdr has no session for this $agent pane.

  The id (or path, for omp and pi) comes from the agent's own integration,
  which reports it only once a session begins. So: install the integration if
  it is missing, then start the agent in that pane again. A session that was
  already running when the integration was installed never reports one.

    herdr integration install $agent    (herdr integration status lists them)
MSG
     exit 1 ;;
  *)  echo "Herdr reports a $kind for this $agent pane, and the plugin expects an id or a path" >&2; exit 1 ;;
esac

printf '%s %s\n' "$agent" "$value"
