//! Locale-aware user-facing strings.
//!
//! The whole UI vocabulary lives here as one flat struct of `&'static str`
//! fields, with one `const` instance per [`Locale`]. Adding a language is
//! adding a variant + one `const` block: the compiler then refuses to build
//! until every string exists in the new language, so coverage is enforced
//! structurally rather than by review. This mirrors the crate's zero-dep
//! philosophy (no clap, no i18n framework — a `Strs` struct instead of a
//! runtime key-value lookup).
//!
//! Interpolated strings are templates with named `{placeholders}`, expanded
//! by [`fill`] at the call site (`format!` cannot take a runtime template).
//!
//! Strings only — code comments and identifiers stay English everywhere.

use crate::state::session::{AgentStatus, status_word_en};

/// The languages the UI speaks. `En` is the default and the fallback.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Locale {
    #[default]
    En,
    /// Simplified Chinese. Any `zh*` language tag maps here for now; a
    /// future `ZhHant` variant would take `zh-TW`/`zh-HK` in `from_tag`.
    ZhHans,
}

impl Locale {
    /// Every locale, in the order the in-app `L` key cycles through.
    pub const ALL: [Locale; 2] = [Locale::En, Locale::ZhHans];

    /// The string table for this locale.
    pub fn strs(self) -> &'static Strs {
        match self {
            Locale::En => &EN,
            Locale::ZhHans => &ZH_HANS,
        }
    }

    /// BCP-47-ish tag, as accepted by `--lang` and reported to the web page.
    pub fn tag(self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::ZhHans => "zh-Hans",
        }
    }

    /// Parse a language tag (`"en"`, `"zh"`, `"zh-CN"`, `"zh_Hans"`, …).
    /// Case-insensitive; `-` and `_` both separate subtags.
    pub fn from_tag(tag: &str) -> Option<Locale> {
        let primary = tag.split(['-', '_']).next()?.trim().to_ascii_lowercase();
        match primary.as_str() {
            "en" => Some(Locale::En),
            "zh" => Some(Locale::ZhHans),
            _ => None,
        }
    }

    /// Detect from the process environment (`LC_ALL` > `LC_MESSAGES` > `LANG`).
    /// Returns `None` when unset or unrecognized — on wasm the environment is
    /// a stub, so the browser frontend detects via `navigator.language`
    /// instead and calls [`Locale::from_tag`] itself.
    pub fn detect_env() -> Option<Locale> {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .find_map(|v| Self::from_tag(&v))
    }

    /// The next locale in the `L`-key cycle.
    pub fn next(self) -> Locale {
        let idx = Self::ALL
            .iter()
            .position(|&l| l == self)
            .unwrap_or_default();
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// The status word for this locale — the single localized entry point
    /// used by cards, the panel, and `zoe inspect` (see [`Strs`] status words).
    pub fn status_word(self, status: AgentStatus, interactive: bool) -> &'static str {
        match self {
            Locale::En => status_word_en(status, interactive),
            Locale::ZhHans => {
                use AgentStatus as S;
                match status {
                    S::Running if interactive => self.strs().word_active,
                    S::Running => self.strs().word_running,
                    S::Idle => self.strs().word_idle,
                    S::Done => self.strs().word_done,
                    S::Failed => self.strs().word_failed,
                    S::Stopped => self.strs().word_stopped,
                }
            }
        }
    }
}

