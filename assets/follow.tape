# Regenerates assets/zoetrope-follow.gif — the same session in Follow camera.
#
#   cargo build --release
#   vhs assets/follow.tape        # run from the repo root
#   cp assets/zoetrope-follow.gif web/public/
#
# Companion to demo.tape, which watches the whole graph in Overview. Here `f`
# hands the camera to the action: it glides to whichever agent just did
# something and keeps a readable zoom, which is what following a LIVE session
# actually looks like. `o` at the end fits everything back, so the last frame of
# the loop is the finished graph rather than a viewport mid-glide.
# Two Outputs, one render. The MP4 is for the website and the GIF for the
# README, and emitting both here means they are the same recording by
# construction — there is no transcode step to skip, and no way for one to end
# up older than the other. VHS renders true-colour frames, so the MP4 never
# passes through the GIF's 256-colour palette on its way out.
Output assets/zoetrope-follow.gif
Output assets/zoetrope-follow.mp4

Set Shell "bash"
Set FontSize 14
Set Width 1728
Set Height 944
Set Padding 0
Set Framerate 24

Hide
Type "clear"
Enter
Sleep 1s
Type "./target/release/zoe assets/claude/demo.jsonl --speed 14"
Enter
Show

# Hand the camera to the activity before much has happened.
Sleep 1.5s
Type "f"
Sleep 14s

# Fit back out on the finished graph.
Type "o"
Sleep 2.5s
