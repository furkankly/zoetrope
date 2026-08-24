<p align="center">
  <img src="https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/icon.svg" alt="" width="80">
</p>

<h1 align="center">zoetrope</h1>

<p align="center">
  <em>在终端或浏览器里，把 Claude Code 会话看成一幅实时流程图。</em>
</p>

<p align="center">
  <a href="https://crates.io/crates/zoetrope"><img src="https://img.shields.io/crates/v/zoetrope.svg?style=flat&labelColor=121212&color=d7af00&logo=Rust&logoColor=white" alt="crates.io"></a>
  <a href="https://docs.rs/zoetrope"><img src="https://img.shields.io/docsrs/zoetrope?style=flat&labelColor=121212&color=d7af00&logo=docs.rs&logoColor=white" alt="docs.rs"></a>
  <a href="https://crates.io/crates/zoetrope"><img src="https://img.shields.io/crates/d/zoetrope.svg?style=flat&labelColor=121212&color=d7af00" alt="downloads"></a>
  <a href="https://github.com/furkankly/zoetrope/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/furkankly/zoetrope/ci.yml?branch=main&style=flat&labelColor=121212&color=d7af00&logo=GitHub%20Actions&logoColor=white" alt="build status"></a>
  <a href="https://crates.io/crates/zoetrope"><img src="https://img.shields.io/crates/msrv/zoetrope?style=flat&labelColor=121212&color=d7af00&label=MSRV" alt="minimum supported Rust version"></a>
</p>

<p align="center">
  <a href="https://zoetrope.furkankly.dev"><b>zoetrope.furkankly.dev</b></a> · 整个应用跑在浏览器里，同一个二进制编译成 WASM
</p>

<p align="center">
  <img src="https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope.svg" alt="一个会话被画成流程图：主 agent 在上，它派生的子 agent 在下，底下是工具活动时间线" width="620">
</p>

<p align="center"><a href="README.md">English</a> · 简体中文</p>

Claude Code 会在 `~/.claude/projects/` 下为每个会话写一份 JSONL transcript。
zoetrope 读取它，把会话画成终端里的一张图：主 agent、它派生的子 agent 和工作流、
每个 agent 跑的工具，并随会话推进实时更新。指向一份已结束的录制，它会按会话自身的
时间戳节奏回放；指向一个正在运行的会话，它会实时跟随。它是只读的，任何数据都不会
离开你的机器。

