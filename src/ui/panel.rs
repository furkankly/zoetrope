//! Detail panel for the selected agent.
//!
//! When an agent node is selected, the main area splits 30/70 and this panel
//! renders the selected agent's description, model, status, timing, and a
//! scrollable list of recent tool calls (name + summary + ✓/✗/⏳). All data
//! comes from the `SessionModel`, keyed by the selected node id.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

use crate::i18n::{Locale, fill};
use crate::state::App;
use crate::state::session::{AgentInfo, ToolState};
use crate::ui::{pad_width, truncate, truncate_tail, wrap};

/// Line cap for the prompt in the **provenance** header only — it sits in a
/// fixed-height region, so an unbounded prompt would starve the tool list. The
/// era anchors in the scrollable list are NOT capped. Generous: a normal prompt
/// fits well within it (the upstream ~240-char excerpt bounds the raw length).
const PROMPT_MAX_LINES: usize = 6;

/// Cached era-header flags for the selected agent's tool list. Attribution is
/// O(calls × prompts) (`prompt_for_ts` per call), and the panel renders every
/// frame — so recompute only when the agent, its call count, or the prompt
/// count changes (all three are append-only between rebuilds).
pub(crate) struct EraCache {
    agent_id: String,
    calls: usize,
    prompts: usize,
    flags: Vec<bool>,
    total: usize,
}

/// Get-or-recompute the era cache for `agent_id`.
fn era_flags<'a>(
    cache: &'a mut Option<EraCache>,
    agent_id: &str,
    agent: &AgentInfo,
    model: &crate::state::session::SessionModel,
) -> &'a EraCache {
    let stale = cache.as_ref().is_none_or(|c| {
        c.agent_id != agent_id
            || c.calls != agent.tool_calls.len()
            || c.prompts != model.prompts.len()
    });
    if stale {
        let (flags, total) = era_header_flags(agent, model);
        *cache = Some(EraCache {
            agent_id: agent_id.to_string(),
            calls: agent.tool_calls.len(),
            prompts: model.prompts.len(),
            flags,
            total,
        });
    }
    cache.as_ref().unwrap()
}

/// Render the detail panel for `agent_id` into `area`.
///
/// `agent_id` is copied out of the flow before this call to avoid borrowing
/// `app` both immutably (selection) and the panel state. Uses
/// `app.detail_scroll` for the tool-call list scroll offset.
pub fn render(frame: &mut Frame, area: Rect, app: &mut App, agent_id: &str) {
    let palette = app.flow.theme.palette();
    // Split the borrows up front: the era cache is written while the session is
    // read, which a whole-`app` borrow would forbid.
    let App {
        session,
        era_cache,
        detail_scroll,
        detail_follow,
        locale,
        ..
    } = app;
    let bg = Style::default().bg(palette.surface);

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.muted).bg(palette.surface))
        .style(bg)
        .padding(Padding::horizontal(1))
        // Affordance: the way out is visible, not tribal knowledge.
        .title_top(
            Line::from(" esc ✕ ")
                .right_aligned()
                .style(bg.fg(palette.subtle)),
        );
    // Scroll indicator once the list is plausibly taller than the panel.
    if let Some(a) = session.agent(agent_id) {
        let n = era_flags(era_cache, agent_id, a, session).total;
        if n > 8 {
            // "tail" while auto-following the newest call; the line offset once
            // the user has scrolled up (detached).
            let label = if *detail_follow {
                format!(" j/k ↕ {} ", locale.strs().panel_tail)
            } else {
                format!(" j/k ↕ {}/{} ", detail_scroll, n)
            };
            block = block.title_bottom(
                Line::from(label)
                    .right_aligned()
                    .style(bg.fg(palette.subtle)),
            );
        }
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(agent) = session.agent(agent_id) else {
        // Selected node has no model entry (stale selection) — show a hint.
        let para = Paragraph::new(Line::from(Span::styled(
            locale.strs().panel_no_detail,
            Style::default().fg(palette.muted),
        )))
        .style(Style::default().bg(palette.surface));
        frame.render_widget(para, inner);
        return;
    };

    // Split: header (fixed), provenance when known (sized to its lines),
    // tools (fill). The prompt is DERIVED from the spawn timestamp's era —
    // same order-independent attribution as the tool-list headers.
    let provenance = session.provenance(agent).and_then(|c| {
        let prompt = session.provenance_prompt(c).map(str::to_string);
        let reasoning = c.reasoning.clone();
        (prompt.is_some() || reasoning.is_some()).then_some((prompt, reasoning))
    });
    // Prompts are wrapped (not cut) — they're the panel's highest-signal text.
    // Wrap up front so the layout can size the provenance block to fit.
    let prov_text_w = (inner.width as usize).saturating_sub(10);
    let prov_prompt: Vec<String> = provenance
        .as_ref()
        .and_then(|(p, _)| p.as_deref())
        .map(|p| wrap(p, prov_text_w, PROMPT_MAX_LINES))
        .unwrap_or_default();
    let prov_thought: Vec<String> = provenance
        .as_ref()
        .and_then(|(_, r)| r.as_deref())
        .map(|r| wrap(r, prov_text_w, PROMPT_MAX_LINES))
        .unwrap_or_default();
    let prov_rows = if provenance.is_some() {
        (1 + prov_prompt.len() + prov_thought.len()) as u16
    } else {
        0
    };
    let [header_area, prov_area, tools_area] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Length(prov_rows),
        Constraint::Fill(1),
    ])
    .areas(inner);

    render_header(frame, header_area, agent, &palette, *locale);
    if provenance.is_some() {
        render_provenance(
            frame,
            prov_area,
            &prov_prompt,
            &prov_thought,
            &palette,
            *locale,
        );
    }
    // The panel auto-tails the newest call by default; scrolling up detaches it
    // (its own state, independent of the graph camera). The renderer clamps the
    // offset to the real maximum and writes it (+ the re-attach) back.
    render_tools(
        frame,
        tools_area,
        agent_id,
        agent,
        session,
        era_cache,
        detail_scroll,
        detail_follow,
        &palette,
        *locale,
    );
}

