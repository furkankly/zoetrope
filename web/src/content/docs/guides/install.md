---
title: Install
description: Install the zoetrope terminal app with Homebrew, cargo or a prebuilt binary (or build it from source), or skip the install and run it in your browser.
---

zoetrope runs two ways: a terminal app you install, or the same portable core
compiled to WebAssembly and running in your browser. The browser app needs no install at all.

## Run it in your browser

The fastest way to try zoetrope is the browser app: it loads a demo session on
open, and from there you can browse your own sessions or drop in a transcript.

- **[Open the browser app →](/app)**

It runs entirely on your machine (it's WebAssembly served as a static page); your
transcripts are never uploaded. See [Usage & keys](/guides/usage/) for the controls,
which are identical to the native app.

## Install the terminal app

However you install it, the project is named `zoetrope` and the command it gives
you is `zoe`.

### Homebrew

On macOS and Linux:

```sh
brew install furkankly/tap/zoetrope
```

The formula pours a prebuilt binary for Apple Silicon, Intel macOS, and arm64 or
x86_64 Linux — no Rust toolchain involved. `brew upgrade zoetrope` tracks new
releases.

### Cargo

If you already have a Rust toolchain, crates.io publishes one crate:

```sh
cargo install zoetrope
```

### A prebuilt binary

Every [release](https://github.com/furkankly/zoetrope/releases) carries archives
built from exactly the commit that was tagged:

| Platform | Archive |
| --- | --- |
| macOS, Apple Silicon | `zoetrope-<version>-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `zoetrope-<version>-x86_64-apple-darwin.tar.gz` |
| Linux, arm64 (`musl`) | `zoetrope-<version>-aarch64-unknown-linux-musl.tar.gz` |
| Linux, x86_64 (`musl`) | `zoetrope-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Windows, x86_64 | `zoetrope-<version>-x86_64-pc-windows-msvc.zip` |

Unpack one and put `zoe` somewhere on your `PATH`. The Linux builds are static
(`musl`), so they don't care which distro they land on. Windows is binary-only —
it isn't in the Homebrew tap.

### From source

```sh
git clone https://github.com/furkankly/zoetrope zoetrope
cd zoetrope
cargo build --release
# binary at ./target/release/zoe
```

### Requirements

- The Homebrew and prebuilt routes need no toolchain at all.
- Building (`cargo install` or from source) needs a recent stable Rust toolchain
  (`rustup` recommended).
- A terminal that supports truecolor and mouse events (most modern terminals do).

## Build the browser app yourself

The browser frontend isn't part of the published crate — it's a separate,
unpublished crate, `zoetrope-web`, that only builds for wasm32, so it's excluded
from the root workspace and no root `cargo` command touches it. It lives with this site under
[`web/`](https://github.com/furkankly/zoetrope/tree/main/web) (the crate itself in
`web/wasm/`), and is compiled to wasm by [`trunk`](https://trunkrs.dev) and served
through Astro:

```sh
cd web
pnpm install
pnpm build          # builds the wasm, then the static site → web/dist/
pnpm dev            # or: run the dev server at http://localhost:4321
```

You'll need the `wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`)
and `trunk` (`cargo install trunk`). `pnpm build:wasm` (that is,
`bash scripts/build-wasm.sh`) builds only the wasm; lint it from the repo root
with `cd web/wasm && cargo clippy` — that crate's `.cargo/config.toml` defaults the
target to wasm32, so no flags are needed.

## Status

Early and pre-release. It's usable for dogfooding your own sessions, but the keys,
CLI, and the on-disk format it reads may still shift. If something looks wrong,
please [open an issue](https://github.com/furkankly/zoetrope/issues).
