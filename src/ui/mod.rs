//! Rendering: top-level `draw`, the agent card content, and the detail panel.
//!
//! `draw` lays out the screen (status bar + canvas, optionally splitting in the
//! detail panel), renders `Background` then the flow then `MiniMap` (render
//! order matters — the `Widget` impl is on `&mut Flow`, companions borrow
//! `&Flow`, so they are separate `render_widget` calls), and finally the status
//! bar.

pub mod chips;
pub mod edges;
pub mod nodes;
pub mod panel;

use rataflow::{Background, MiniMap, MiniMapPosition};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Sparkline, SparklineBar};

use crate::i18n::{Locale, fill};
use crate::state::session::LogKind;
use crate::state::{App, Camera, Mode, Transport};

/// Render the entire UI for one frame.
///
/// Splits off a status bar, renders the flow canvas (with `Background` and
/// `MiniMap` companions), conditionally renders the detail panel when an agent
/// is selected, and draws the status bar (title, live/replay indicator, agent &
/// tool counts, pause state, key hints).
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Top: canvas (fill); one bordered timeline panel — the scrubber (6 rows),
    // plus an event-log line and a single divider on top when the session has
    // prompts (→ 8 rows); bottom: a one-row status bar.
    let show_scrubber = app.timeline.has_span();
    let show_log = show_scrubber && !app.session.prompts.is_empty();
    let (canvas_area, timeline_area, status_area) = if show_scrubber {
        let panel_h = if show_log { 8 } else { 6 };
        let [canvas, panel, status] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(panel_h),
            Constraint::Length(1),
        ])
        .areas(area);
        (canvas, Some(panel), status)
    } else {
        let [canvas, status] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
        (canvas, None, status)
    };
    // Cleared here; `render_scrubber` sets it to the seekable TRACK rect (not the
    // whole row) so the input handler maps a click to the same width the
    // playhead is drawn over.
    app.scrubber_area = None;

    // Copy the selected agent id out *before* borrowing the flow mutably for
    // the canvas render (borrow split: companions take &Flow, Widget is &mut).
    let selected = app.selected_agent_id();

    // When an agent is selected, split the canvas 30/70 for the detail panel —
    // the panel is what you're reading; the canvas only keeps the selected
    // node (click-centered) in view for orientation.
    let (flow_area, panel_area) = if selected.is_some() {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
                .areas(canvas_area);
        (left, Some(right))
    } else {
        (canvas_area, None)
    };

    render_canvas(frame, flow_area, app, selected.is_none());

    // A user selection centers its node — resolved HERE, after the flow has
    // rendered into the (possibly just-narrowed) canvas, because `center_on`
    // probes against the last-rendered viewport size. Centering at event time
    // would target the pre-split width and land the node off-center.
    if let Some(id) = app.pending_center.take() {
        // Manual spatial-nav: pan to the node but DON'T snap the zoom (that's a
        // Follow concern) — an arrow press shouldn't yank the user's zoom.
        app.center_node(&id, false);
    }

    if let (Some(panel_area), Some(id)) = (panel_area, selected.as_ref()) {
        panel::render(frame, panel_area, app, id);
    }

    if let Some(panel) = timeline_area {
        render_timeline_panel(frame, panel, app, show_log);
    }

    render_status_bar(frame, status_area, app);

    if app.show_help {
        render_help(frame, area, &app.flow.theme.palette(), app.locale);
    }
    if app.show_info {
        render_info(frame, area, app);
    }
}

