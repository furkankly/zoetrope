#!/usr/bin/env bash
# Action command: install or remove the plugin's default keybindings.
#
# Herdr plugins cannot ship keys in their manifest (plugin v1 binds keys only
# from the user's config), so this writes one marker-fenced block into the
# config Herdr loads, the way it resolves the path itself: HERDR_CONFIG_PATH,
# then $XDG_CONFIG_HOME/herdr/config.toml, then ~/.config/herdr/config.toml.
#
# Rules: never touch anything outside the block, leave a key that is already
# bound elsewhere alone and say so, back the file up before writing, hand the
# result to `herdr config check`, and roll back if it does not pass.
set -euo pipefail

herdr="${HERDR_BIN_PATH:-herdr}"
plugin="${HERDR_PLUGIN_ID:-furkankly.zoetrope}"
begin="# >>> $plugin keys (managed: \`setup-keys\` writes this block, \`remove-keys\` deletes it)"
end="# <<< $plugin keys"

# Where the graph opens is a standing preference, not a per-press decision, so
# one key is bound and the other placements are written into the same block as
# comments. Switching is uncommenting a binding, or changing `open` to
# `open-tab` in the one that is live. Each key sits next to the Herdr default
# that does the same thing to a pane: prefix+z zooms, prefix+v splits,
# prefix+c opens a tab.
#
# key | action id | description
bindings=(
  "prefix+shift+z|open|zoetrope: session graph (overlay)"
)
alternates=(
  "prefix+shift+v|open-split|zoetrope: session graph (split)"
  "prefix+shift+c|open-tab|zoetrope: session graph (tab)"
)

config_path() {
  if [ "${HERDR_CONFIG_PATH+set}" = set ]; then
    printf '%s' "$HERDR_CONFIG_PATH"
  elif [ -n "${XDG_CONFIG_HOME:-}" ]; then
    printf '%s/herdr/config.toml' "$XDG_CONFIG_HOME"
  else
    printf '%s/.config/herdr/config.toml' "$HOME"
  fi
}

# The file without the managed block.
strip_block() {
  awk -v b="$begin" -v e="$end" '
    $0 == b { skip = 1; next }
    $0 == e { skip = 0; next }
    !skip
  ' "$1"
}

# An action runs on the server with no terminal, and `plugin action invoke`
# returns before it finishes: stdout goes to `herdr plugin log`. A notification
# is sent too, for the setups that have them switched on.
say() {
  echo "$*"
  "$herdr" notification show "zoetrope" --body "$*" --sound none >/dev/null 2>&1 || true
}

fail() {
  echo "$*" >&2
  "$herdr" notification show "zoetrope: keybindings not installed" --body "$*" --sound none >/dev/null 2>&1 || true
  exit 1
}

reload() {
  "$herdr" server reload-config >/dev/null 2>&1 \
    || echo "No running Herdr to reload; the keys are live from the next start."
}

path=$(config_path)
[ -n "$path" ] || fail "HERDR_CONFIG_PATH is set but empty, so Herdr loads no config and there is nowhere to put a key."

case "${1:-}" in
  setup)
    mkdir -p "$(dirname "$path")"
    [ -f "$path" ] || : > "$path"
    "$herdr" config check >/dev/null 2>&1 || fail "$path does not pass \`herdr config check\`; fix it first, nothing was changed."

    rest=$(strip_block "$path")
    installed=""
    skipped=""
    lines=()
    for entry in "${bindings[@]}"; do
      key=${entry%%|*}
      rem=${entry#*|}
      action=${rem%%|*}
      desc=${rem#*|}
      if printf '%s\n' "$rest" | grep -q "\"$key\""; then
        skipped="$skipped $key"
        continue
      fi
      installed="$installed $key"
      lines+=("[[keys.command]]" "key = \"$key\"" "type = \"plugin_action\"" \
              "command = \"$plugin.$action\"" "description = \"$desc\"" "")
    done
    installed=${installed# }
    skipped=${skipped# }

    # The placements this block does not bind, offered in place.
    lines+=("# The graph opens over the focused pane and gives the layout back when" \
            "# it closes. The same key closes it. Two other placements, if you would" \
            "# rather keep the graph on screen: uncomment one, or point the binding" \
            "# above at its action. Then: herdr server reload-config")
    for entry in "${alternates[@]}"; do
      key=${entry%%|*}
      rem=${entry#*|}
      action=${rem%%|*}
      desc=${rem#*|}
      lines+=("# [[keys.command]]" "# key = \"$key\"" "# type = \"plugin_action\"" \
              "# command = \"$plugin.$action\"" "# description = \"$desc\"" "")
    done

    [ -n "$installed" ] || fail "Every default key is already bound in $path ($skipped), so nothing was changed. Bind $plugin.open to a key of your own instead."

    cp "$path" "$path.zoetrope-backup"
    # `rest` came from a command substitution, which already dropped every
    # trailing newline, so the block always lands after exactly one blank line.
    { [ -n "$rest" ] && printf '%s\n\n' "$rest"
      printf '%s\n' "$begin"
      printf '%s\n' "${lines[@]}"
      printf '%s\n' "$end"
    } > "$path"

    if ! "$herdr" config check >/dev/null 2>&1; then
      cp "$path.zoetrope-backup" "$path"
      fail "The written config did not pass \`herdr config check\`; restored $path from the backup."
    fi
    reload
    msg="Bound $installed in $path (backup: $path.zoetrope-backup). Focus an agent pane and press ${installed%% *}. The split and tab placements are in the same block, commented out."
    [ -z "$skipped" ] || msg="$msg Left $skipped alone, already bound there."
    say "$msg"
    ;;
  remove)
    [ -f "$path" ] || { say "No config at $path, nothing to remove."; exit 0; }
    grep -qxF "$begin" "$path" || { say "No $plugin block in $path, nothing to remove."; exit 0; }
    cp "$path" "$path.zoetrope-backup"
    rest=$(strip_block "$path.zoetrope-backup")
    if [ -n "$rest" ]; then printf '%s\n' "$rest" > "$path"; else : > "$path"; fi
    reload
    say "Removed the $plugin keybindings from $path (backup: $path.zoetrope-backup)."
    ;;
  *)
    echo "usage: keys.sh setup|remove" >&2
    exit 2
    ;;
esac
