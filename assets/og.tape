# VHS tape for the social card. 1200x630 is the OG/Twitter ratio, so this frames
# the real TUI to that shape rather than cropping the 1728x944 demo GIF, which
# would cut the detail panel in half.
#
# Regenerate:
#   cargo build --release
#   vhs assets/og.tape
#   node web/scripts/og.mjs
#
# The still is a SCREENSHOT, not a GIF frame. This tape used to write an
# animated GIF that a second command then reduced to its last coalesced frame,
# and a note here claimed VHS's Screenshot directive "silently no-ops in
# 0.11.0". It does not. It needs an `Output` directive to be present —
# Screenshot on its own is ignored — and a relative path, since VHS's parser
# rejects absolute ones. With both, 0.11.0 writes the PNG.
#
# The workaround cost real quality: the card was assembled from a 256-colour GIF
# frame when a true-colour still was there for the asking. For the one image
# that represents the project in every feed, that is the wrong economy.
#
# tmp-og.gif is the throwaway VHS insists on. It is gitignored.
#
# Same fixture as demo.tape, so the card, the README GIF and the wasm app at
# /app all show one session.

Output tmp-og.gif

Set Shell "bash"
Set FontSize 15
Set Width 1200
Set Height 630
Set Padding 0
Set Framerate 12

Hide
Type "clear"
Enter
Sleep 1s
Type "./target/release/zoe assets/claude/demo.jsonl --speed 40"
Enter
Show

# The graph, filling the frame. An earlier cut opened a detail panel here, which
# covers roughly two thirds of the width and pushed the flow graph — the thing
# that identifies zoetrope at a glance — into a sliver at the left edge.
Sleep 9s
Screenshot assets/og-shot.png

Hide
Type "q"
Sleep 300ms