fn render_header(
    frame: &mut Frame,
    area: Rect,
    agent: &AgentInfo,
    palette: &rataflow::Palette,
    locale: Locale,
) {
    // Single-source vocabulary + presence colors (shared with cards/inspect).
    let status_text = agent.status_word(locale);
    let status_color = crate::ui::status_color(agent.status, palette);

    let bg = Style::default().bg(palette.surface);

    let mut lines: Vec<Line> = Vec::new();

    // Title: agent type, bold.
    let title = agent.agent_type.as_deref().unwrap_or("claude");
    lines.push(Line::from(Span::styled(
        title,
        bg.fg(palette.text).add_modifier(Modifier::BOLD),
    )));

    // Status + model.
    let mut status_spans = vec![Span::styled(status_text, bg.fg(status_color))];
    if let Some(model) = agent.model.as_ref() {
        status_spans.push(Span::styled("  ", bg));
        status_spans.push(Span::styled(model.as_str(), bg.fg(palette.subtle)));
    }
    lines.push(Line::from(status_spans));

    // Timing: duration if both ends known, else first seen.
    if let Some(timing) = fmt_timing(agent, locale) {
        lines.push(Line::from(Span::styled(timing, bg.fg(palette.muted))));
    }

    // Counts: tools + tokens.
    lines.push(Line::from(Span::styled(
        fill(
            locale.strs().panel_counts,
            &[
                ("n", &agent.tool_calls.len().to_string()),
                ("k", &agent.output_tokens.to_string()),
            ],
        ),
        bg.fg(palette.muted),
    )));

    // Description (wrapped) on the remaining rows.
    if let Some(desc) = agent.description.as_ref().filter(|d| !d.is_empty()) {
        lines.push(Line::from(Span::styled(
            desc.as_str(),
            bg.fg(palette.subtle),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines).style(bg).wrap(Wrap { trim: true }),
        area,
    );
}

/// "Why does this agent exist": the triggering prompt + the assistant's
/// reasoning right before the spawn.
fn render_provenance(
    frame: &mut Frame,
    area: Rect,
    prompt: &[String],
    reasoning: &[String],
    palette: &rataflow::Palette,
    locale: Locale,
) {
    if area.height == 0 {
        return;
    }
    let bg = Style::default().bg(palette.surface);
    let label = bg.fg(palette.accent);
    let width = area.width as usize;

    let block = Style::default().bg(palette.muted);

    let triggered_by = locale.strs().panel_triggered_by;
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        triggered_by.to_string()
            + &"─"
                .repeat(width.saturating_sub(unicode_width::UnicodeWidthStr::width(triggered_by))),
        bg.fg(palette.muted),
    ))];
    // Label width in columns — the continuation indent and the prompt/thought
    // labels must line up, and a CJK label is wider per char than a Latin one.
    let prompt_label = locale.strs().panel_prompt_label;
    let thought_label = locale.strs().panel_thought_label;
    let label_w = unicode_width::UnicodeWidthStr::width(prompt_label)
        .max(unicode_width::UnicodeWidthStr::width(thought_label));
    let indent = " ".repeat(label_w);
    // The user's prompt: a label on the first line, continuations indented, all
    // on a full-width subtle GRAY block — a quiet anchor, since gold is reserved
    // for agent activity/focus, not context.
    for (i, l) in prompt.iter().enumerate() {
        let prefix = if i == 0 {
            pad_width(prompt_label, label_w)
        } else {
            indent.clone()
        };
        let pad = width.saturating_sub(
            unicode_width::UnicodeWidthStr::width(prefix.as_str())
                + unicode_width::UnicodeWidthStr::width(l.as_str()),
        );
        lines.push(Line::from(vec![
            Span::styled(prefix, block.fg(palette.accent)),
            Span::styled(l.clone(), block.fg(palette.text)),
            Span::styled(" ".repeat(pad), block),
        ]));
    }
    // The assistant's reasoning: dim, no highlight (not the user's words).
    for (i, l) in reasoning.iter().enumerate() {
        let prefix = if i == 0 {
            pad_width(thought_label, label_w)
        } else {
            indent.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, label),
            Span::styled(l.clone(), bg.fg(palette.subtle)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).style(bg), area);
}

