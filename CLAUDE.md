# zoetrope

Terminal UI that visualizes Claude Code agent sessions as a live flow graph. The same core also runs in the browser, compiled to wasm. Read-only, zero network.

Transcript formats enter through one boundary: `src/provider/` turns records into the facts in `src/fact.rs`, and nothing past it knows the format. See `docs/ARCHITECTURE.md` §0 before adding to either side.

See `docs/ARCHITECTURE.md` for the invariants and principles, `docs/DESIGN.md` for the module map and transcript format, `TODO.md` for the roadmap, and `README.md` for usage.

## Running

Two crates: `zoetrope` at the root (the published lib + the `zoe` binary) and `zoetrope-web` in `web/wasm/` (the browser frontend, `publish = false`, wasm32-only). The browser frontend is **excluded** from the root workspace and resolves as its own — it cannot be compiled for the host at all, so keeping it a member would mean permanent phantom errors in the editor. The commands below therefore only touch `zoetrope`; the browser frontend needs its own manifest path.

```bash
cargo build                     # native (default features)
cargo clippy                    # lint — must pass with no warnings
cargo fmt                       # format
cargo test                      # test

# the browser frontend (trunk → web/public/wasm/), lint it explicitly:
bash web/scripts/build-wasm.sh
cd web/wasm && cargo clippy   # its .cargo/config.toml defaults to wasm32
```

## Commits

[Conventional Commits](https://www.conventionalcommits.org/), lowercase imperative subject. Scope is a module, not a file: `fact`, `provider`, `state`, `graph`, `timeline`, `tailer`, `ui`, `panel`, `cli`, `wasm`, `web`, `docs`. A change inside one provider is `provider` (e.g. `fix(provider): inherit the timestamp across progress records`).

```
feat(timeline): index the playhead by event instead of wall-clock
fix(tailer): fold appends at the live edge without rebuilding
```

`cliff.toml` sets `filter_unconventional = true` — a non-conforming commit is dropped from the changelog. Lint with `committed HEAD~1..HEAD`, generate with `git cliff -o CHANGELOG.md`.
