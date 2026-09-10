#!/usr/bin/env bash
# One entry point for every moving image and social card in this repo.
#
#   ./assets/build.sh tapes    regenerate the recordings (GIF + MP4 together)
#   ./assets/build.sh og       rebuild the social card
#   ./assets/build.sh social   rebuild the GitHub repo social preview
#   ./assets/build.sh sync     copy everything into web/public
#   ./assets/build.sh check    verify the lot without changing anything
#   ./assets/build.sh all      tapes + og + social + sync
#
# ADDING A RECORDING: add one "tape:output" line to DEMOS below and drop the
# matching assets/<tape>.tape beside it. Encoding, syncing and the orphan and
# staleness checks all derive from that line.
#
# WHY BOTH FORMATS, since they look redundant and are not:
#
#   GIF  the README. GitHub strips <video> from markdown, and its auto-embedded
#        player needs a file uploaded through the web UI to `user-attachments`,
#        which no script can regenerate.
#   MP4  the landing page. These three were 5.5 MB of GIF in three <img>s;
#        as H.264 they are about 1.7 MB, which is most of the page weight. Each
#        tape emits it alongside its GIF, from the same render, so the two
#        cannot disagree and there is no transcode step to forget.
#
# The GIFs stay in web/public too, as the <video> fallback. If that fallback is
# ever judged not worth its deploy size, delete the copies AND the nested <img>
# in web/src/pages/index.astro — one without the other is what actually breaks.
set -euo pipefail

cd "$(dirname "$0")/.."

# tape basename : output basename. They differ because the outputs are prefixed.
DEMOS=(
  "demo:zoetrope-demo"
  "follow:zoetrope-follow"
  "tour:zoetrope-tour"
  "codex:zoetrope-codex"
)
# Recordings VHS cannot make. A Herdr pane runs the plugin inside a live
# multiplexer with a real agent working in the next pane, which no tape can
# script, so this one is captured by hand and only ever replaced by hand. It is
# exempt from the orphan and MP4 checks below; the README links it directly.
HAND=(
  "zoetrope-herdr"
)

WEB_PUBLIC=web/public