/// Expand named `{placeholder}`s in a template with the given values. Missing
/// keys pass through untouched (a stale template shows the placeholder, which
/// is loud in a TUI — the failure mode of a silent empty string is worse).
pub fn fill(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in args {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

/// The complete user-facing vocabulary, one field per string. Field names are
/// grouped by surface: `word_*` (status vocabulary), `card_*`, `help_*` /
/// `sec_*` / `legend_*` (help overlay), `info_*` (session-info overlay),
/// `badge_*` / `transport_*` / `statusbar_*`, `panel_*`, `cli_*` / `err_*`
/// (the `zoe` binary), `inspect_*` / `kind_*`.
pub struct Strs {
    // ── status vocabulary (cards, panel, inspect — single source) ──
    pub word_active: &'static str,
    pub word_running: &'static str,
    pub word_idle: &'static str,
    pub word_done: &'static str,
    pub word_failed: &'static str,
    pub word_stopped: &'static str,

    // ── agent card (ui/nodes.rs) ──
    /// The unit word in `⚒ N <word>` (no last tool to show).
    pub card_tools_word: &'static str,
    /// The token unit in the card footer (`1.2k <unit>`).
    pub card_tok_unit: &'static str,

    // ── help overlay (ui/mod.rs) ──
    pub help_title: &'static str,
    pub help_close: &'static str,
    /// Quit hint on wasm — a browser tab can't close itself.
    pub help_quit_web: &'static str,
    pub sec_camera: &'static str,
    pub sec_layout: &'static str,
    pub sec_navigate: &'static str,
    pub sec_viewport: &'static str,
    pub sec_panel: &'static str,
    pub sec_timeline: &'static str,
    pub sec_pacing: &'static str,
    pub sec_info: &'static str,
    pub sec_replay: &'static str,
    pub sec_language: &'static str,
    pub sec_status: &'static str,
    pub sec_quit: &'static str,
    pub help_camera: &'static str,
    pub help_layout: &'static str,
    pub help_navigate: &'static str,
    pub help_viewport: &'static str,
    pub help_panel: &'static str,
    /// Timeline help, split around the `◆` prompt glyph.
    pub help_timeline_a: &'static str,
    pub help_timeline_b: &'static str,
    pub help_pacing: &'static str,
    pub help_info: &'static str,
    pub help_replay: &'static str,
    pub help_language: &'static str,
    pub legend_active_running: &'static str,
    pub legend_idle: &'static str,
    pub legend_done: &'static str,
    pub legend_failed: &'static str,
    pub legend_edges: &'static str,

    // ── session-info overlay (ui/mod.rs) ──
    pub info_heading: &'static str,
    pub info_close: &'static str,
    pub info_title: &'static str,
    pub info_perms: &'static str,
    pub info_mode: &'static str,
    pub info_queued: &'static str,
    pub info_last: &'static str,
    /// `{q}` queued ops · `{f}` file edits.
    pub info_queued_edits: &'static str,

    // ── transport badges: status bar ──
    pub badge_live: &'static str,
    pub badge_play: &'static str,
    pub badge_pause: &'static str,
    pub badge_past: &'static str,
    pub badge_idle: &'static str,
    // ── transport tags: scrubber info row ──
    pub transport_live: &'static str,
    pub transport_play: &'static str,
    pub transport_pause: &'static str,
    pub transport_history: &'static str,
    pub transport_end: &'static str,

    // ── status bar ──
    pub statusbar_untitled: &'static str,
    /// `{a}` agents · `{t}` tools.
    pub statusbar_counts: &'static str,
    pub statusbar_overview: &'static str,
    pub statusbar_follow: &'static str,
    pub statusbar_quit: &'static str,
    pub statusbar_help: &'static str,
    pub statusbar_pause: &'static str,

    // ── detail panel (ui/panel.rs) ──
    /// The auto-follow label in the scroll indicator (`j/k ↕ <word>`).
    pub panel_tail: &'static str,
    pub panel_no_detail: &'static str,
    /// `{n}` tools · `{k}` tokens.
    pub panel_counts: &'static str,
    pub panel_triggered_by: &'static str,
    pub panel_prompt_label: &'static str,
    pub panel_thought_label: &'static str,
    pub panel_no_tool_calls: &'static str,
    pub panel_tool_calls: &'static str,
    /// `{t}` = local time the agent first appeared.
    pub panel_started: &'static str,

    // ── CLI (main.rs) ──
    pub cli_usage: &'static str,
    pub err_inspect_requires: &'static str,
    pub err_inspect_single: &'static str,
    pub err_speed_requires: &'static str,
    pub err_lang_requires: &'static str,
    /// `{v}` = the raw argument.
    pub err_speed_invalid: &'static str,
    pub err_speed_positive: &'static str,
    pub err_unknown_flag: &'static str,
    pub err_single_path: &'static str,
    pub err_unknown_lang: &'static str,
    pub err_not_readable: &'static str,
    /// `{p}` = the path.
    pub err_reading: &'static str,

    // ── inspect output (main.rs) ──
    /// `{id}` session id, `{title}` session title.
    pub inspect_session: &'static str,
    pub inspect_untitled: &'static str,
    /// `{m}` mode, `{p}` permission.
    pub inspect_mode_perm: &'static str,
    /// `{a}` agents, `{t}` tool calls, `{q}` queued, `{f}` file edits.
    pub inspect_summary: &'static str,
    /// `{p}` = the last prompt (already debug-quoted by the caller).
    pub inspect_last_prompt: &'static str,
    pub kind_main: &'static str,
    pub kind_subagent: &'static str,
    pub kind_workflow: &'static str,
    /// `{m}` = model name.
    pub inspect_model: &'static str,
    /// `{n}` total, `{ok}`/`{err}`/`{pend}` tallies, `{k}` tokens.
    pub inspect_tools: &'static str,
    pub inspect_prompt: &'static str,
    pub inspect_thought: &'static str,
}

const EN: Strs = Strs {
    word_active: "active",
    word_running: "running",
    word_idle: "idle",
    word_done: "done",
    word_failed: "failed",
    word_stopped: "stopped",

    card_tools_word: "tools",
    card_tok_unit: "tok",

    help_title: " zoetrope — keys ",
    help_close: " ? or esc to close ",
    help_quit_web: "close the browser tab",
    sec_camera: "camera",
    sec_layout: "layout",
    sec_navigate: "navigate",
    sec_viewport: "viewport",
    sec_panel: "panel",
    sec_timeline: "timeline",
    sec_pacing: "pacing",
    sec_info: "info",
    sec_replay: "replay",
    sec_language: "language",
    sec_status: "status",
    sec_quit: "quit",
    help_camera: "o overview · f follow · pan/zoom = manual",
    help_layout: "r rearrange the graph",
    help_navigate: "tab / shift-tab cycle · ↑↓←→ · click select",
    help_viewport: "h j k l pan · + - zoom · 0 reset · c center",
    help_panel: "j/k · pgup/pgdn scroll · esc close",
    help_timeline_a: "[ ] step prompts ",
    help_timeline_b: " · End/g live · drag to seek",
    help_pacing: "s skip idle gaps (on = »; off = real-time)",
    help_info: "i session details (mode, prompts, …)",
    help_replay: "space pause/resume",
    help_language: "L toggle language",
    legend_active_running: "active/running",
    legend_idle: "idle",
    legend_done: "done",
    legend_failed: "failed",
    legend_edges: "green edges = agent running",

    info_heading: " session ",
    info_close: " i / esc to close ",
    info_title: "title",
    info_perms: "perms",
    info_mode: "mode",
    info_queued: "queued",
    info_last: "last",
    info_queued_edits: "{q} · {f} file edits",

    badge_live: " ● LIVE ",
    badge_play: " ▶ PLAY ",
    badge_pause: " ⏸ PAUSE ",
    badge_past: " ⏮ PAST ",
    badge_idle: " ■ IDLE ",
    transport_live: "● LIVE",
    transport_play: "▶ play",
    transport_pause: "⏸ paused",
    transport_history: "⏮ history",
    transport_end: "■ end",

    statusbar_untitled: "session",
    statusbar_counts: "  {a} agents · {t} tools",
    statusbar_overview: "  ⌖ overview",
    statusbar_follow: "  ⌖ follow",
    statusbar_quit: "q quit · ",
    statusbar_help: "? help",
    statusbar_pause: "space pause",

    panel_tail: "tail",
    panel_no_detail: "no detail for this agent",
    panel_counts: "{n} tools · {k} tok",
    panel_triggered_by: "─ triggered by ",
    panel_prompt_label: "↳ prompt",
    panel_thought_label: "↳ thought",
    panel_no_tool_calls: "no tool calls",
    panel_tool_calls: " tool calls ",
    panel_started: "⏱ started {t}",

    cli_usage: "\
zoetrope — visualize Claude Code agent sessions as a flow graph

USAGE:
    zoe                     follow the current project's live session
    zoe <file.jsonl>        replay a recording, played from the start
    zoe <dir>               follow another project's live session
    zoe <file> --follow     follow a file's live edge instead of replaying
    zoe <file> --speed N    playback speed (default 8.0)
    zoe --lang <en|zh>      UI language (default: follow the system locale)
    zoe inspect <file>      headless: print the session tree + info
    zoe --version           print the version and exit

Once open, scrub/follow/pause/go-live are available no matter how you launched.",
    err_inspect_requires: "inspect requires a <file.jsonl>",
    err_inspect_single: "inspect takes a single file argument",
    err_speed_requires: "--speed requires a number",
    err_lang_requires: "--lang requires a value (en or zh)",
    err_speed_invalid: "invalid --speed value: {v}",
    err_speed_positive: "--speed must be a positive number, got {v}",
    err_unknown_flag: "unknown flag {v}",
    err_single_path: "expected a single path argument",
    err_unknown_lang: "unknown --lang value: {v} (expected en or zh)",
    err_not_readable: "not a readable file: {p}",
    err_reading: "reading transcript {p}",

    inspect_session: "session {id} — {title}",
    inspect_untitled: "(untitled)",
    inspect_mode_perm: "  mode: {m} · permission: {p}",
    inspect_summary: "  {a} agent(s), {t} tool call(s) · {q} queued · {f} file edit(s)",
    inspect_last_prompt: "  last prompt: {p}",
    kind_main: "main",
    kind_subagent: "subagent",
    kind_workflow: "workflow",
    inspect_model: "    model: {m}",
    inspect_tools: "    tools: {n} ({ok}✓ {err}✗ {pend}⏳)   tokens: {k}",
    inspect_prompt: "    ↳ prompt: {p}",
    inspect_thought: "    ↳ thought: {p}",
};

const ZH_HANS: Strs = Strs {
    word_active: "活跃",
    word_running: "运行中",
    word_idle: "空闲",
    word_done: "已完成",
    word_failed: "失败",
    word_stopped: "已停止",

    card_tools_word: "个工具",
    card_tok_unit: "token",

    help_title: " zoetrope — 按键 ",
    help_close: " ? 或 esc 关闭 ",
    help_quit_web: "关闭浏览器标签页",
    sec_camera: "镜头",
    sec_layout: "布局",
    sec_navigate: "导航",
    sec_viewport: "视口",
    sec_panel: "面板",
    sec_timeline: "时间线",
    sec_pacing: "节奏",
    sec_info: "信息",
    sec_replay: "回放",
    sec_language: "语言",
    sec_status: "状态",
    sec_quit: "退出",
    help_camera: "o 总览 · f 跟随 · 平移/缩放 = 手动",
    help_layout: "r 重新整理图表",
    help_navigate: "tab / shift-tab 切换 · ↑↓←→ · 点击选择",
    help_viewport: "h j k l 平移 · + - 缩放 · 0 复位 · c 居中",
    help_panel: "j/k · pgup/pgdn 滚动 · esc 关闭",
    help_timeline_a: "[ ] 切换提示词 ",
    help_timeline_b: " · End/g 回到直播 · 拖动进度条定位",
    help_pacing: "s 跳过空闲段（开 = »；关 = 实时）",
    help_info: "i 会话详情（模式、提示词…）",
    help_replay: "space 暂停/继续",
    help_language: "L 切换语言",
    legend_active_running: "活跃/运行中",
    legend_idle: "空闲",
    legend_done: "已完成",
    legend_failed: "失败",
    legend_edges: "绿色连线 = agent 运行中",

    info_heading: " 会话 ",
    info_close: " i / esc 关闭 ",
    info_title: "标题",
    info_perms: "权限",
    info_mode: "模式",
    info_queued: "队列",
    info_last: "最近",
    info_queued_edits: "{q} · {f} 次文件编辑",

    badge_live: " ● 直播 ",
    badge_play: " ▶ 播放 ",
    badge_pause: " ⏸ 暂停 ",
    badge_past: " ⏮ 回看 ",
    badge_idle: " ■ 空闲 ",
    transport_live: "● 直播",
    transport_play: "▶ 播放",
    transport_pause: "⏸ 已暂停",
    transport_history: "⏮ 回看",
    transport_end: "■ 结束",

    statusbar_untitled: "会话",
    statusbar_counts: "  {a} 个 agent · {t} 个工具",
    statusbar_overview: "  ⌖ 总览",
    statusbar_follow: "  ⌖ 跟随",
    statusbar_quit: "q 退出 · ",
    statusbar_help: "? 帮助",
    statusbar_pause: "space 暂停",

    panel_tail: "跟随",
    panel_no_detail: "该 agent 暂无详情",
    panel_counts: "{n} 个工具 · {k} token",
    panel_triggered_by: "─ 触发来源 ",
    panel_prompt_label: "↳ 提示词",
    panel_thought_label: "↳ 思考",
    panel_no_tool_calls: "无工具调用",
    panel_tool_calls: " 工具调用 ",
    panel_started: "⏱ 开始于 {t}",

    cli_usage: "\
zoetrope — 将 Claude Code agent 会话可视化为流程图

用法：
    zoe                     跟随当前项目的实时会话
    zoe <file.jsonl>        回放一段录制，从开头开始
    zoe <dir>               跟随另一个项目的实时会话
    zoe <file> --follow     跟随文件的实时边缘（而非回放）
    zoe <file> --speed N    回放倍速（默认 8.0）
    zoe --lang <en|zh>      界面语言（默认跟随系统语言）
    zoe inspect <file>      无界面：打印会话树与信息
    zoe --version           打印版本号并退出

打开之后，无论以何种方式启动，都可以拖动进度条、跟随、暂停、回到直播。",
    err_inspect_requires: "inspect 需要一个 <file.jsonl> 文件",
    err_inspect_single: "inspect 只接受一个文件参数",
    err_speed_requires: "--speed 需要一个数值",
    err_lang_requires: "--lang 需要一个值（en 或 zh）",
    err_speed_invalid: "无效的 --speed 值：{v}",
    err_speed_positive: "--speed 必须为正数，收到 {v}",
    err_unknown_flag: "未知参数 {v}",
    err_single_path: "只接受一个路径参数",
    err_unknown_lang: "未知 --lang 值：{v}（可用：en、zh）",
    err_not_readable: "文件不可读：{p}",
    err_reading: "读取 transcript {p}",

    inspect_session: "会话 {id} — {title}",
    inspect_untitled: "（无标题）",
    inspect_mode_perm: "  模式：{m} · 权限：{p}",
    inspect_summary: "  {a} 个 agent、{t} 次工具调用 · {q} 个排队操作 · {f} 次文件编辑",
    inspect_last_prompt: "  最近提示：{p}",
    kind_main: "主会话",
    kind_subagent: "子agent",
    kind_workflow: "工作流",
    inspect_model: "    模型：{m}",
    inspect_tools: "    工具：{n}（{ok}✓ {err}✗ {pend}⏳）   token：{k}",
    inspect_prompt: "    ↳ 提示词：{p}",
    inspect_thought: "    ↳ 思考：{p}",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_tag_parses_primary_subtag() {
        assert_eq!(Locale::from_tag("en"), Some(Locale::En));
        assert_eq!(Locale::from_tag("en_US.UTF-8"), Some(Locale::En));
        assert_eq!(Locale::from_tag("EN"), Some(Locale::En));
        assert_eq!(Locale::from_tag("zh"), Some(Locale::ZhHans));
        assert_eq!(Locale::from_tag("zh-CN"), Some(Locale::ZhHans));
        assert_eq!(Locale::from_tag("zh_Hans"), Some(Locale::ZhHans));
        assert_eq!(Locale::from_tag("ZH_TW"), Some(Locale::ZhHans));
        assert_eq!(Locale::from_tag(""), None);
        assert_eq!(Locale::from_tag("fr"), None);
        assert_eq!(Locale::from_tag("  "), None);
    }

    #[test]
    fn next_cycles_all_locales() {
        assert_eq!(Locale::En.next(), Locale::ZhHans);
        assert_eq!(Locale::ZhHans.next(), Locale::En);
    }

    #[test]
    fn fill_expands_named_placeholders() {
        assert_eq!(fill("{a} × {b}", &[("a", "3"), ("b", "x")]), "3 × x");
        // Missing key passes through loudly.
        assert_eq!(fill("{a} {missing}", &[("a", "3")]), "3 {missing}");
        // No placeholders at all.
        assert_eq!(fill("plain text", &[("a", "3")]), "plain text");
    }

    #[test]
    fn every_locale_string_is_non_empty() {
        // Field presence is compile-enforced; emptiness is not, so a blank
        // translation must fail here rather than render as a missing label.
        for locale in Locale::ALL {
            let s = locale.strs();
            macro_rules! check {
                ($($field:ident),+ $(,)?) => {
                    for (name, value) in [$((stringify!($field), s.$field)),+] {
                        assert!(
                            !value.trim().is_empty(),
                            "{}: {:?} is empty",
                            locale.tag(),
                            name
                        );
                    }
                };
            }
            check!(
                word_active,
                word_running,
                word_idle,
                word_done,
                word_failed,
                word_stopped,
                card_tools_word,
                card_tok_unit,
                help_title,
                help_close,
                help_quit_web,
                sec_camera,
                sec_layout,
                sec_navigate,
                sec_viewport,
                sec_panel,
                sec_timeline,
                sec_pacing,
                sec_info,
                sec_replay,
                sec_language,
                sec_status,
                sec_quit,
                help_camera,
                help_layout,
                help_navigate,
                help_viewport,
                help_panel,
                help_timeline_a,
                help_timeline_b,
                help_pacing,
                help_info,
                help_replay,
                help_language,
                legend_active_running,
                legend_idle,
                legend_done,
                legend_failed,
                legend_edges,
                info_heading,
                info_close,
                info_title,
                info_perms,
                info_mode,
                info_queued,
                info_last,
                info_queued_edits,
                badge_live,
                badge_play,
                badge_pause,
                badge_past,
                badge_idle,
                transport_live,
                transport_play,
                transport_pause,
                transport_history,
                transport_end,
                statusbar_untitled,
                statusbar_counts,
                statusbar_overview,
                statusbar_follow,
                statusbar_quit,
                statusbar_help,
                statusbar_pause,
                panel_tail,
                panel_no_detail,
                panel_counts,
                panel_triggered_by,
                panel_prompt_label,
                panel_thought_label,
                panel_no_tool_calls,
                panel_tool_calls,
                panel_started,
                cli_usage,
                err_inspect_requires,
                err_inspect_single,
                err_speed_requires,
                err_lang_requires,
                err_speed_invalid,
                err_speed_positive,
                err_unknown_flag,
                err_single_path,
                err_unknown_lang,
                err_not_readable,
                err_reading,
                inspect_session,
                inspect_untitled,
                inspect_mode_perm,
                inspect_summary,
                inspect_last_prompt,
                kind_main,
                kind_subagent,
                kind_workflow,
                inspect_model,
                inspect_tools,
                inspect_prompt,
                inspect_thought,
            );
        }
    }

    #[test]
    fn status_words_are_translated() {
        use crate::state::session::AgentStatus as S;
        assert_eq!(Locale::En.status_word(S::Running, false), EN.word_running);
        assert_eq!(Locale::En.status_word(S::Running, true), "active");
        assert_eq!(Locale::ZhHans.status_word(S::Running, false), "运行中");
        assert_eq!(Locale::ZhHans.status_word(S::Running, true), "活跃");
        assert_eq!(Locale::ZhHans.status_word(S::Stopped, false), "已停止");
    }
}
