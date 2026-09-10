#!/usr/bin/env bash
# Build step: make sure a `zoe` the plugin can drive is on PATH.
#
# Runs during `herdr plugin install`, after the preview has shown this command;
# `herdr plugin link` skips build steps. Nothing is vendored: `zoe` is a
# standalone tool with a Homebrew tap and a crates.io release, and the plugin
# drives the one the user has rather than keeping a private copy on its own
# update schedule. When it is missing entirely, it is installed with whichever
# package manager is available.
set -euo pipefail

# What the pane needs is `zoe --provider <agent> --follow <id>`: opening a
# session by id, narrowed to one agent. The check below asks the binary whether
# it takes that, rather than comparing version strings, because a version is a
# label and this is the thing that has to work. A build from a checkout carries
# the crate version of its base release, so a floor would reject a binary that
# has the flag; and a future release that renamed the flag would pass a floor
# and then fail at the first key press. The number here only names the release
# where this appeared, for the messages.
ZOE_SINCE="0.2.0"

takes_provider() {
  zoe --help 2>/dev/null | grep -q -- '--provider'
}

version_of() {
  zoe --version 2>/dev/null | tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+\.[0-9]+' | head -1
}

# A checkout carries zoetrope's own source when the plugin lives in its repo,
# so someone installing from main before a release can build exactly what these
# scripts need. Printed as advice; this step never replaces a managed binary.
from_source_hint() {
  root=$(cd "${HERDR_PLUGIN_ROOT:-.}/.." 2>/dev/null && pwd) || return 0
  [ -f "$root/Cargo.toml" ] || return 0
  printf '    cargo install --path "%s" --force   (build it from this checkout)\n' "$root"
}

report() {
  {
    printf '\n  %s\n\n' "$1"
    printf '  %s:\n' "$2"
    printf '    %s furkankly/tap/zoetrope\n' "$3"
    printf '    cargo install zoetrope%s\n' "$4"
    from_source_hint
    printf '\n  then run the plugin install again.\n\n'
  } >&2
  exit 1
}

too_old() { report "$1" "Upgrade with one of" "brew upgrade" " --force"; }
missing() { report "$1" "Install it with one of" "brew install" ""; }

check() {
  if takes_provider; then
    echo "zoe $(version_of) at $(command -v zoe) opens sessions by id: good."
    return 0
  fi
  too_old "zoe $(version_of) at $(command -v zoe) does not take --provider, so it cannot open the session Herdr names. That arrived in $ZOE_SINCE."
}

if command -v zoe >/dev/null 2>&1; then
  check
  exit 0
fi

if command -v brew >/dev/null 2>&1; then
  echo "zoe is not on PATH; installing it with Homebrew."
  brew install furkankly/tap/zoetrope
elif command -v cargo >/dev/null 2>&1; then
  echo "zoe is not on PATH; installing it with cargo (this compiles it, a few minutes)."
  cargo install zoetrope
else
  missing "zoe is not on PATH, and neither Homebrew nor cargo is available to install it."
fi

command -v zoe >/dev/null 2>&1 || missing "zoe was installed but is not on PATH; add its bin directory to PATH."
check
