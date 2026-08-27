//! The session rail: one row per live session, down the left edge.
//!
//! Reads [`SessionRail`](crate::state::rail::SessionRail) only — rows arrive
//! pre-summarized from the discovery sweep, so rendering never touches the
//! filesystem and never folds a model. The row for the watched session is
//! marked; every other row is a session the app is *not* tailing, which is the
//! whole point of the rail being cheap.
//!
//! Glyphs and colors are the graph's: `●` running, `◌` idle
//! ([`AgentStatus::glyph`](crate::state::session::AgentStatus::glyph)), and
//! every color resolved from `flow.theme.palette()` — `success` for alive,
//! `error` for failures, `accent` for the focus marker, on the `surface`
//! background the other panels use. A rail row and the agent card for the same
//! session must never look like they disagree, which they would the moment the
//! rail hardcoded a color the theme could change.

use chrono::{DateTime, Utc};
use rataflow::Palette;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

use crate::state::App;
use crate::state::rail::{RailRow, RailStatus};
use crate::state::session::AgentStatus;
use crate::ui::truncate;

/// Width of the rail pane, in columns.
///
/// Wide enough for a project name plus the status/count/age columns, narrow
/// enough that the canvas keeps the majority of a standard 80-column terminal.
pub const RAIL_WIDTH: u16 = 30;

/// Columns the fixed right-hand side of a row occupies: status glyph, agent
/// count, and age. The project name gets whatever is left.
const FIXED_COLS: u16 = 12;

/// Render the rail into `area`.
///
/// Records `area` on the App so a click can be mapped back to a row; a frame
/// that does not draw the rail leaves it cleared, so a stale rect can never
/// swallow a canvas click.
pub fn render(frame: &mut Frame, area: Rect, app: &mut App, now: DateTime<Utc>) {
    let palette = app.flow.theme.palette();
    let bg = Style::default().bg(palette.surface);
    // No view-mode label: until the forest renderer lands there is only one
    // layout, and printing "auto:forest" over a rail would be a caption that
    // disagrees with the screen.
    let title = format!(
        " sessions {}/{} ",
        app.rail.running(now),
        app.rail.rows.len()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(bg.fg(palette.subtle))
        .style(bg)
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title,
            bg.fg(palette.subtle).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.rail_area = Some(inner);

    if app.rail.rows.is_empty() {
        let msg = if app.rail.swept {
            "no recent sessions"
        } else {
            "scanning…"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                msg,
                bg.fg(palette.subtle).add_modifier(Modifier::ITALIC),
            ))
            .style(bg),
            inner,
        );
        return;
    }

    let focused = app.rail.focused_index(&app.current_session_id);
    let lines: Vec<Line> = app
        .rail
        .rows
        .iter()
        .take(inner.height as usize)
        .enumerate()
        .map(|(i, row)| row_line(row, Some(i) == focused, inner.width, now, &palette))
        .collect();

    frame.render_widget(Paragraph::new(lines).style(bg), inner);
}

/// One rail row: `▸ project  ● 4 12m`.
fn row_line<'a>(
    row: &'a RailRow,
    focused: bool,
    width: u16,
    now: DateTime<Utc>,
    palette: &Palette,
) -> Line<'a> {
    let bg = Style::default().bg(palette.surface);
    let alive = row.status(now) == RailStatus::Running;
    // The graph's own status vocabulary: `success` is reserved for "alive"
    // there, so reusing it here is what keeps the two readings identical.
    let (glyph, status_color) = if alive {
        (AgentStatus::Running.glyph(), palette.success)
    } else {
        (AgentStatus::Idle.glyph(), palette.subtle)
    };

    // The focused row is the one the canvas is showing, and `accent` is the
    // app's selection color everywhere else — so it means the same thing here.
    let marker = if focused { "▸" } else { " " };
    let name_width = width.saturating_sub(FIXED_COLS).max(4) as usize;
    let name = truncate(&row.project, name_width);

    let name_style = match (focused, alive) {
        (true, _) => bg.fg(palette.accent).add_modifier(Modifier::BOLD),
        (false, true) => bg.fg(palette.text),
        (false, false) => bg.fg(palette.subtle),
    };

    let mut spans = vec![
        Span::styled(marker, bg.fg(palette.accent)),
        Span::styled(" ", bg),
        Span::styled(format!("{name:<name_width$}"), name_style),
        Span::styled(" ", bg),
        Span::styled(glyph.to_string(), bg.fg(status_color)),
        // Agent count includes the main agent, so it is never zero.
        Span::styled(format!("{:>3}", row.agents), bg.fg(palette.subtle)),
        Span::styled(format!("{:>4}", row.age(now)), bg.fg(palette.subtle)),
    ];

    // Failures and subagent-only activity are the two things worth interrupting
    // a scan of the rail for, so they get their own marks rather than a number
    // column nobody reads.
    if row.failures > 0 {
        spans.push(Span::styled(" ✗", bg.fg(palette.error)));
    } else if row.sidecar_active {
        spans.push(Span::styled(" ·", bg.fg(palette.success)));
    }

    Line::from(spans)
}