/// Centered session-info overlay (`i`): the untimed session-level metadata that
/// is kept off the timeline — title, permission/editor mode, last prompt, and
/// queued/file-edit counts.
fn render_info(frame: &mut Frame, area: Rect, app: &App) {
    let palette = app.flow.theme.palette();
    let w = area.width.min(54);
    let h = area.height.min(11);
    if w < 24 || h < 7 {
        return;
    }
    let popup = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    frame.render_widget(Clear, popup);

    let bg = Style::default().bg(palette.surface);
    let key = bg.fg(palette.accent).add_modifier(Modifier::BOLD);
    let txt = bg.fg(palette.text);
    let dim = bg.fg(palette.subtle);
    let info = &app.session_info;
    let strs = app.locale.strs();

    let value_w = (w as usize).saturating_sub(12);
    let row = |label: &'static str, value: String, style: Style| {
        Line::from(vec![
            Span::styled(format!(" {} ", pad_width(label, 8)), key),
            Span::styled(truncate(&value, value_w), style),
        ])
    };
    let dash = "—".to_string();

    let lines = vec![
        Line::from(""),
        row(
            strs.info_title,
            info.title.clone().unwrap_or_else(|| dash.clone()),
            txt,
        ),
        row(
            strs.info_perms,
            info.permission_mode.clone().unwrap_or_else(|| dash.clone()),
            txt,
        ),
        row(
            strs.info_mode,
            info.mode.clone().unwrap_or_else(|| dash.clone()),
            txt,
        ),
        row(
            strs.info_queued,
            fill(
                strs.info_queued_edits,
                &[
                    ("q", &info.queued_ops.to_string()),
                    ("f", &info.file_snapshots.to_string()),
                ],
            ),
            dim,
        ),
        row(
            strs.info_last,
            info.last_prompt
                .as_deref()
                .map(|p| format!("\"{p}\""))
                .unwrap_or_else(|| dash.clone()),
            dim,
        ),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(bg.fg(palette.accent))
        .style(bg)
        .title_top(
            Line::from(strs.info_heading)
                .centered()
                .style(bg.fg(palette.text).add_modifier(Modifier::BOLD)),
        )
        .title_bottom(Line::from(strs.info_close).centered().style(dim));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Per-column scrubber tallies: tool-call count + spawn/fail flags for each bar
/// column, plus the busiest count. These depend only on the items, the bar width,
/// and `floor` — NOT the playhead — so they're cached on `App` and recomputed
/// only when the item count or width changes, instead of rescanning every item
/// every frame.
pub(crate) struct ScrubberTally {
    pub len: usize,
    pub width: usize,
    pub counts: Vec<u64>,
    pub spawn_at: Vec<bool>,
    pub fail_at: Vec<bool>,
    pub maxc: u64,
    /// Item indices after a long idle gap (fast-forward markers) — also
    /// playhead-independent, so cached here rather than rescanned every frame.
    pub gaps: Vec<usize>,
    /// Item indices of main-thread user prompts — the era boundaries `[`/`]`
    /// step between, drawn as chapter ticks. Playhead-independent → cached.
    pub prompts: Vec<usize>,
}

/// Bin `items` into `width` columns over the reachable span `[floor, len]`,
/// tallying tool calls and spawn/fail presence per column. Pure (caller caches).
pub(crate) fn compute_scrubber_tally(
    items: &[crate::tailer::ReplayItem],
    width: usize,
    floor: usize,
) -> ScrubberTally {
    let len = items.len();
    let last = (width.saturating_sub(1)).max(1) as f64;
    let reach = len.saturating_sub(floor);
    let col_idx = |c: usize| -> usize {
        (floor + ((c as f64 / last) * reach as f64).round() as usize).min(len)
    };
    // Spawn `tool_use_id`s that have a discovered subagent (its meta joins on the
    // same id). A subagent EXISTS from its birth, which is when its meta folds and
    // the canvas node appears — so we mark ❋ there (the meta), and the spawning
    // tool_use is only a fallback for spawns whose subagent isn't loaded (e.g. a
    // single-file upload). Scanned over ALL items, so it's fold-independent.
    let meta_tool_use_ids: std::collections::BTreeSet<&str> = items
        .iter()
        .filter_map(|it| match &it.update {
            crate::tailer::Update::SubagentMeta { meta, .. } => meta.tool_use_id.as_deref(),
            _ => None,
        })
        .collect();
    let mut counts = vec![0u64; width];
    let mut spawn_at = vec![false; width];
    let mut fail_at = vec![false; width];
    for c in 0..width {
        let (a, b) = (col_idx(c), if c + 1 < width { col_idx(c + 1) } else { len });
        for it in &items[a..b] {
            match &it.update {
                crate::tailer::Update::Entry { entry, .. } => {
                    counts[c] += entry.tool_use_count() as u64;
                    // A spawn call marks ❋ only when its subagent has no meta (not
                    // loaded); otherwise the subagent's own meta marks it at birth.
                    spawn_at[c] |= entry
                        .spawn_tool_use_ids()
                        .iter()
                        .any(|id| !meta_tool_use_ids.contains(id));
                    fail_at[c] |= entry.tool_failure_count() > 0;
                }
                // A subagent's meta discovery IS its birth on the timeline — the
                // moment the node appears on the canvas. Mark ❋ here for every
                // subagent, so the strip, the canvas, and the log all agree on
                // when the agent starts to exist.
                crate::tailer::Update::SubagentMeta { .. } => spawn_at[c] = true,
            }
        }
    }
    let maxc = counts.iter().copied().max().unwrap_or(0);
    ScrubberTally {
        len,
        width,
        counts,
        spawn_at,
        fail_at,
        maxc,
        gaps: Vec::new(),
        prompts: Vec::new(),
    }
}

/// Render the DVR scrubber content into `area` (the border is owned by
/// [`render_timeline_panel`]): a 2-row-tall tool-activity sparkline (with the
/// playhead + markers overlaid) above a 1-row info line (playhead date+time on
/// the left, transport tag on the right).
fn render_scrubber(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.width < 8 || area.height < 4 {
        return;
    }
    let palette = app.flow.theme.palette();
    let bg = Style::default().bg(palette.surface);

    // A dedicated 1-row marker strip ABOVE the 2-row bars, so markers and bars
    // never overwrite each other; then the info line.
    let [marker_row, bars_area, info_row] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(area);

    let transport = app.transport();
    // Seekable region = the marker strip + the bars.
    app.scrubber_area = Some(Rect::new(
        marker_row.x,
        marker_row.y,
        marker_row.width,
        marker_row.height + bars_area.height,
    ));

    let width = bars_area.width as usize;
    let len = app.timeline.items.len();
    if width >= 2 && len > 0 {
        let p = app.timeline.progress().clamp(0.0, 1.0);
        let head = ((p * (width - 1) as f64).round() as usize).min(width - 1);
        let last = (width - 1) as f64;

        // Per-column tallies (tool counts + spawn/fail) are head-independent, so
        // recompute them only when the item count or width changes — most frames
        // (replay paced, playhead moving) reuse the cache instead of rescanning
        // every item.
        let stale = app
            .scrubber_tally
            .as_ref()
            .is_none_or(|t| t.len != len || t.width != width);
        if stale {
            let floor = app.timeline.floor();
            let mut tally = compute_scrubber_tally(&app.timeline.items, width, floor);
            tally.gaps = app.timeline.gap_markers();
            tally.prompts = app.timeline.prompt_markers();
            app.scrubber_tally = Some(tally);
        }
        let tally = app.scrubber_tally.as_ref().unwrap();
        let counts = &tally.counts;
        let spawn_at = &tally.spawn_at;
        let fail_at = &tally.fail_at;
        let maxc = tally.maxc;

        // Normalize to the available eighths (8 per row) with a FLOOR of 1 for any
        // nonzero column — otherwise the busiest column scales the rest down and a
        // tick with little activity rounds to 0 (invisible). `ceil` guarantees
        // count>0 → at least the lowest block.
        let levels = (bars_area.height as u64) * 8;
        let bars: Vec<SparklineBar> = counts
            .iter()
            .enumerate()
            .map(|(c, &cnt)| {
                let lvl = if cnt == 0 || maxc == 0 {
                    0
                } else {
                    ((cnt as f64 / maxc as f64) * levels as f64).ceil() as u64
                };
                // Played (left of the playhead) bright accent; unplayed dim.
                let color = if c < head {
                    palette.accent
                } else {
                    palette.subtle
                };
                SparklineBar::from(lvl).style(bg.fg(color))
            })
            .collect();
        frame.render_widget(
            Sparkline::default().data(bars).max(levels).style(bg),
            bars_area,
        );

        // Overlays via buffer writes. All markers live on the dedicated marker
        // strip (never over the bars); the playhead spans the strip + both bars.
        let buf = frame.buffer_mut();
        let marker_y = marker_row.y;
        let bars_bot = bars_area.y + bars_area.height - 1;
        let x = |col: usize| marker_row.x + col as u16;
        // Fast-forward markers: where playback compresses a long idle gap
        // (cached on the tally — playhead-independent). Only meaningful when
        // inactivity-skip is on; in faithful mode time isn't compressed, so a
        // `»` would lie — suppress it.
        if app.timeline.compress_gaps {
            for &gi in &tally.gaps {
                let col = (app.timeline.bar_fraction_for_index(gi) * last).round() as usize;
                if col < width {
                    buf[(x(col), marker_y)]
                        .set_symbol("»")
                        .set_style(bg.fg(palette.subtle));
                }
            }
        }
        // Prompt-era chapter ticks — the boundaries `[`/`]` snap between. Drawn
        // across the WHOLE timeline (unlike the past-only event markers) so the
        // forward jump targets are visible too: a filled ◆ in accent behind the
        // playhead, a dim hollow ◇ ahead. Below the event markers, so a spawn or
        // failure sharing the column still wins (the tick reappears either side).
        for &pi in &tally.prompts {
            let col = (app.timeline.bar_fraction_for_index(pi) * last).round() as usize;
            if col < width {
                let (glyph, style) = if col < head {
                    ("◆", bg.fg(palette.accent).add_modifier(Modifier::BOLD))
                } else {
                    ("◇", bg.fg(palette.subtle))
                };
                buf[(x(col), marker_y)].set_symbol(glyph).set_style(style);
            }
        }
        // Event markers, PAST only (reveal as the playhead reaches them — in sync
        // with the graph's chips): spawns, then failures (more urgent → on top).
        // Spawn = the Claude sunburst in Claude coral (#d7875f ≈ xterm 173).
        for c in (0..head).filter(|&c| spawn_at[c]) {
            buf[(x(c), marker_y)]
                .set_symbol("❋")
                .set_style(bg.fg(Color::Indexed(173)).add_modifier(Modifier::BOLD));
        }
        for c in (0..head).filter(|&c| fail_at[c]) {
            buf[(x(c), marker_y)]
                .set_symbol("✗")
                .set_style(bg.fg(palette.error).add_modifier(Modifier::BOLD));
        }
        // Playhead: a gold vertical line over a translucent (tinted) column,
        // spanning the marker strip and both bar rows.
        let ph_style = Style::default()
            .fg(palette.accent)
            .bg(palette.muted)
            .add_modifier(Modifier::BOLD);
        for y in marker_y..=bars_bot {
            buf[(x(head), y)].set_symbol("│").set_style(ph_style);
        }
    }

    // --- Info row: date+time (left), transport tag (right). On its own row, so
    // label-width changes never touch the track. ---
    let clock = match app.timeline.cursor {
        Some(c) => c
            .with_timezone(&chrono::Local)
            .format("%b %d %H:%M:%S")
            .to_string(),
        None => "—".to_string(),
    };
    let s = app.locale.strs();
    let (tag, tag_style) = match transport {
        Transport::Live => (
            s.transport_live,
            bg.fg(palette.success).add_modifier(Modifier::BOLD),
        ),
        Transport::Playing => (s.transport_play, bg.fg(palette.accent)),
        Transport::Paused => (s.transport_pause, bg.fg(palette.accent)),
        Transport::History => (s.transport_history, bg.fg(palette.subtle)),
        Transport::Idle => (s.transport_end, bg.fg(palette.subtle)),
    };
    // Display columns, not chars: a CJK tag is 2 cols per char and would
    // under-reserve the right-aligned area.
    let tag_cols = unicode_width::UnicodeWidthStr::width(tag) as u16;
    let [clock_area, tag_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(tag_cols + 1)]).areas(info_row);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(clock, bg.fg(palette.subtle)))).style(bg),
        clock_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(tag, tag_style)))
            .style(bg)
            .alignment(Alignment::Right),
        tag_area,
    );
}

