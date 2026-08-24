//! [`AgentNode`]: the `NodeContent` for an agent card.
//!
//! Renders a bordered card showing the agent type, a status glyph + color,
//! a truncated description, a tool count, the last tool name, and an output
//! token count. Visual state (status, tool tallies) is mirrored from the
//! domain model into the node via `flow.node_content_mut` on each sync, so the
//! card needs no back-reference to the `SessionModel`.

use rataflow::{NodeContent, NodeRenderContext};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Widget};

use crate::i18n::Locale;
use crate::state::session::AgentStatus;

/// Fixed card dimensions for main / workflow nodes (world units).
pub const MAIN_NODE_DIMS: (f64, f64) = (30.0, 7.0);
/// Fixed card dimensions for subagent nodes (world units).
pub const SUB_NODE_DIMS: (f64, f64) = (26.0, 6.0);

/// Below this on-screen size a card has no room for any text — it renders at
/// cell level instead (solid status-colored fill). Semantic zoom: zoomed out,
/// nodes show as status cells instead of empty bordered boxes.
pub const CELL_MIN_WIDTH: u16 = 10;
/// See [`CELL_MIN_WIDTH`].
pub const CELL_MIN_HEIGHT: u16 = 3;

/// Custom node content rendered as an agent card.
///
/// Plain `pub` fields: the graph layer reads them back and mutates them in
/// place during incremental sync (mirroring [`crate::state::session::AgentInfo`]).
#[derive(Debug, Clone)]
pub struct AgentNode {
    /// Title line — agent type, or `"claude"` for the main agent.
    pub title: String,
    /// Truncated description shown under the title.
    pub description: Option<String>,
    pub status: AgentStatus,
    /// Number of tool calls (for the `⚒ N tools` line).
    pub tool_count: usize,
    /// Name of the most recent tool call, if any.
    pub last_tool: Option<String>,
    pub output_tokens: u64,
    /// Interactive agents (main, forks) word `Running` as "active": we know
    /// there are recent entries, not that a task is executing.
    pub interactive: bool,
    /// UI language for the card's own words (status, tools, token unit). The
    /// render context carries no App reference, so the locale is stamped onto
    /// the content at graph sync — and refreshed in place by
    /// [`App::set_locale`](crate::state::App::set_locale).
    pub locale: Locale,
}

use crate::ui::truncate;

/// Compact human token count, e.g. `1.2k`, `34`, `2.0M`.
fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

impl NodeContent for AgentNode {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        let palette = ctx.theme.palette();
        let area = ctx.area;

        // Degenerate areas: a border needs at least 2x2; bail out gracefully.
        if area.width == 0 || area.height == 0 {
            return;
        }

        // Single-source vocabulary + presence colors (cards/panel/inspect
        // share them; only the pulse override below is card-specific).
        let glyph_color = crate::ui::status_color(self.status, &palette);
        let status_text = self.locale.status_word(self.status, self.interactive);