/// Which rail row a click at `y` lands on, if any.
///
/// `area` is the rail's INNER rect (what [`render`] recorded), so row 0 is the
/// first session rather than the border.
pub fn row_at(area: Rect, y: u16, rows: usize) -> Option<usize> {
    let index = y.checked_sub(area.y)? as usize;
    (index < rows && index < area.height as usize).then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn row(project: &str, ago: i64, failures: usize, now: DateTime<Utc>) -> RailRow {
        RailRow {
            session_id: "s".to_string(),
            project: project.to_string(),
            main_path: PathBuf::from("/p/s.jsonl"),
            agents: 4,
            failures,
            last_activity: Some(now - chrono::Duration::seconds(ago)),
            sidecar_active: false,
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-08-26T12:00:00Z".parse().unwrap()
    }

    /// The palette the app actually renders with — `graph::new_flow`'s, so a
    /// theme change reaches these tests rather than being asserted around.
    fn palette() -> Palette {
        crate::state::graph::new_flow().theme.palette()
    }

    /// The rendered row must fit the pane exactly — an over-long project name
    /// that pushed the age column off the edge would silently hide the one
    /// number the rail exists to show.
    #[test]
    fn a_row_never_outgrows_the_pane() {
        let n = now();
        let long = row("a-very-long-project-name-indeed", 5, 0, n);
        for width in [16u16, RAIL_WIDTH, 60] {
            let line = row_line(&long, true, width, n, &palette());
            let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                rendered.chars().count() <= width as usize,
                "width {width}: {rendered:?} is {} chars",
                rendered.chars().count()
            );
        }
    }

    #[test]
    fn a_failure_mark_wins_over_the_subagent_mark() {
        let n = now();
        let mut r = row("proj", 5, 2, n);
        r.sidecar_active = true;
        let line = row_line(&r, false, RAIL_WIDTH, n, &palette());
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains('✗'));
        assert!(
            !rendered.contains('·'),
            "one mark per row — a failure is the more urgent of the two"
        );
    }

    #[test]
    fn running_and_idle_use_the_graphs_glyphs() {
        let n = now();
        let live: String = row_line(&row("p", 5, 0, n), false, RAIL_WIDTH, n, &palette())
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        let idle: String = row_line(&row("p", 9999, 0, n), false, RAIL_WIDTH, n, &palette())
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(live.contains(AgentStatus::Running.glyph()));
        assert!(idle.contains(AgentStatus::Idle.glyph()));
    }

    /// Draw the whole UI with a populated rail and return the flattened buffer.
    ///
    /// The unit tests above check one row in isolation; this proves the rail
    /// survives `draw`'s layout — that it is actually reached, gets a pane, and
    /// does not get clipped away by the canvas/panel split.
    fn rendered_ui(app: &mut App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn app_with_two_sessions() -> App {
        let mut app = App::new("sess-a".to_string(), crate::state::Mode::Live);
        let n = Utc::now();
        let mut a = row("alpha", 5, 0, n);
        a.session_id = "sess-a".to_string();
        a.main_path = PathBuf::from("/p/sess-a.jsonl");
        let mut b = row("bravo", 5, 0, n);
        b.session_id = "sess-b".to_string();
        b.main_path = PathBuf::from("/p/sess-b.jsonl");
        app.rail.adopt(vec![a, b]);
        app
    }

    #[test]
    fn the_rail_reaches_the_screen_and_marks_the_watched_session() {
        let mut app = app_with_two_sessions();
        let out = rendered_ui(&mut app, 100, 24);
        assert!(out.contains("alpha"), "rail row missing: {out:?}");
        assert!(out.contains("bravo"));
        assert!(out.contains("sessions 2/2"), "rail header missing");
        assert!(app.rail_area.is_some(), "click target recorded");

        // One session: the rail costs 30 columns to say nothing, so it stays
        // away and the canvas keeps the full width.
        let mut solo = App::new("sess-a".to_string(), crate::state::Mode::Live);
        let n = Utc::now();
        let mut a = row("alpha", 5, 0, n);
        a.session_id = "sess-a".to_string();
        solo.rail.adopt(vec![a]);
        let out = rendered_ui(&mut solo, 100, 24);
        assert!(!out.contains("sessions 1/1"));
        assert!(solo.rail_area.is_none(), "no stale click target");
    }

    /// A terminal too narrow to hold both the rail and a usable canvas keeps
    /// the canvas — the rail is the thing you can live without.
    #[test]
    fn a_narrow_terminal_drops_the_rail() {
        let mut app = app_with_two_sessions();
        let out = rendered_ui(&mut app, 50, 20);
        assert!(!out.contains("alpha"));
        assert!(app.rail_area.is_none());
    }

    #[test]
    fn keys_and_clicks_queue_a_session_switch() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
            MouseEventKind,
        };

        let mut app = app_with_two_sessions();
        rendered_ui(&mut app, 100, 24);

        // `n` steps to the next session and queues it for the tailer; the
        // marker does NOT move yet, because the app is still watching sess-a
        // until the tailer confirms the switch.
        crate::handler::handle_event(
            &Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            &mut app,
        );
        assert_eq!(app.pending_watch, Some(PathBuf::from("/p/sess-b.jsonl")));
        assert_eq!(app.rail.focused_index(&app.current_session_id), Some(0));

        // With two rows, `p` wraps to the same other session as `n` does.
        app.pending_watch = None;
        crate::handler::handle_event(
            &Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            &mut app,
        );
        assert_eq!(app.pending_watch, Some(PathBuf::from("/p/sess-b.jsonl")));

        let area = app.rail_area.expect("rail drawn");
        let click = |app: &mut App, row: u16| {
            crate::handler::handle_event(
                &Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x + 2,
                    row: area.y + row,
                    modifiers: KeyModifiers::NONE,
                }),
                app,
            );
        };

        // Clicking the row already being watched must not queue anything — a
        // switch tears down and rebuilds the whole model.
        app.pending_watch = None;
        click(&mut app, 0);
        assert_eq!(app.pending_watch, None, "no redundant model rebuild");

        // Clicking the other row queues the switch.
        click(&mut app, 1);
        assert_eq!(app.pending_watch, Some(PathBuf::from("/p/sess-b.jsonl")));

        // `w` hides the rail even with two sessions; `v` cycles the layout.
        let press = |app: &mut App, c: char| {
            crate::handler::handle_event(
                &Event::Key(KeyEvent {
                    code: KeyCode::Char(c),
                    modifiers: KeyModifiers::NONE,
                    kind: KeyEventKind::Press,
                    state: crossterm::event::KeyEventState::NONE,
                }),
                app,
            );
        };
        press(&mut app, 'w');
        assert!(!app.rail.should_show());
        press(&mut app, 'w');
        assert!(app.rail.should_show());
    }

    /// Regression: a concurrent-session count used to select a layout that had
    /// no renderer, which silently removed the rail and made `w` look broken.
    /// Nothing but the user and the terminal width may hide it.
    #[test]
    fn concurrent_sessions_never_hide_the_rail() {
        let n = Utc::now();
        let mut app = App::new("sess-0".to_string(), crate::state::Mode::Live);
        let rows: Vec<_> = (0..5)
            .map(|i| {
                let mut r = row(&format!("proj{i}"), 1, 0, n);
                r.session_id = format!("sess-{i}");
                r
            })
            .collect();
        app.rail.adopt(rows);
        assert_eq!(app.rail.running(n), 5);

        let out = rendered_ui(&mut app, 100, 24);
        assert!(out.contains("proj0"), "rail vanished: {out:?}");
        assert!(app.rail_area.is_some());

        // ...and `w` still governs it in both directions.
        app.rail.toggle_visible();
        let out = rendered_ui(&mut app, 100, 24);
        assert!(!out.contains("proj0"));
        app.rail.toggle_visible();
        let out = rendered_ui(&mut app, 100, 24);
        assert!(out.contains("proj0"));
    }

    /// Every color in a row must come from the palette, and every span must
    /// carry the panel background. A hardcoded color would look right in the
    /// stock dark theme and wrong the moment the theme changed — the same
    /// class of bug as the rail and the agent card disagreeing about liveness.
    #[test]
    fn row_colors_come_from_the_palette() {
        let n = now();
        let p = palette();

        let live_row = row("proj", 5, 2, n);
        let live = row_line(&live_row, true, RAIL_WIDTH, n, &p);
        for span in &live.spans {
            assert_eq!(
                span.style.bg,
                Some(p.surface),
                "span {:?} is missing the panel background",
                span.content
            );
            if let Some(fg) = span.style.fg {
                assert!(
                    [p.accent, p.success, p.error, p.subtle, p.text].contains(&fg),
                    "span {:?} uses {fg:?}, which is not in the palette",
                    span.content
                );
            }
        }

        // The specific assignments the graph shares.
        let fg_of = |line: &Line, needle: &str| {
            line.spans
                .iter()
                .find(|s| s.content.contains(needle))
                .and_then(|s| s.style.fg)
        };
        assert_eq!(fg_of(&live, "▸"), Some(p.accent), "focus marker");
        assert_eq!(fg_of(&live, "●"), Some(p.success), "running");
        assert_eq!(fg_of(&live, "✗"), Some(p.error), "failures");

        let idle_row = row("proj", 9999, 0, n);
        let idle = row_line(&idle_row, false, RAIL_WIDTH, n, &p);
        assert_eq!(fg_of(&idle, "◌"), Some(p.subtle), "idle");
    }

    #[test]
    fn clicks_map_to_rows_inside_the_pane_only() {
        let area = Rect::new(0, 5, RAIL_WIDTH, 4);
        assert_eq!(row_at(area, 5, 3), Some(0));
        assert_eq!(row_at(area, 7, 3), Some(2));
        assert_eq!(row_at(area, 8, 3), None, "past the last row");
        assert_eq!(row_at(area, 4, 3), None, "above the pane");
        assert_eq!(row_at(area, 9, 9), None, "past the pane height");
    }
}