/// The bordered timeline panel: one rounded frame around the scrubber, with an
/// event-log line and a single `├──┤` divider on top when `show_log` (so there's
/// one border between the log and the timeline, not two adjacent ones).
fn render_timeline_panel(frame: &mut Frame, area: Rect, app: &mut App, show_log: bool) {
    if area.width < 8 || area.height < 6 {
        return;
    }
    let palette = app.flow.theme.palette();
    let bg = Style::default().bg(palette.surface);
    let border = bg.fg(palette.subtle);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .style(bg);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content = if show_log {
        // Log line (top), a single divider, then the scrubber content (4 rows).
        let [log_row, div_row, scrub] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(4),
        ])
        .areas(inner);
        render_log_line(frame, log_row, app);
        // One divider that joins the outer frame with ├ … ┤.
        let buf = frame.buffer_mut();
        let dy = div_row.y;
        buf[(area.x, dy)].set_symbol("├").set_style(border);
        for cx in inner.x..inner.x + inner.width {
            buf[(cx, dy)].set_symbol("─").set_style(border);
        }
        buf[(area.x + area.width - 1, dy)]
            .set_symbol("┤")
            .set_style(border);
        scrub
    } else {
        inner
    };
    render_scrubber(frame, content, app);
}

/// The event-log line inside the timeline panel: the most recent timeline event
/// at or before the playhead — its own timestamp, then an icon-matched glyph (◆
/// prompt, ❋ spawn, ✗ failure) and text — narrating what's happening as the
/// playhead moves. A property of the timeline position, never stitched onto an
/// agent. Renders into `row` (border owned by the panel); blank before the first
/// event.
fn render_log_line(frame: &mut Frame, row: Rect, app: &App) {
    // Spawn `tool_use_id`s that have a discovered subagent — so a spawn call only
    // narrates as a fallback when its subagent isn't loaded (matches the strip).
    let meta_tool_use_ids: std::collections::BTreeSet<String> = app
        .timeline
        .items
        .iter()
        .filter_map(|it| match &it.update {
            crate::tailer::Update::SubagentMeta { meta, .. } => meta.tool_use_id.clone(),
            _ => None,
        })
        .collect();
    let Some(ev) = app
        .session
        .latest_event_at(app.timeline.cursor, &meta_tool_use_ids)
    else {
        return;
    };
    let palette = app.flow.theme.palette();
    let bg = Style::default().bg(palette.surface);
    // 1-col padding inside the panel border.
    let r = Rect::new(row.x + 1, row.y, row.width.saturating_sub(2), row.height);
    if r.width == 0 {
        return;
    }
    let time = ev
        .ts
        .with_timezone(&chrono::Local)
        .format("%H:%M:%S")
        .to_string();
    let (icon, icon_style) = match ev.kind {
        LogKind::Prompt => ("◆ ", bg.fg(palette.accent).add_modifier(Modifier::BOLD)),
        // Coral to match the spawn ❋ on the scrubber's marker strip (xterm 173).
        LogKind::Spawn => (
            "❋ ",
            bg.fg(Color::Indexed(173)).add_modifier(Modifier::BOLD),
        ),
        LogKind::Failure => ("✗ ", bg.fg(palette.error).add_modifier(Modifier::BOLD)),
    };
    // Width left for the text after the "HH:MM:SS " prefix and the 2-col icon.
    let tw = (r.width as usize).saturating_sub(time.chars().count() + 3);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{time} "), bg.fg(palette.subtle)),
            Span::styled(icon, icon_style),
            Span::styled(truncate(&ev.text, tw), bg.fg(palette.text)),
        ]))
        .style(bg),
        r,
    );
}