#[allow(clippy::too_many_arguments)]
fn render_tools(
    frame: &mut Frame,
    area: Rect,
    agent_id: &str,
    agent: &AgentInfo,
    model: &crate::state::session::SessionModel,
    era_cache: &mut Option<EraCache>,
    detail_scroll: &mut u16,
    detail_follow: &mut bool,
    palette: &rataflow::Palette,
    locale: Locale,
) {
    let bg = Style::default().bg(palette.surface);

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(bg.fg(palette.muted))
        .title(Span::styled(
            locale.strs().panel_tool_calls,
            bg.fg(palette.subtle),
        ))
        .style(bg);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if agent.tool_calls.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                locale.strs().panel_no_tool_calls,
                bg.fg(palette.muted),
            )))
            .style(bg),
            inner,
        );
        return;
    }

    let width = inner.width as usize;
    // Prompt-era group headers: a separator whenever consecutive calls fall
    // under a different user prompt (timestamp-derived, cached — see
    // [`EraCache`]). Skipped when the whole list shares one era — the
    // provenance section already names it.
    let header_before = &era_flags(era_cache, agent_id, agent, model).flags;

    // Pass 1: wrap the (few) era headers and total the virtual line count —
    // WITHOUT building a styled Line per tool call. Only the viewport's worth
    // of rows is materialized below; formatting every call of a tool-heavy
    // agent each frame dominated render time.
    let mut headers: Vec<Option<Vec<String>>> = Vec::with_capacity(agent.tool_calls.len());
    let mut total = 0usize;
    for (tc, is_header) in agent.tool_calls.iter().zip(header_before) {
        let wrapped = if *is_header
            && let Some(e) = model.prompt_for_ts(tc.ts)
            && let Some(p) = model.prompts.get(e)
        {
            // Era anchor: the user prompt that starts this group. NOT
            // line-capped: it lives in the scrollable list, so a long prompt
            // just takes more rows (the upstream ~240-char excerpt bounds it).
            Some(wrap(&p.excerpt, width.saturating_sub(2), usize::MAX))
        } else {
            None
        };
        total += wrapped.as_ref().map_or(0, Vec::len) + 1;
        headers.push(wrapped);
    }

    // Resolve the scroll + tail state against the real line count, and write both
    // back so the scroll indicator and the next keypress match what's on screen.
    let (scroll, follow) = resolve_scroll(
        total.min(u16::MAX as usize) as u16,
        inner.height,
        *detail_scroll,
        *detail_follow,
    );
    *detail_scroll = scroll;
    *detail_follow = follow;

    // Pass 2: materialize only the rows intersecting the viewport, rendering
    // with a residual scroll from the first materialized row.
    let view_start = scroll as usize;
    let view_end = view_start + inner.height as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(inner.height as usize + 4);
    let mut idx = 0usize;
    let mut first_built: Option<usize> = None;
    for (tc, wrapped) in agent.tool_calls.iter().zip(&headers) {
        let rows = wrapped.as_ref().map_or(0, Vec::len) + 1;
        if idx + rows > view_start && idx < view_end {
            if first_built.is_none() {
                first_built = Some(idx);
            }
            if let Some(wrapped) = wrapped {
                // Wrapped onto a subtle GRAY block (prompts are context, not
                // the agent activity gold is reserved for) — thin gold tick +
                // bright text, padded full-width so the band reads.
                let block = Style::default().bg(palette.muted);
                for l in wrapped {
                    let pad =
                        width.saturating_sub(2 + unicode_width::UnicodeWidthStr::width(l.as_str()));
                    lines.push(Line::from(vec![
                        Span::styled("▍ ", block.fg(palette.accent)),
                        Span::styled(l.clone(), block.fg(palette.text)),
                        Span::styled(" ".repeat(pad), block),
                    ]));
                }
            }
            lines.push(tool_line(tc, width, palette));
        }
        idx += rows;
        if idx >= view_end {
            break;
        }
    }
    let local_scroll = scroll.saturating_sub(first_built.unwrap_or(0) as u16);
    frame.render_widget(
        Paragraph::new(lines).style(bg).scroll((local_scroll, 0)),
        inner,
    );
}