die() { echo "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null || die "missing $1"; }
outputs() { for d in "${DEMOS[@]}"; do echo "${d##*:}"; done; }

cmd_tapes() {
  have vhs
  cargo build --release
  for d in "${DEMOS[@]}"; do
    echo "vhs assets/${d%%:*}.tape"
    vhs "assets/${d%%:*}.tape"     # writes both the GIF and the MP4
  done
}

cmd_og() {
  have vhs
  cargo build --release
  vhs assets/og.tape          # writes assets/og-shot.png; see the tape
  rm -f tmp-og.gif            # VHS requires an Output even when only a Screenshot is wanted
  node web/scripts/og.mjs
}

cmd_social() {
  node web/scripts/social-preview.mjs
  echo "  upload it by hand: repo Settings -> Social preview (GitHub has no API for it)"
}

cmd_sync() {
  cp assets/*.gif assets/*.mp4 "$WEB_PUBLIC/" 2>/dev/null || true
  [[ -f assets/og.png ]] && cp assets/og.png "$WEB_PUBLIC/"
  # The favicon is the mark, so it is a copy of assets/icon.svg rather than its
  # own drawing. Synced here for the same reason og.png is: the site serves from
  # web/public, and a second hand-maintained copy of a mark ends up a version
  # behind the one it was drawn from.
  # ...and again into web/src/assets, which is where Starlight's `logo` option
  # imports it from: Astro will not import an asset from outside web/.
  [[ -f assets/icon.svg ]] && cp assets/icon.svg "$WEB_PUBLIC/favicon.svg" \
    && cp assets/icon.svg web/src/assets/icon.svg
  echo "  gifs + mp4s + og.png + favicon.svg + src/assets/icon.svg -> web/"
}

cmd_check() {
  local bad=0
  echo "expected recordings: $(outputs | tr '\n' ' ')"

  # tour.tape has to sleep at least as long as the pointer script runs, or VHS
  # cuts the recording mid-gesture. The length is asked of the binary, which
  # sums its own Step list — the tape used to carry a hand-copied "MUST equal
  # the script's total (~10.8s)" that nothing enforced.
  if [[ -x target/release/zoe ]]; then
    local secs sleep_s
    secs=$(ZOETROPE_DEMO=duration target/release/zoe 2>/dev/null || echo 0)
    sleep_s=$(grep -oE '^Sleep [0-9.]+s' assets/tour.tape | tail -1 | grep -oE '[0-9.]+')
    if [[ -n $secs && -n $sleep_s ]] && awk -v a="$sleep_s" -v b="$secs" 'BEGIN{exit !(a < b)}'; then
      echo "  SHORT   assets/tour.tape sleeps ${sleep_s}s for a ${secs}s script"; bad=1
    fi
  else
    echo "  note: cargo build --release to check tour.tape's Sleep"
  fi

  for name in $(outputs); do
    for ext in gif mp4; do
      [[ -f assets/$name.$ext ]] || { echo "  MISSING assets/$name.$ext"; bad=1; }
      if [[ -f assets/$name.$ext ]] && ! cmp -s "assets/$name.$ext" "$WEB_PUBLIC/$name.$ext"; then
        echo "  UNSYNCED $WEB_PUBLIC/$name.$ext"; bad=1
      fi
    done
    # A GIF newer than its MP4 means the page is serving an older recording than
    # the README — skew nobody notices until the two are seen side by side.
    if [[ -f assets/$name.gif && -f assets/$name.mp4 && assets/$name.gif -nt assets/$name.mp4 ]]; then
      echo "  STALE   assets/$name.mp4 is older than its GIF"; bad=1
    fi
  done

  # Anything in assets/ that no tape produces.
  for f in assets/*.gif assets/*.mp4; do
    [[ -e $f ]] || continue
    local n; n=$(basename "$f"); n=${n%.*}
    outputs | grep -qx "$n" && continue
    printf '%s\n' "${HAND[@]}" | grep -qx "$n" ||
      { echo "  ORPHAN  $f is not in DEMOS or HAND"; bad=1; }
  done

  [[ -f assets/og-shot.png ]] || { echo "  MISSING assets/og-shot.png (run: build.sh og)"; bad=1; }
  [[ -f $WEB_PUBLIC/og.png ]] && cmp -s assets/og.png "$WEB_PUBLIC/og.png" || {
    echo "  UNSYNCED $WEB_PUBLIC/og.png"; bad=1; }
  [[ -f $WEB_PUBLIC/favicon.svg ]] && cmp -s assets/icon.svg "$WEB_PUBLIC/favicon.svg" || {
    echo "  UNSYNCED $WEB_PUBLIC/favicon.svg (run: build.sh sync)"; bad=1; }
  [[ -f web/src/assets/icon.svg ]] && cmp -s assets/icon.svg web/src/assets/icon.svg || {
    echo "  UNSYNCED web/src/assets/icon.svg (run: build.sh sync)"; bad=1; }
  [[ -f assets/social-preview.png ]] || { echo "  MISSING assets/social-preview.png (run: build.sh social)"; bad=1; }
  # The card is drawn FROM the mark, so a mark edited afterwards means the card
  # on GitHub is of an older drawing. Same staleness rule as the MP4s.
  if [[ assets/mark.svg -nt assets/social-preview.png ]]; then
    echo "  STALE   assets/social-preview.png is older than assets/mark.svg"; bad=1
  fi

  ((bad == 0)) && echo "all good" || return 1
}

case "${1:-}" in
  tapes) cmd_tapes ;;
  og)     cmd_og ;;
  social) cmd_social ;;
  sync)  cmd_sync ;;
  check) cmd_check ;;
  all)   cmd_tapes; cmd_og; cmd_social; cmd_sync ;;
  *)     sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