/// Centered help overlay: full key reference + status-glyph legend.
fn render_help(frame: &mut Frame, area: Rect, palette: &rataflow::Palette, locale: Locale) {
    let w = area.width.min(60);
    let h = area.height.min(18);
    if w < 24 || h < 9 {
        return;
    }
    let popup = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    frame.render_widget(Clear, popup);

    let bg = Style::default().bg(palette.surface);
    let key = bg.fg(palette.accent).add_modifier(Modifier::BOLD);
    let txt = bg.fg(palette.text);
    let dim = bg.fg(palette.subtle);
    let s = locale.strs();

    // The keymap is identical on native and web except quit: a browser tab
    // can't reliably close itself, so there `q`/`ctrl-c` don't apply.
    let quit_hint = if cfg!(target_arch = "wasm32") {
        s.help_quit_web
    } else {
        "q · ctrl-c"
    };

    // Section-label column, padded to one common display width so the
    // descriptions align (pad_width: CJK labels are 2 cols per char).
    const LABEL_COLS: usize = 10;
    let section = |name: &str| Span::styled(pad_width(name, LABEL_COLS), key);
    let legend_spacer = Span::styled(" ".repeat(LABEL_COLS), key);

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            section(s.sec_camera),
            Span::styled(s.help_camera, txt),
        ]),
        Line::from(vec![
            section(s.sec_layout),
            Span::styled(s.help_layout, txt),
        ]),
        Line::from(vec![
            section(s.sec_navigate),
            Span::styled(s.help_navigate, txt),
        ]),
        Line::from(vec![
            section(s.sec_viewport),
            Span::styled(s.help_viewport, txt),
        ]),
        Line::from(vec![section(s.sec_panel), Span::styled(s.help_panel, txt)]),
        Line::from(vec![
            section(s.sec_timeline),
            Span::styled(s.help_timeline_a, txt),
            Span::styled("◆", bg.fg(palette.accent)),
            Span::styled(s.help_timeline_b, txt),
        ]),
        Line::from(vec![
            section(s.sec_pacing),
            Span::styled(s.help_pacing, txt),
        ]),
        Line::from(vec![section(s.sec_info), Span::styled(s.help_info, txt)]),
        Line::from(vec![
            section(s.sec_replay),
            Span::styled(s.help_replay, txt),
        ]),
        Line::from(vec![
            section(s.sec_language),
            Span::styled(s.help_language, txt),
        ]),
        Line::from(vec![
            section(s.sec_status),
            Span::styled("● ", bg.fg(palette.success)),
            Span::styled(format!("{}  ", s.legend_active_running), txt),
            Span::styled("◌ ", dim),
            Span::styled(format!("{}  ", s.legend_idle), txt),
            Span::styled("✓ ", bg.fg(palette.accent)),
            Span::styled(format!("{}  ", s.legend_done), txt),
            Span::styled("✗ ", bg.fg(palette.error)),
            Span::styled(s.legend_failed, txt),
        ]),
        Line::from(vec![legend_spacer, Span::styled(s.legend_edges, dim)]),
        Line::from(""),
        Line::from(vec![section(s.sec_quit), Span::styled(quit_hint, txt)]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(bg.fg(palette.accent))
        .style(bg)
        .title_top(
            Line::from(s.help_title)
                .centered()
                .style(bg.fg(palette.text).add_modifier(Modifier::BOLD)),
        )
        .title_bottom(Line::from(s.help_close).centered().style(dim));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Render the flow graph with its `Background` and `MiniMap` companions.
///
/// Three separate `render_widget` calls: `Background::new(&flow)` reads the
/// flow immutably, then `&mut flow` (the `Widget` impl) renders the canvas,
/// then `MiniMap::new(&flow)` reads it immutably again. The borrows do not
/// overlap because each call completes before the next begins.
///
/// The minimap is skipped while the detail panel is open (`show_minimap` =
/// false): the canvas is narrowed to a 30% orientation strip and the overlay
/// would eat its top-right corner — sometimes covering the centered node.
fn render_canvas(frame: &mut Frame, area: Rect, app: &mut App, show_minimap: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    frame.render_widget(Background::new(&app.flow), area);
    frame.render_widget(&mut app.flow, area);
    // Chips right after the flow (frame-exact anchors), under the minimap.
    // `now_reference` drives the live-ticking duration on a single-tool chip.
    let now = app.timeline.now_reference();
    chips::render(&app.chips, &app.flow, &app.session, now, frame.buffer_mut());
    if show_minimap {
        frame.render_widget(
            MiniMap::new(&app.flow).position(MiniMapPosition::TopRight),
            area,
        );
    }
}

/// Render the bottom status bar: title, live/replay badge, pause indicator,
/// agent & tool counts, and key hints.
fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let palette = app.flow.theme.palette();
    let bg = Style::default().bg(palette.surface);
    let s = app.locale.strs();

    let title = app
        .session_info
        .title
        .as_deref()
        .unwrap_or(s.statusbar_untitled);

    // Emergent transport badge — "LIVE" is following + fresh appends, never a
    // hardcoded mode.
    let (badge, badge_color) = match app.transport() {
        Transport::Live => (s.badge_live, palette.success),
        Transport::Playing => (s.badge_play, palette.accent),
        Transport::Paused => (s.badge_pause, palette.accent),
        Transport::History => (s.badge_past, palette.subtle),
        Transport::Idle => (s.badge_idle, palette.subtle),
    };

    let mut left: Vec<Span> = vec![
        // Wordmark: the gold identity chip in every screenshot.
        Span::styled(
            " zoetrope ",
            Style::default()
                .bg(palette.accent)
                .fg(palette.canvas_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", bg),
        Span::styled(
            badge,
            bg.fg(palette.canvas_bg)
                .bg(badge_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", bg),
        Span::styled(
            truncate(title, 40),
            bg.fg(palette.text).add_modifier(Modifier::BOLD),
        ),
    ];

    left.push(Span::styled(
        fill(
            s.statusbar_counts,
            &[
                ("a", &app.session.agent_count().to_string()),
                ("t", &app.session.tool_count().to_string()),
            ],
        ),
        bg.fg(palette.subtle),
    ));

    match app.camera {
        Camera::Overview => left.push(Span::styled(s.statusbar_overview, bg.fg(palette.subtle))),
        Camera::Follow => left.push(Span::styled(s.statusbar_follow, bg.fg(palette.subtle))),
        Camera::Manual => {}
    }

    if let Some(err) = &app.last_error {
        left.push(Span::styled(
            format!("  ⚠ {}", truncate(err, 50)),
            bg.fg(palette.error).add_modifier(Modifier::BOLD),
        ));
    }

    // Key hints — the help overlay carries the full list. `q quit` only applies
    // natively (a browser tab can't close itself), so it's dropped on web.
    let quit = if cfg!(target_arch = "wasm32") {
        ""
    } else {
        s.statusbar_quit
    };
    let pause = if app.mode == Mode::Replay {
        format!(" · space {}", s.statusbar_pause)
    } else {
        String::new()
    };
    let hints = format!("{}{}{}", quit, s.statusbar_help, pause);

    // Reserve the hint area in terminal CELLS, not bytes: the hints contain
    // multibyte glyphs (`·`, the arrows, and every CJK char in a translated
    // hint), so `str::len()` would over-reserve and squeeze the left status —
    // and char count would UNDER-reserve CJK (2 columns per char), so measure
    // display columns.
    let hints_cols = unicode_width::UnicodeWidthStr::width(hints.as_str()) as u16;
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(hints_cols + 1)]).areas(area);

    frame.render_widget(Paragraph::new(Line::from(left)).style(bg), left_area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hints, bg.fg(palette.muted))))
            .style(bg)
            .alignment(Alignment::Right),
        right_area,
    );
}