/// Resolve the panel's scroll offset for one render: clamp to the reachable
/// maximum (keep the last screenful in view — no over-scroll into blank) and
/// reconcile the tail. Following pins to the bottom; scrolling back down to the
/// bottom (or content that fits) re-attaches. Returns `(offset, tailing)`.
fn resolve_scroll(total: u16, height: u16, scroll: u16, follow: bool) -> (u16, bool) {
    let max = total.saturating_sub(height);
    let offset = if follow { max } else { scroll.min(max) };
    (offset, offset >= max)
}

/// Which tool rows get an era header above them, plus the total rendered
/// line count (rows + headers). Shared by the renderer, the scroll clamp
/// (handler), and the scroll indicator so they can never disagree about the
/// list's true length.
fn era_header_flags(
    agent: &AgentInfo,
    model: &crate::state::session::SessionModel,
) -> (Vec<bool>, usize) {
    let mut flags = Vec::with_capacity(agent.tool_calls.len());
    let mut distinct = 0usize;
    let mut prev: Option<usize> = None;
    let mut headers = 0usize;
    for tc in &agent.tool_calls {
        let era = model.prompt_for_ts(tc.ts);
        let is_boundary = matches!(era, Some(e) if prev != Some(e));
        if is_boundary {
            distinct += 1;
        }
        flags.push(is_boundary);
        if let Some(e) = era {
            prev = Some(e);
        }
        if is_boundary {
            headers += 1;
        }
    }
    // Single-era lists get no headers — the provenance section names it.
    if distinct <= 1 {
        return (vec![false; agent.tool_calls.len()], agent.tool_calls.len());
    }
    (flags, agent.tool_calls.len() + headers)
}

/// Total rendered lines of an agent's tool list (rows + era headers) — the
/// scroll ceiling the handler clamps against.
pub fn tool_list_lines(agent: &AgentInfo, model: &crate::state::session::SessionModel) -> usize {
    era_header_flags(agent, model).1
}

/// One row of the tool-call list: state glyph, name, summary, local time.
fn tool_line(
    tc: &crate::state::session::ToolCallInfo,
    width: usize,
    palette: &rataflow::Palette,
) -> Line<'static> {
    let bg = Style::default().bg(palette.surface);
    let (glyph, color) = match tc.state {
        ToolState::Pending => ('⏳', palette.accent),
        ToolState::Ok => ('✓', palette.success),
        ToolState::Err => ('✗', palette.error),
    };
    let w = |s: &str| unicode_width::UnicodeWidthStr::width(s);
    let head = format!("{glyph} ");
    let mut used = w(&head) + w(tc.name.as_str());
    let mut spans = vec![
        Span::styled(head, bg.fg(color)),
        Span::styled(
            tc.name.clone(),
            bg.fg(palette.text).add_modifier(Modifier::BOLD),
        ),
    ];
    // Recorded transcript timestamps (UTC on the wire), shown in the viewer's
    // local time — and RIGHT-ALIGNED to the panel edge, not tacked onto the end
    // of the summary (which left it floating mid-line on a wide panel).
    let time = tc.ts.map(|t| {
        t.with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string()
    });
    let time_w = time.as_ref().map(|t| t.chars().count() + 1).unwrap_or(0);
    if let Some(summary) = tc.summary.as_ref().filter(|s| !s.is_empty()) {
        // The summary fills the space between the name and the right-aligned
        // time — the full panel width, so a wide screen shows more of it.
        let budget = width.saturating_sub(used + 1 + time_w);
        // Path tools: keep the basename (truncate the front); everything else
        // front-loads its meaning, so keep the head.
        let summary = if matches!(tc.name.as_str(), "Read" | "Write" | "Edit") {
            truncate_tail(summary, budget)
        } else {
            truncate(summary, budget)
        };
        used += 1 + w(&summary);
        spans.push(Span::styled(format!(" {summary}"), bg.fg(palette.subtle)));
    }
    if let Some(time) = time {
        // Pad from the content out to where the flush-right time begins.
        let pad = width.saturating_sub(used + time_w);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), bg));
        }
        spans.push(Span::styled(format!(" {time}"), bg.fg(palette.muted)));
    }
    Line::from(spans)
}

