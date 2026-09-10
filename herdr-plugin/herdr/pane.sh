#!/usr/bin/env bash
# Action command: open the graph pane, or close it when it is the focused one.
#
# Pressing the key again with the graph in front should put the agent back, not
# ask the graph pane which session it is running. Herdr labels a plugin pane
# with its manifest title, so a focused pane labelled "zoetrope" that holds no
# agent is ours.
#
# The pane resolves the session itself (herdr/open.sh). An action runs headless
# with its output going to `herdr plugin log`, and Herdr notifications can be
# turned off, so the pane's own terminal is the only place a message is certain
# to be seen.
set -euo pipefail

herdr="${HERDR_BIN_PATH:-herdr}"
ctx="${HERDR_PLUGIN_CONTEXT_JSON:-{\}}"
title="zoetrope"
placement="${2:-overlay}"

case "${1:-open}" in
  open) ;;
  *) printf 'usage: pane.sh open [overlay|popup|split|tab|zoomed]\n' >&2; exit 2 ;;
esac

if command -v jq >/dev/null 2>&1; then
  focused=$(printf '%s' "$ctx" | jq -r '.focused_pane_id // empty')
  if [ -n "$focused" ]; then
    pane=$("$herdr" pane get "$focused" 2>/dev/null | jq -r '.result.pane // empty' 2>/dev/null || true)
    label=$(printf '%s' "$pane" | jq -r '.label // empty' 2>/dev/null || true)
    agent=$(printf '%s' "$pane" | jq -r '.agent // empty' 2>/dev/null || true)
    if [ "$label" = "$title" ] && [ -z "$agent" ]; then
      exec "$herdr" plugin pane close "$focused"
    fi
  fi
fi

exec "$herdr" plugin pane open \
  --plugin "${HERDR_PLUGIN_ID:-furkankly.zoetrope}" \
  --entrypoint graph \
  --placement "$placement"