/// Presence color for a status — single source, paired with
/// `AgentStatus::glyph()` / `AgentInfo::status_word()` in the model.
/// Green = alive, calm accent = finished history, dim = idle, red = broken.
pub(crate) fn status_color(
    status: crate::state::session::AgentStatus,
    palette: &rataflow::Palette,
) -> ratatui::style::Color {
    use crate::state::session::AgentStatus;
    match status {
        AgentStatus::Running => palette.success,
        AgentStatus::Idle => palette.subtle,
        AgentStatus::Done => palette.accent,
        AgentStatus::Failed => palette.error,
        // Stopped: terminal but not success/failure — a neutral, muted mark.
        AgentStatus::Stopped => palette.muted,
    }
}

/// Right-pad to exactly `cols` display columns (unicode-width, so CJK labels
/// pad correctly — `format!("{:<w$}")` counts chars and under-pads wide ones).
pub(crate) fn pad_width(s: &str, cols: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let w = s.width();
    if w >= cols {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(cols - w))
    }
}

/// Truncate to `max` display columns with an ellipsis (column-measured via
/// unicode-width — CJK/emoji count 2 — and never panics on multibyte input).
pub(crate) fn truncate(s: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    // Keep columns for the content up to max-1, reserving one for the ellipsis.
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        w += cw;
        out.push(ch);
    }
    out.push('…');
    out
}