/// Format a timing line from an agent's first/last timestamps.
fn fmt_timing(agent: &AgentInfo, locale: Locale) -> Option<String> {
    match (agent.first_ts, agent.last_ts) {
        (Some(first), Some(last)) => {
            let secs = (last - first).num_seconds().max(0);
            if secs >= 60 {
                Some(format!("⏱ {}m {}s", secs / 60, secs % 60))
            } else {
                Some(format!("⏱ {secs}s"))
            }
        }
        (Some(first), None) => Some(fill(
            locale.strs().panel_started,
            &[(
                "t",
                &first
                    .with_timezone(&chrono::Local)
                    .format("%H:%M:%S")
                    .to_string(),
            )],
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_scroll_clamps_and_reconciles_tail() {
        // Content shorter than the viewport → always tailing, offset 0.
        assert_eq!(resolve_scroll(5, 10, 3, false), (0, true));
        // Following → pinned to the bottom (max = 20 - 8 = 12).
        assert_eq!(resolve_scroll(20, 8, 0, true), (12, true));
        // Detached and scrolled up → keep the offset, stay detached.
        assert_eq!(resolve_scroll(20, 8, 5, false), (5, false));
        // Detached but (over-)scrolled to the bottom → clamp + re-attach.
        assert_eq!(resolve_scroll(20, 8, 99, false), (12, true));
    }

    #[test]
    fn tool_list_lines_counts_era_headers() {
        use crate::state::session::{SessionModel, ToolCallInfo, ToolState};
        use crate::tailer::{Source, Update};
        use crate::transcript::parse_line;

        let mut m = SessionModel::new("s".into());
        for (uid, ts, text) in [
            ("p1", "2026-06-07T10:00:00.000Z", "first"),
            ("p2", "2026-06-07T11:00:00.000Z", "second"),
        ] {
            let line = format!(
                r#"{{"type":"user","uuid":"{uid}","parentUuid":null,"origin":{{"kind":"human"}},"timestamp":"{ts}","message":{{"role":"user","content":"{text}"}}}}"#
            );
            m.apply_update(&Update::Entry {
                source: Source::Main,
                entry: parse_line(&line).unwrap(),
            });
        }
        let agent = m.agents.get_mut(crate::state::session::MAIN_ID).unwrap();
        for (i, ts) in [
            "2026-06-07T10:30:00.000Z",
            "2026-06-07T11:30:00.000Z",
            "2026-06-07T11:31:00.000Z",
        ]
        .iter()
        .enumerate()
        {
            agent.tool_calls.push(ToolCallInfo {
                id: format!("t{i}"),
                name: "Bash".into(),
                summary: None,
                ts: Some(ts.parse().unwrap()),
                end_ts: None,
                state: ToolState::Ok,
            });
        }
        let agent = m.agent(crate::state::session::MAIN_ID).unwrap();
        // 3 tool rows + 2 era headers (eras 0 and 1) = 5 rendered lines —
        // the scroll ceiling the handler clamps against.
        assert_eq!(tool_list_lines(agent, &m), 5);
    }

    use chrono::{TimeZone, Utc};

    fn agent_with_ts(first: Option<i64>, last: Option<i64>) -> AgentInfo {
        let mut a = AgentInfo::new(crate::state::session::AgentKind::Subagent);
        a.first_ts = first.map(|s| Utc.timestamp_opt(s, 0).unwrap());
        a.last_ts = last.map(|s| Utc.timestamp_opt(s, 0).unwrap());
        a
    }

    #[test]
    fn timing_duration_under_a_minute() {
        let a = agent_with_ts(Some(100), Some(142));
        assert_eq!(fmt_timing(&a, Locale::En).as_deref(), Some("⏱ 42s"));
    }

    #[test]
    fn timing_duration_over_a_minute() {
        let a = agent_with_ts(Some(0), Some(125));
        assert_eq!(fmt_timing(&a, Locale::En).as_deref(), Some("⏱ 2m 5s"));
    }

    #[test]
    fn timing_negative_clamped() {
        let a = agent_with_ts(Some(100), Some(50));
        assert_eq!(fmt_timing(&a, Locale::En).as_deref(), Some("⏱ 0s"));
    }

    #[test]
    fn timing_none_when_no_first() {
        let a = agent_with_ts(None, None);
        assert!(fmt_timing(&a, Locale::En).is_none());
    }
}
