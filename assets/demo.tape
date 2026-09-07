# Regenerates assets/zoetrope-demo.gif — the README hero and the landing page's
# "See it" panel.
#
#   cargo build --release
#   vhs assets/demo.tape          # run from the repo root
#   cp assets/zoetrope-demo.gif web/public/   # the site serves its own copy
#
# Records the bundled demo fixture (assets/claude/demo.jsonl + assets/claude/demo/subagents/),
# which is the same session the wasm app at /app boots into — so the GIF and the
# browser demo show the same thing (including its workflow group). --speed 14
# replays the fixture's ~200s of content in ~15s; the keys afterwards show the
# detail panel, the session-info overlay, and prompt-era scrubbing.
# Two Outputs, one render. The MP4 is for the website and the GIF for the
# README, and emitting both here means they are the same recording by
# construction — there is no transcode step to skip, and no way for one to end
# up older than the other. VHS renders true-colour frames, so the MP4 never
# passes through the GIF's 256-colour palette on its way out.
Output assets/zoetrope-demo.gif
Output assets/zoetrope-demo.mp4

Set Shell "bash"
Set FontSize 14
Set Width 1728
Set Height 944
Set Padding 0
Set Framerate 24

# Hide the launch: the recording should open on the TUI, not on a shell prompt.
Hide
Type "clear"
Enter
Sleep 1s
Type "./target/release/zoe assets/claude/demo.jsonl --speed 14"
Enter
Show

Sleep 15s

# Show the detail panel: spatial-nav to a subagent so its provenance opens
# (the prompt that spawned it + the reasoning around it).
Down
Sleep 3s
Right
Sleep 3s

# Session info overlay (title, mode, permission, queued, file edits).
Type "i"
Sleep 3s
Escape
Sleep 1s

# Time-travel: step back a prompt era, then snap to the live edge.
Type "["
Sleep 2s
Type "g"
Sleep 2s