基于 [ratatui](https://ratatui.rs) 与 [rataflow](https://github.com/furkankly/rataflow) 构建。

![zoetrope 回放一个 Claude Code 会话，呈现为流程图](https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope-demo.gif)

## 安装

**Homebrew** — macOS 与 Linux：

```bash
brew install furkankly/tap/zoetrope
```

**Cargo** — 需要 Rust 工具链：

```bash
cargo install zoetrope
```

**预编译二进制** — 无需工具链。每个
[release](https://github.com/furkankly/zoetrope/releases) 都附带 macOS（Apple Silicon
与 Intel）、Linux（`musl`，arm64 与 x86_64）和 Windows（x86_64）的压缩包。解压后把
`zoe` 放进 `PATH` 即可。

无论哪种方式，命令都是 `zoe`。也可以从源码构建：

```bash
git clone https://github.com/furkankly/zoetrope
cd zoetrope
cargo build --release
./target/release/zoe
```

完全不想安装？**[在浏览器里试试](https://zoetrope.furkankly.dev/app)**，
把一份 transcript 拖到页面上，就能看到同一张图。

## 用法

```bash
zoe                          # 跟随当前项目的实时会话
zoe <dir>                    # 跟随另一个项目的会话
zoe <file.jsonl>             # 从头回放一份录制
zoe <file.jsonl> --follow    # 打开一份录制并跟随其实时边缘
zoe <file.jsonl> --speed N   # 回放倍速（默认 8.0）
zoe --lang zh                # 界面语言（默认跟随系统语言）
zoe inspect <file.jsonl>     # 打印会话树并退出（无 TUI）
```

给它一个文件，它会读完整个 transcript，然后继续监听新写入的行。给它一个目录（或不带
参数），它会找到该项目最新的会话并实时跟随。无论怎么启动，控制方式都一样：拖动进度
条、跟随、暂停、回到直播。

同一套引擎也[跑在浏览器里](https://zoetrope.furkankly.dev/app)，经
[ratzilla](https://github.com/ratatui/ratzilla) 编译为 WebAssembly。从磁盘打开一个
会话，或把 transcript 拖到页面上。在浏览器里它同样不离开本机。

## 功能

**流程图**
- 每个 agent 一个节点：主会话、它的子 agent，以及工作流分组（其子项嵌套在下方）
- 每张卡片上有状态、当前工具、工具数与输出 token 数
- agent 工作时连线流动，结束后归于平静
- 工具调用以卡片下方的 chip 呈现（`⚒ bash ×5`，或单次调用期间 `⚒ bash 0.5s` 跳动），
  最终落定为 `✓` 或 `✗`
- 图超出屏幕后出现小地图，标示视口所在位置

**时间旅行**
- 直播与回放共用一条可拖动的进度条
- 按事件而非钟表索引，所以忙碌的一分钟会得到应有的空间，而不是被压成一条缝
- 拖动、暂停、在提示词段之间跳转，或一键回到直播边缘
- 回退定位时，你看到的就是会话当时的样子：agent “反完成”、工具数下降、图收缩回去
- 可选的空隙压缩：跳过空闲段，或保持忠实的实时节奏

**检视**
- 点击任意 agent 查看它的溯源：派生它的提示词、它前后的思考、所用模型，以及每一次
  工具调用和耗时
- 会话信息浮层：模式、权限、排队操作、文件编辑、最近的提示词
- `zoe inspect` 无界面地打印整棵树，没有 TTY 也能跑

**读懂你的会话**
- 实时跟随正在运行的会话，或回放已结束的
- 读入会话写入的一切：主 transcript、其子 agent、以及自带子项的工作流——图就是全貌
- Claude Code 写入未见过的内容也不中断：陌生记录被跳过，永不致命
- 只读，且完全不联网（见下文）

## 按键

`space` 播放/暂停 · `[` `]` 上一个/下一个提示词 · `End` 或 `g` 回到直播 · 拖动进度条
定位 · `?` 查看其余全部。

<details>
<summary>完整按键表</summary>

| 按键 | 作用 |
| --- | --- |
| `space` | 播放 / 暂停（从播放头处继续） |
| `[` / `]` | 上一个 / 下一个提示词段 |
| `End` / `g` | 跳到直播边缘 |
| `s` | 切换空隙压缩（忠实节奏 vs. 跳过空闲段） |
| 鼠标拖动 | 沿进度条定位 |
| `o` / `f` | 镜头：总览 / 跟随 |
| `r` | 重新布局（整理图表） |
| 方向键 / `Tab` / `Shift-Tab` | 在 agent 之间移动 |
| `h` `j` `k` `l` | 平移图表 |
| `+` / `-` / `0` | 放大 / 缩小 / 复位 |
| `c` | 居中到选中的 agent |
| 点击 | 打开 agent 的详情面板 |
| `j` / `k` / `PgUp` / `PgDn` | 滚动详情面板 |
| `i` | 会话信息浮层 |
| `L` | 切换界面语言 |
| `?` | 帮助浮层 |
| `esc` | 关闭浮层 / 清除选中 |
| `q` / `ctrl-c` | 退出 |

选中 agent 时 `j` / `k` 滚动详情面板，否则平移图表。

</details>

把镜头交给动作（`f`），它会滑向刚刚有所动作的 agent。看实时运行就是这样：

![zoetrope 处于跟随模式，镜头追踪正在工作的 agent](https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope-follow.gif)

也可以自己上手：平移、在指针处缩放、打开 agent 的面板、拖动进度条在会话中穿行。

![平移、缩放、打开详情面板、拖动进度条](https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope-tour.gif)

## 界面语言

界面默认跟随系统语言（终端读 `LANG`/`LC_ALL`，浏览器读
`navigator.language`）。显式指定：启动时加 `--lang zh`（或设 `ZOETROPE_LANG`
环境变量），运行中按 `L` 在语言之间循环切换。目前提供英文和简体中文；
`zoe --help` 的用法说明同样按当前语言输出。

## 底层原理

zoetrope 把 transcript 当作**只增不改的事件日志**。它 tail 这些文件，逐行防御式解析，
把内容折叠成一个由 agent、工具调用和提示词组成的派生模型。任何东西都不被原地修改。
模型是"迄今所见事实"的纯函数，因此回退定位是精确的。

把一份日志变成一场可观看的会话，其中有几个关键设计：

- **两个时钟，严格分离。** 内容时间来自 transcript 自身的时间戳；呈现时间是你所控制的
  播放头。所有节奏决策（倍速、空隙压缩、拖动）只碰后者，因此任何显示选择都无法改变
  会话记录所陈述的事实。
- **直播与回放共用一条时间线。** 在直播边缘之后，播放头按节奏前进；在边缘上它钉住，
  新到达的内容随到随折。一个实时会话和一份保存的录制，唯一的区别是播放头从哪里
  开始。同一台引擎，同一套控制。
- **事实优先于启发式。** 父子关系、完成与命名都来自格式实际记录的内容（子 agent 的
  `toolUseId`、工作流的 `runId`、journal 的 `result`），而不是靠猜字符串或猜时序。
- **镜头归你，历史不归你。** 录制不可变，图也绝不会在你脚下自行重排（`r` 手动整理）。
  镜头跟随动作，直到你接管它，然后它停在你放的地方。
- **零网络，可证明的。** 依赖树里没有任何 HTTP 客户端，`tokio` 也不带 `net` feature。
  `cargo tree` 就是证明。这是一个可以核验的性质，不是一句承诺。

模型、时间线和渲染都在**可移植核心**里——一个无 IO 的库，可为任何目标编译，包括
WebAssembly。原生前端（本 crate 的 `zoe` 二进制）和浏览器前端（`web/wasm/` 下的
`zoetrope-web` crate，即线上应用）架在它上面，区别只在 IO 和事件循环。

模块图与 transcript 格式见
[`docs/DESIGN.md`](https://github.com/furkankly/zoetrope/blob/main/docs/DESIGN.md)，
上述不变量的完整阐述见
[`docs/ARCHITECTURE.md`](https://github.com/furkankly/zoetrope/blob/main/docs/ARCHITECTURE.md)。

## 关于 transcript 格式

zoetrope 读取的 JSONL 格式是 Claude Code 未公开的内部格式，随时可能变动。zoetrope
的设计取向是退化而非崩溃：未识别的记录被跳过，缺失字段走回退，一行坏数据永远不会
带走整个会话。如果新版 Claude Code 让某处显示变得奇怪，请
[提一个 issue](https://github.com/furkankly/zoetrope/issues)。

## 参与贡献

欢迎提交 Pull Request。

- 本项目所有 commit 遵循 [Conventional Commits](https://www.conventionalcommits.org/)
  （例如 `feat(timeline): index the playhead by event instead of wall-clock`、
  `fix(tailer): fold appends at the live edge without rebuilding`）。变更日志由
  [git-cliff](https://github.com/orhun/git-cliff) 从中生成，不合规范的 commit 会被
  略过。
- 提 PR 前请运行 `cargo fmt`、`cargo clippy` 和 `cargo test`。
- 这些覆盖可移植核心与原生前端。浏览器前端是第二个 crate（`zoetrope-web`，位于
  `web/wasm/`），只针对 wasm32 构建，因此被排除在根 workspace 之外，任何根目录
  `cargo` 命令都不会碰它。从仓库根目录用 `bash web/scripts/build-wasm.sh` 构建它，
  用 `cd web/wasm && cargo clippy` 检查（其 `.cargo/config.toml` 已把目标默认为
  wasm32）。

## 许可证

[MIT](https://github.com/furkankly/zoetrope/blob/main/LICENSE)。

## 致谢

- [ratatui](https://github.com/ratatui/ratatui) — 终端 UI 框架
- [rataflow](https://github.com/furkankly/rataflow) — 节点流程图组件
- [ratzilla](https://github.com/ratatui/ratzilla) — WebAssembly 后端