/// Greedy word-wrap `text` to lines of at most `width` display columns
/// (unicode-width — CJK/emoji count 2), capped at `max_lines` (the last line
/// gets a trailing `…` when content was dropped). Over-long words are
/// hard-split. Used for prompts — the panel's readable, high-signal anchors —
/// unlike the dense single-line tool rows.
pub(crate) fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in text.split_whitespace() {
        let wlen = word.width();
        let fits = if cur.is_empty() {
            wlen <= width
        } else {
            cur_w + 1 + wlen <= width
        };
        if fits {
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += 1;
            }
            cur.push_str(word);
            cur_w += wlen;
        } else if wlen > width {
            // Hard-split a word longer than the whole width.
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let mut rest = word;
            while rest.width() > width {
                let cut = split_at_width(rest, width);
                lines.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            cur = rest.to_string();
            cur_w = cur.width();
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
            cur_w = wlen;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            *last = truncate(&format!("{last} …"), width);
        }
    }
    lines
}

/// Byte index splitting `s` at no more than `cols` display columns — but at
/// least one char, so a single char wider than `cols` still advances (no
/// infinite hard-split loop).
fn split_at_width(s: &str, cols: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut w = 0;
    for (i, ch) in s.char_indices() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > cols && i > 0 {
            return i;
        }
        w += cw;
    }
    s.len()
}

