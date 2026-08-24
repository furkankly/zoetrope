---
title: 安装
description: 用 Homebrew、cargo 或预编译二进制安装 zoetrope 终端应用（也可以从源码构建），或者跳过安装、直接在浏览器里运行。
---

zoetrope 有两种运行方式：安装一个终端应用，或者把同一套可移植核心编译成
WebAssembly 在浏览器里跑。浏览器版完全不需要安装。

## 在浏览器里运行

体验 zoetrope 最快的方式就是浏览器版：打开即加载一个演示会话，然后你可以浏览
自己的会话，或拖入一份 transcript。

- **[打开浏览器版 →](/app)**

它完全运行在你的机器上（它是作为静态页面提供的 WebAssembly）；你的 transcript
永远不会被上传。控制方式与终端版完全一致，参见[用法与按键](/zh/guides/usage/)。

## 安装终端应用

无论怎么安装，项目名叫 `zoetrope`，装出来的命令是 `zoe`。

### Homebrew

在 macOS 和 Linux 上：

```sh
brew install furkankly/tap/zoetrope
```

这个 formula 直接安装预编译二进制，覆盖 Apple Silicon、Intel macOS 以及 arm64 /
x86_64 Linux——不需要 Rust 工具链。`brew upgrade zoetrope` 跟进新版本。

### Cargo

如果你已有 Rust 工具链，crates.io 上发布了一个 crate：

```sh
cargo install zoetrope
```

### 预编译二进制

每个 [release](https://github.com/furkankly/zoetrope/releases) 都附带从打 tag 的
那个 commit 原样构建的压缩包：

| 平台 | 压缩包 |
| --- | --- |
| macOS，Apple Silicon | `zoetrope-<version>-aarch64-apple-darwin.tar.gz` |
| macOS，Intel | `zoetrope-<version>-x86_64-apple-darwin.tar.gz` |
| Linux，arm64（`musl`） | `zoetrope-<version>-aarch64-unknown-linux-musl.tar.gz` |
| Linux，x86_64（`musl`） | `zoetrope-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Windows，x86_64 | `zoetrope-<version>-x86_64-pc-windows-msvc.zip` |

解压任一个，把 `zoe` 放到 `PATH` 里的某个目录。Linux 构建是静态的（`musl`），
不挑发行版。Windows 只有二进制——不在 Homebrew tap 里。

### 从源码构建

```sh
git clone https://github.com/furkankly/zoetrope zoetrope
cd zoetrope
cargo build --release
# 二进制位于 ./target/release/zoe
```

### 环境要求

- Homebrew 和预编译二进制完全不需要工具链。
- 自行构建（`cargo install` 或从源码）需要较新的稳定版 Rust 工具链（推荐
  `rustup`）。
- 一个支持真彩色和鼠标事件的终端（大多数现代终端都支持）。

## 自己构建浏览器版

浏览器前端不在发布的 crate 里——它是一个独立的、不发布的 crate
`zoetrope-web`，只针对 wasm32 构建，因此被排除在根 workspace 之外，任何根目录的
`cargo` 命令都不会碰它。它和本站一起放在
[`web/`](https://github.com/furkankly/zoetrope/tree/main/web)（crate 本体在
`web/wasm/`），由 [`trunk`](https://trunkrs.dev) 编译成 wasm 并经 Astro 提供：

```sh
cd web
pnpm install
pnpm build          # 先构建 wasm，再构建静态站点 → web/dist/
pnpm dev            # 或者：在 http://localhost:4321 启动开发服务器
```

你需要 `wasm32-unknown-unknown` 目标（`rustup target add
wasm32-unknown-unknown`）和 `trunk`（`cargo install trunk`）。`pnpm build:wasm`（即
`bash scripts/build-wasm.sh`）只构建 wasm；从仓库根目录用
`cd web/wasm && cargo clippy` 检查它——那个 crate 的 `.cargo/config.toml` 已把目标
默认为 wasm32，无需额外参数。

## 状态

早期、预发布阶段。可以用来观摩自己的会话，但按键、CLI 以及它读取的磁盘格式仍可能
变动。如果哪里看起来不对，请[提一个 issue](https://github.com/furkankly/zoetrope/issues)。
