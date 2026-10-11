#!/usr/bin/env bash
# Print "<agent> <session-id>" for the pane the plugin was invoked from, or
# exit non-zero with the reason on stderr.
#
# The pane id comes from `focused_pane_id` in HERDR_PLUGIN_CONTEXT_JSON, never
# from HERDR_PANE_ID: in a pane command HERDR_PANE_ID is the plugin's own new
# pane, and asking Herdr about that one returns a pane with no agent. The
# context names the pane that was focused when the plugin was invoked, and it
# is the same field for an action and for a pane command.
#
# Herdr's Claude Code and Codex integrations report the native session id from
# a SessionStart hook (`pane.report_agent_session`), so `pane.get` carries
# `agent_session: {source, agent, kind: "id", value}` for both. That pair is
# all zoe needs. Nothing is guessed from the working directory; when Herdr has
# no id, the caller says so.
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
  "") echo "pane $pane_id has no agent: focus a Claude Code, Codex, OMP or Pi pane" >&2; exit 1 ;;
  *)  echo "agent '$agent' in pane $pane_id is not one zoe reads (Claude Code, Codex, OMP and Pi)" >&2; exit 1 ;;
esac

# OMP and Pi have no session id zoe can look up: Herdr reports the absolute
# path of the session `.jsonl` (kind `path`), and that exact file is opened.
# The path is never guessed or normalised, and anything that is not a readable
# regular file is refused rather than passed on.
if [ "$agent" = omp ] || [ "$agent" = pi ]; then
  [ "$kind" = path ] || { echo "Herdr reports ${kind:-no session reference} for this $agent pane, and the plugin expects a path" >&2; exit 1; }
  case "$value" in
    "" | *[[:cntrl:]]*) echo "the $agent session path from Herdr is missing or has control characters" >&2; exit 1 ;;
    /*) ;;
    *) echo "the $agent session path from Herdr is not absolute: $value" >&2; exit 1 ;;
  esac
  [ -f "$value" ] && [ -r "$value" ] || { echo "the $agent session path from Herdr is not a readable file: $value" >&2; exit 1; }
  [ "$agent" = pi ] && agent=omp
  printf '%s %s\n' "$agent" "$value"
  exit 0
fi

case "$kind" in
  id) ;;
  "") cat >&2 <<MSG
Herdr has no session id for this $agent pane.

  The id comes from the agent's SessionStart hook, which fires only when a
  session begins. So: install the integration if it is missing, then start the
  agent in that pane again. A session that was already running when the
  integration was installed never reports one.

    herdr integration install $agent    (herdr integration status lists them)
MSG
     exit 1 ;;
  *)  echo "Herdr reports a $kind for this $agent pane, and the plugin expects an id" >&2; exit 1 ;;
esac

printf '%s %s\n' "$agent" "$value"
