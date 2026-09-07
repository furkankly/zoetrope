# Regenerates assets/zoetrope-tour.gif — the interaction tour.
#
#   cargo build --release
#   vhs assets/tour.tape          # run from the repo root
#   cp assets/zoetrope-tour.gif web/public/
#
# VHS is keyboard-only and films a headless terminal with no OS pointer, so the
# app draws its own: ZOETROPE_DEMO=1 arms the scripted pointer in
# `src/autopilot.rs`, and `t` fires it. Firing on a key (rather than at startup)
# lets this tape hide the replay and open on the finished graph — the tour is
# about using the app, not about watching it load. demo.tape covers the build.
#
# The waypoints are read off the live layout at the moment `t` lands, so they
# survive a relayout. Sleep below must be at least the script's own length,
# which the binary reports and ./assets/build.sh check verifies:
#     ZOETROPE_DEMO=duration target/release/zoe
# A short sleep means a
# trailing still frame is the worst thing in a looping recording.
# Two Outputs, one render. The MP4 is for the website and the GIF for the
# README, and emitting both here means they are the same recording by
# construction — there is no transcode step to skip, and no way for one to end
# up older than the other. VHS renders true-colour frames, so the MP4 never
# passes through the GIF's 256-colour palette on its way out.
Output assets/zoetrope-tour.gif
Output assets/zoetrope-tour.mp4

Set Shell "bash"
Set FontSize 14
Set Width 1728
Set Height 944
Set Padding 0
Set Framerate 24

# Launch and let the replay finish off-camera.
Hide
Type "clear"
Enter
Sleep 1s
Type "ZOETROPE_DEMO=1 ./target/release/zoe assets/claude/demo.jsonl --speed 60"
Enter
Sleep 5s
Show

# A beat on the settled graph, then hand over to the pointer.
Sleep 0.6s
Type "t"
Sleep 10.8s