        // Cell level (semantic zoom): too small for any text — a bordered card
        // carries no information here, so paint a solid status-colored block.
        if area.width < CELL_MIN_WIDTH || area.height < CELL_MIN_HEIGHT {
            let mut fill = Style::default().bg(glyph_color);
            if ctx.selected {
                fill = fill.add_modifier(Modifier::REVERSED);
            }
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    buf[(x, y)].set_char(' ').set_style(fill);
                }
            }
            return;
        }

        let border_color = if ctx.selected {
            palette.accent
        } else {
            palette.muted
        };
        let bg_style = Style::default().bg(palette.surface);
        let border_style = bg_style.fg(border_color);

        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .style(bg_style);

        // Only pad when there's horizontal room to spare.
        if area.width >= 4 {
            block = block.padding(Padding::horizontal(1));
        }

        let inner = block.inner(area);
        block.render(area, buf);

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // Pulse: alive agents breathe on the shared animation clock (~1s
        // cycle at the default 120ms phase step) — the wide-shot heartbeat.
        let glyph = if self.status == AgentStatus::Running && (ctx.animation_phase / 4) % 2 == 1 {
            '○'
        } else {
            self.status.glyph()
        };
        let inner_w = inner.width as usize;

        // Title row: status glyph + agent type, bold.
        let title_budget = inner_w.saturating_sub(2); // glyph + space
        let title = truncate(&self.title, title_budget);
        let title_line = Line::from(vec![
            Span::styled(format!("{glyph} "), bg_style.fg(glyph_color)),
            Span::styled(
                title,
                bg_style.fg(palette.text).add_modifier(Modifier::BOLD),
            ),
        ]);

        // Build the candidate lines in priority order.
        let mut lines: Vec<Line> = Vec::new();
        lines.push(title_line);

        if let Some(desc) = self.description.as_ref().filter(|d| !d.is_empty()) {
            let desc = truncate(desc, inner_w);
            lines.push(Line::from(Span::styled(desc, bg_style.fg(palette.subtle))));
        }

        // Tools row: "⚒ N · last_tool".
        let tools_text = if let Some(last) = self.last_tool.as_ref() {
            let prefix = format!("⚒ {} · ", self.tool_count);
            let budget =
                inner_w.saturating_sub(unicode_width::UnicodeWidthStr::width(prefix.as_str()));
            format!("{prefix}{}", truncate(last, budget))
        } else {
            format!(
                "⚒ {} {}",
                self.tool_count,
                self.locale.strs().card_tools_word
            )
        };
        lines.push(Line::from(Span::styled(
            tools_text,
            bg_style.fg(palette.accent),
        )));

        // Footer row: status word + token count, separated to the edges.
        let tokens = fmt_tokens(self.output_tokens);
        let tok_unit = self.locale.strs().card_tok_unit;
        let footer = if self.output_tokens > 0 {
            Line::from(vec![
                Span::styled(status_text, bg_style.fg(glyph_color)),
                Span::styled(format!("  {tokens} {tok_unit}"), bg_style.fg(palette.muted)),
            ])
        } else {
            Line::from(Span::styled(status_text, bg_style.fg(glyph_color)))
        };
        lines.push(footer);

        // Each row is one cell tall, stacked from the top; render only what fits.
        for (i, line) in lines.into_iter().enumerate() {
            if i as u16 >= inner.height {
                break;
            }
            let rect = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
            Paragraph::new(line).style(bg_style).render(rect, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_budget() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn truncate_handles_multibyte() {
        // Must not panic on a char boundary; counts columns not bytes.
        let s = "café crème brûlée";
        let out = truncate(s, 5);
        assert_eq!(out.chars().count(), 5);
    }

    #[test]
    fn truncate_measures_display_columns_not_chars() {
        use unicode_width::UnicodeWidthStr;
        // CJK chars are 2 columns wide: 6 chars = 12 columns must truncate to
        // fit a 10-column budget (chars-based counting would pass it through
        // and overflow the card).
        let s = "修复解析错误";
        let out = truncate(s, 10);
        assert!(out.width() <= 10, "{out:?} is {} columns", out.width());
        assert!(out.ends_with('…'));
        // And an exact fit is left alone.
        assert_eq!(truncate(s, 12), s);
    }

    #[test]
    fn token_formatting() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_500), "1.5k");
        assert_eq!(fmt_tokens(2_000_000), "2.0M");
    }

    #[test]
    fn glyphs_per_status() {
        assert_eq!(AgentStatus::Running.glyph(), '●');
        assert_eq!(AgentStatus::Done.glyph(), '✓');
        assert_eq!(AgentStatus::Failed.glyph(), '✗');
    }

    fn render_into(area: ratatui::layout::Rect) -> Buffer {
        use rataflow::Theme;
        use rataflow::types::Position;

        let node = AgentNode {
            title: "claude".into(),
            description: None,
            status: AgentStatus::Done,
            tool_count: 3,
            last_tool: Some("Bash".into()),
            output_tokens: 1200,
            interactive: false,
            locale: Locale::En,
        };
        let ctx = NodeRenderContext {
            id: "main",
            area,
            selected: false,
            dragging: false,
            position_absolute: Position::new(0.0, 0.0),
            theme: Theme::default(),
            animation_phase: 0,
        };
        let mut buf = Buffer::empty(area);
        node.render(&ctx, &mut buf);
        buf
    }

    #[test]
    fn card_level_renders_text() {
        let area = ratatui::layout::Rect::new(0, 0, 24, 6);
        let buf = render_into(area);
        let text: String = (0..area.width).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(
            text.contains("claude"),
            "card level must show title: {text}"
        );
    }

    #[test]
    fn cell_level_renders_status_fill() {
        use rataflow::Theme;
        use ratatui::style::Color;

        // Below the text threshold: solid status-colored fill, no border.
        let area = ratatui::layout::Rect::new(0, 0, 6, 2);
        let buf = render_into(area);
        let palette = Theme::default().palette();
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &buf[(x, y)];
                assert_eq!(cell.symbol(), " ", "cell level draws no text/border");
                assert_eq!(cell.style().bg, Some(palette.accent)); // Done = calm accent
                assert_ne!(cell.style().bg, Some(Color::Reset));
            }
        }
    }
}