/// Like [`truncate`] but keeps the END (e.g. a path's basename), eliding the
/// front: `…/state/timeline.rs`. Column-measured, never panics on multibyte.
pub(crate) fn truncate_tail(s: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    // Keep trailing chars totalling at most max-1 columns (one for the ellipsis).
    let mut w = 0;
    let mut start = s.len();
    for (i, ch) in s.char_indices().rev() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        w += cw;
        start = i;
    }
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::{compute_scrubber_tally, truncate, truncate_tail, wrap};
    use crate::tailer::{ReplayItem, Source, Update};

    #[test]
    fn truncate_basic() {
        assert_eq!(truncate("session title", 7), "sessio…");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn wrap_word_wraps_and_caps() {
        assert_eq!(
            wrap("one two three four", 8, 5),
            vec!["one two", "three", "four"]
        );
        // Cap with ellipsis on the last kept line.
        assert_eq!(wrap("aaa bbb ccc ddd", 3, 2), vec!["aaa", "bb…"]);
        // Fits on one line.
        assert_eq!(wrap("short", 20, 3), vec!["short"]);
    }

    #[test]
    fn truncate_tail_keeps_the_basename() {
        // Long path: keep the END (basename), elide the front.
        assert_eq!(truncate_tail("src/state/timeline.rs", 12), "…timeline.rs");
        // Fits → unchanged.
        assert_eq!(truncate_tail("Cargo.toml", 20), "Cargo.toml");
    }

    #[test]
    fn wide_text_stays_within_column_budgets() {
        use unicode_width::UnicodeWidthStr;
        // CJK chars are 2 columns wide — char-counted budgets overflowed cards
        // and pushed the panel's right-aligned time column off its rect.
        let s = "日本語テスト"; // 6 chars, 12 columns
        let out = truncate(s, 6);
        assert!(out.width() <= 6, "{out:?} is {} columns", out.width());
        let tail = truncate_tail(s, 6);
        assert!(tail.width() <= 6, "{tail:?} is {} columns", tail.width());
        // wrap: every produced line fits the column budget (hard-split words).
        for line in wrap("修复解析错误 and fix the parser", 6, usize::MAX) {
            assert!(line.width() <= 6, "{line:?} is {} columns", line.width());
        }
    }

    #[test]
    fn scrubber_tally_bins_tools_spawns_and_failures() {
        // An assistant turn with two tool_use blocks, one of them an Agent spawn.
        let assistant = r#"{"type":"assistant","uuid":"a","timestamp":"2026-06-05T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}},{"type":"tool_use","id":"t2","name":"Agent","input":{}}]}}"#;
        // A user turn carrying an errored tool_result.
        let failure = r#"{"type":"user","uuid":"u","timestamp":"2026-06-05T10:00:02.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true}]}}"#;
        let item = |line: &str, t: &str| {
            ReplayItem::at(
                Some(t.parse().unwrap()),
                Update::Entry {
                    source: Source::Main,
                    entry: crate::transcript::parse_line(line).unwrap(),
                },
            )
        };
        let items = vec![
            item(assistant, "2026-06-05T10:00:01.000Z"),
            item(failure, "2026-06-05T10:00:02.000Z"),
        ];

        let t = compute_scrubber_tally(&items, 4, 0);
        assert_eq!(t.len, 2);
        assert_eq!(t.width, 4);
        // Two tool_use blocks total across the columns; one spawn; one failure.
        assert_eq!(t.counts.iter().sum::<u64>(), 2);
        assert_eq!(t.maxc, 2, "both tool_use land in the same column");
        assert!(t.spawn_at.iter().any(|&s| s), "the Agent spawn is flagged");
        assert!(
            t.fail_at.iter().any(|&f| f),
            "the errored result is flagged"
        );
    }

    #[test]
    fn spawn_is_marked_at_birth_meta_with_the_call_as_a_no_meta_fallback() {
        let ts = "2026-06-05T10:00:01.000Z";
        // The spawning `Agent` call (tool_use id "toolu_1").
        let call = format!(
            r#"{{"type":"assistant","uuid":"a","timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_1","name":"Agent","input":{{}}}}]}}}}"#
        );
        let call_item = || {
            ReplayItem::at(
                Some(ts.parse().unwrap()),
                Update::Entry {
                    source: Source::Main,
                    entry: crate::transcript::parse_line(&call).unwrap(),
                },
            )
        };
        let meta_item = |tool_use_id: Option<&str>| {
            ReplayItem::at(
                Some("2026-06-05T10:00:05.000Z".parse().unwrap()),
                Update::SubagentMeta {
                    agent_id: "a1000000000000001".into(),
                    workflow: None,
                    meta: crate::transcript::SubagentMeta {
                        agent_type: Some("subagent".into()),
                        description: None,
                        tool_use_id: tool_use_id.map(str::to_string),
                        stopped_by_user: None,
                    },
                },
            )
        };

        // Call + its meta (joined on toolu_1): ONE ❋, at the meta (birth), and the
        // call does NOT also mark. The meta lands in a later column than the call.
        let t = compute_scrubber_tally(&[call_item(), meta_item(Some("toolu_1"))], 8, 0);
        assert_eq!(
            t.spawn_at.iter().filter(|&&s| s).count(),
            1,
            "one ❋, at birth — the call is suppressed by its meta"
        );

        // The call alone (subagent not loaded, e.g. single-file upload) → the call
        // is the fallback marker.
        let t = compute_scrubber_tally(&[call_item()], 4, 0);
        assert!(t.spawn_at.iter().any(|&s| s), "no meta → the call marks ❋");

        // A workflow/journal subagent (meta with no tool_use_id) → its meta marks.
        let t = compute_scrubber_tally(&[meta_item(None)], 4, 0);
        assert!(t.spawn_at.iter().any(|&s| s), "meta birth marks ❋");
    }
}
