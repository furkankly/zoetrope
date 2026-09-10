# Regenerates assets/zoetrope-codex.gif — the "Codex, same app" panel on the
# landing page and the recording under the README's supported-agents table.
#
#   cargo build --release
#   vhs assets/codex.tape          # run from the repo root
#
# Records the Codex CLI capture (assets/codex/cli-0.153.4/, a real session on
# the same dark-mode brief the Claude demo uses), opened by its root rollout;
# the provider is read off the file, and its four subagent rollouts are found
# beside it. --speed 12 replays the session's ~3 minutes in ~15s; the keys
# afterwards open a subagent's panel (its prompt, the reasoning that spawned
# it, both tool layers of an `exec` call) and the session-info overlay (app,
# version, cwd). Two Outputs, one render: see demo.tape for why.
Output assets/zoetrope-codex.gif
Output assets/zoetrope-codex.mp4

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
Type "./target/release/zoe assets/codex/cli-0.153.4/2026/09/07/rollout-2026-09-07T16-25-14-01a07c0b-5e16-70f2-ad21-7c2a3a228b22.jsonl --speed 12"
Enter
Show

Sleep 16s

# A subagent's detail panel: the prompt that spawned it, the reasoning around
# it, and its tool calls with the shell commands the `exec` programs ran.
Down
Sleep 3s
Right
Sleep 4s
Escape
Sleep 1s

# Session info overlay: the app, its version, the working directory.
Type "i"
Sleep 3s
Escape
Sleep 2s
