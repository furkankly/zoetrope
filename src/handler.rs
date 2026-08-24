//! Input routing.
//!
//! App-level keys (`q`/`ctrl-c` quit, `space` play/pause, `[`/`]` step + `End`/`g`
//! go-live transport, `s` pacing, `o`/`f`/`r` camera, `i`/`?` overlays) mutate `app`
//! directly; everything else is
//! forwarded to the flow — first `handle_controls_key_event` (zoom/fit), then
//! `handle_key_event` (selection nav), and mouse to `handle_mouse_event`. Flow
//! events are consumed via `into_events`; the graph is read-only so only
//! selection state matters (read during render), but events are still drained.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use rataflow::EventResponse;

use crate::state::{App, Camera};

/// Route one crossterm event. Returns `true` if the app should quit. Mutates
/// `app` only — every key path is in-process state (no channel).
pub fn handle_event(event: &Event, app: &mut App) -> bool {
    match event {
        Event::Key(key) => handle_key(key, app),
        Event::Mouse(mouse) => {
            // A press/drag on the scrubber row seeks the playhead — intercept it
            // before the flow sees it (else it reads as a pane drag → pan).
            if let Some(bar) = app.scrubber_area
                && mouse.row >= bar.y
                && mouse.row < bar.y + bar.height
                && bar.width > 1
                && matches!(
                    mouse.kind,
                    MouseEventKind::Down(MouseButton::Left)
                        | MouseEventKind::Drag(MouseButton::Left)
                )
            {
                let rel = mouse.column.saturating_sub(bar.x).min(bar.width - 1);
                // Queue rather than seek now: a drag delivers many events per
                // frame and a backward seek rebuilds the whole model — the tick
                // applies only the latest target, once per frame.
                app.pending_seek = Some(rel as f64 / (bar.width - 1) as f64);
                return false;
            }
            // rataflow re-exports ratatui's crossterm; types unify, so the
            // `From<crossterm::event::MouseEvent>` impl applies directly.
            let response = app.flow.handle_mouse_event(*mouse);
            process_flow_events(app, response.into_events());
            false
        }
        _ => false,
    }
}

/// Handle a single key event, returning `true` to quit.
fn handle_key(key: &KeyEvent, app: &mut App) -> bool {
    // Some terminals emit both Press and Release; act on Press (and the legacy
    // empty kind) only, so a single keystroke isn't handled twice.
    if matches!(key.kind, KeyEventKind::Release) {
        return false;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        // Quit.
        KeyCode::Char('q') | KeyCode::Char('Q') => {
            app.should_quit = true;
            return true;
        }
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl => {
            app.should_quit = true;
            return true;
        }

        // Detail-panel tool-call list scrolling (only meaningful when an agent
        // is selected). `j`/`k` and PageDown/PageUp move the offset; clamped to
        // the list length so long lists (30–190+ calls) are fully reachable.
        // Arrow keys are intentionally left to the flow for graph navigation.
        KeyCode::Char('j') => {
            if scroll_detail(app, 1) {
                return false;
            }
        }
        KeyCode::Char('k') => {
            if scroll_detail(app, -1) {
                return false;
            }
        }
        KeyCode::PageDown => {
            if scroll_detail(app, PAGE_SCROLL) {
                return false;
            }
        }
        KeyCode::PageUp => {
            if scroll_detail(app, -PAGE_SCROLL) {
                return false;
            }
        }

        // Camera keys name destinations, not toggles: `o` overview (auto-fit
        // everything), `f` follow (readable zoom, track the latest activity).
        // The only ways out of Manual.
        KeyCode::Char('o') | KeyCode::Char('O') => {
            app.camera = Camera::Overview;
            app.camera_glide = None; // fit-view owns the viewport now
            // Camera is orthogonal to layout: frame what's there, never reflow.
            app.flow.request_fit_view();
            return false;
        }
        KeyCode::Char('f') | KeyCode::Char('F') => {
            app.camera = Camera::Follow;
            // `track_activity` → `center_node` owns the readable-zoom bump.
            app.track_activity();
            return false;
        }

        // Tidy the graph: layout is user-driven (new nodes never auto-reflow,
        // which read as jumpy), so `r` runs Sugiyama on demand and reframes.
        KeyCode::Char('r') | KeyCode::Char('R') => {
            app.relayout_now();
            return false;
        }

        // Timeline scrubbing (DVR). `[`/`]` step the playhead to the prev/next
        // prompt-era boundary; `End`/`g`/`G` re-pin to the live/replay edge (`g`
        // is the letter alias, vim-style "go to end", that also works in the
        // browser where `End` can be unreliable).
        KeyCode::Char('[') => {
            app.seek_prompt(false);
            return false;
        }
        KeyCode::Char(']') => {
            app.seek_prompt(true);
            return false;
        }
        KeyCode::End | KeyCode::Char('g') | KeyCode::Char('G') => {
            app.go_live();
            return false;
        }

        // Toggle inactivity-skip: compress dead air (default) vs faithful
        // real-time pacing. Presentation-only — never touches content.
        KeyCode::Char('s') | KeyCode::Char('S') => {
            app.timeline.compress_gaps = !app.timeline.compress_gaps;
            return false;
        }

        // Help overlay.
        KeyCode::Char('?') => {
            app.show_help = !app.show_help;
            return false;
        }

        // Session-info overlay (untimed metadata: mode, perms, last prompt, …).
        KeyCode::Char('i') | KeyCode::Char('I') => {
            app.show_info = !app.show_info;
            return false;
        }

        // Cycle the UI language. Shift-L: lowercase `l` pans the graph (hjkl).
        KeyCode::Char('L') => {
            app.cycle_locale();
            return false;
        }

        // Close the help overlay, else the detail panel (clear selection).
        // Without this there is no way out of the panel: pane-clicks
        // deliberately don't deselect (drag-friendly) and the whitelist
        // blocks the library's own bindings.
        KeyCode::Esc => {
            if app.show_help {
                app.show_help = false;
            } else if app.show_info {
                app.show_info = false;
            } else if app.camera != Camera::Follow {
                // Close the detail panel. In Follow esc is a no-op: the panel is
                // part of Follow's contract (it auto-narrates the active agent),
                // and a user selection would have dropped Follow to Manual anyway.
                app.flow.clear_selection();
                app.detail_scroll = 0;
                app.detail_follow = true;
            }
            return false;
        }

        // Unified play/pause: freeze when playing, or resume from the current
        // cursor when paused or scrubbed into the past. Works in both intents.
        KeyCode::Char(' ') => {
            app.toggle_play_pause();
            return false;
        }

        _ => {}
    }

    // Whitelist policy: the graph is read-only, so only navigation and
    // viewport keys reach the flow. Falling through by default would expose
    // destructive/stateful library bindings — Delete/Backspace removes the
    // selected node (re-added on the next sync with a layout jump), 'i'
    // silently toggles the viewport lock, 'm' toggles multi-select — none of
    // which zoetrope surfaces or wants.
    let response = match key.code {
        // Selection navigation: sequential + spatial. The flow is configured with
        // `SelectionReveal::None` (see `graph::new_flow`), so selection changes
        // without the library moving the camera — zoetrope's center-glide
        // (pending_center → center_node) is the sole, smooth camera move.
        KeyCode::Tab
        | KeyCode::BackTab
        | KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right => app.flow.handle_key_event(*key),
        // Viewport: zoom in/out/reset (controls bindings).
        KeyCode::Char('+' | '=' | '-' | '_' | '0') => app.flow.handle_controls_key_event(*key),
        // Viewport: vim panning (j/k reach here only with no agent selected —
        // the detail-panel scroll consumed them above otherwise) and
        // center-on-selected.
        KeyCode::Char('h' | 'j' | 'k' | 'l' | 'c') => app.flow.handle_key_event(*key),
        _ => EventResponse::NotHandled,
    };
    process_flow_events(app, response.into_events());
    false
}

/// Rows moved per PageUp/PageDown in the detail panel's tool-call list.
const PAGE_SCROLL: i32 = 10;

/// Scroll the detail panel's tool-call list by `delta` rows, clamped to the
/// selected agent's tool-call count. Returns `true` if an agent was selected
/// (so the key is consumed and not forwarded to the flow); `false` lets the key
/// fall through to graph navigation when no panel is shown.
fn scroll_detail(app: &mut App, delta: i32) -> bool {
    if app.selected_agent_id().is_none() {
        return false;
    }
    // Scrolling up detaches the tail; scrolling down to the bottom re-attaches.
    // The renderer owns the upper clamp (it alone knows the panel height and the
    // true line count incl. era headers) and writes the resolved offset back.
    if delta < 0 {
        app.detail_follow = false;
    }
    app.detail_scroll = (app.detail_scroll as i32 + delta).max(0) as u16;
    true
}

/// Drain and react to the flow events produced by an input handler call.
///
/// Intentionally minimal — selection is read during render, so only side
/// effects that aren't render-derived are acted on here: the
/// detail-panel scroll resets when the selection changes, and any user-driven
/// viewport change (pan keys, wheel zoom, drag pan) hands the camera to the
/// user.
pub fn process_flow_events(app: &mut App, events: impl Iterator<Item = rataflow::FlowEvent>) {
    // The logic lives on `App` so the browser frontend shares it; this stays as
    // the native handler's call surface.
    app.process_flow_events(events);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Mode;
    use crossterm::event::MouseEvent;
    use rataflow::FlowEvent;

    /// An App with a 20-column scrubber at (x=2, rows 5..8) over a 4-item replay.
    fn scrubber_app() -> App {
        use crate::tailer::{ReplayItem, Source, UiEvent, Update};
        let item = |uuid: &str, t: &str| {
            let line = format!(
                r#"{{"type":"user","uuid":"{uuid}","parentUuid":null,"timestamp":"{t}","message":{{"role":"user","content":"x"}}}}"#
            );
            ReplayItem::at(
                Some(t.parse().unwrap()),
                Update::Entry {
                    source: Source::Main,
                    entry: crate::transcript::parse_line(&line).unwrap(),
                },
            )
        };
        let mut app = App::new("s".into(), Mode::Replay);
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "s".into(),
            items: vec![
                item("u1", "2026-06-05T10:00:00.000Z"),
                item("u2", "2026-06-05T10:00:01.000Z"),
                item("u3", "2026-06-05T10:00:02.000Z"),
                item("u4", "2026-06-05T10:00:03.000Z"),
            ],
            speed: 8.0,
            info: Default::default(),
        });
        app.scrubber_area = Some(ratatui::layout::Rect::new(2, 5, 20, 3));
        app
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn scrubber_click_maps_columns_to_fractions() {
        let mut app = scrubber_app();
        let down = |col, row| mouse(MouseEventKind::Down(MouseButton::Left), col, row);

        // Leftmost column → fraction 0.0.
        handle_event(&down(2, 5), &mut app);
        assert_eq!(app.pending_seek, Some(0.0));

        // Rightmost column (x + width - 1 = 21) → fraction 1.0.
        handle_event(&down(21, 6), &mut app);
        assert_eq!(app.pending_seek, Some(1.0));

        // Past the right edge clamps to 1.0 (no overshoot, no panic).
        handle_event(&down(60, 7), &mut app);
        assert_eq!(app.pending_seek, Some(1.0));

        // A row BELOW the bar is not a seek — it forwards to the flow.
        app.pending_seek = None;
        handle_event(&down(10, 8), &mut app);
        assert_eq!(app.pending_seek, None, "off-bar clicks must not seek");
    }

    #[test]
    fn scrubber_drag_burst_coalesces_to_one_seek_per_tick() {
        let mut app = scrubber_app();
        let drag = |col| mouse(MouseEventKind::Drag(MouseButton::Left), col, 6);

        // Ride to the edge first so a backward drag exercises the rebuild path.
        app.go_live();
        assert_eq!(app.timeline.folded, 4);

        // A burst of drag events within one frame: only the LAST target is
        // queued; nothing rebuilds until the tick.
        handle_event(&drag(15), &mut app);
        handle_event(&drag(9), &mut app);
        handle_event(&drag(2), &mut app);
        assert_eq!(app.pending_seek, Some(0.0));
        assert_eq!(app.timeline.folded, 4, "no rebuild before the tick");

        // The tick applies exactly one seek — to the latest target.
        app.tick_timeline(std::time::Duration::ZERO);
        assert_eq!(app.pending_seek, None);
        assert_eq!(
            app.timeline.folded,
            app.timeline.fold_at_fraction(0.0),
            "one coalesced seek lands on the last drag target"
        );
    }

    #[test]
    fn viewport_change_hands_camera_to_user() {
        let mut app = App::new("s".into(), Mode::Live);
        assert_eq!(app.camera, Camera::Overview);
        process_flow_events(
            &mut app,
            vec![FlowEvent::ViewportChanged {
                x: 1.0,
                y: 2.0,
                zoom: 1.5,
            }]
            .into_iter(),
        );
        assert_eq!(app.camera, Camera::Manual);
    }

    #[test]
    fn selection_nav_leaves_the_camera_to_our_glide() {
        use ratatui::widgets::Widget;
        let mk = |id: &str, x: f64| {
            rataflow::Node::new(
                id,
                (x, 0.0),
                (10.0, 5.0),
                crate::ui::nodes::AgentNode {
                    locale: crate::i18n::Locale::En,
                    title: id.into(),
                    description: None,
                    status: crate::state::session::AgentStatus::Running,
                    tool_count: 0,
                    last_tool: None,
                    output_tokens: 0,
                    interactive: false,
                },
            )
        };
        let mut app = App::new("s".into(), Mode::Live);
        // Two nodes far apart so the second is off-screen when we focus the first.
        app.flow.add_node(mk("a", 0.0)).unwrap();
        app.flow.add_node(mk("b", 500.0)).unwrap();
        // Render so the flow has a canvas, then zoom/focus tightly on "a" so "b"
        // is well off-screen (the library WOULD pan to reveal it).
        let area = ratatui::layout::Rect::new(0, 0, 60, 20);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        (&mut app.flow).render(area, &mut buf);
        app.flow.zoom_to(5.0);
        app.flow.center_on((5.0, 2.5));
        app.flow.select_node("a");
        let before = (app.flow.viewport.x, app.flow.viewport.y);

        // Tab to "b": the flow is `SelectionReveal::None`, so the selection moves
        // WITHOUT the library touching the camera — zoetrope's center-glide (queued
        // via pending_center) is the sole, smooth camera move.
        handle_event(
            &Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut app,
        );

        assert_eq!(
            (app.flow.viewport.x, app.flow.viewport.y),
            before,
            "a selection key must leave the viewport to our glide (library reveal is off)"
        );
        assert_eq!(
            app.pending_center.as_deref(),
            Some("b"),
            "the newly-selected node is queued for a smooth center-glide instead"
        );
    }

    #[test]
    fn destructive_and_stateful_library_keys_are_inert() {
        let mut app = App::new("s".into(), Mode::Live);
        // A selected, default-flags node — deletable=true in the library.
        let node = rataflow::Node::new(
            "a",
            (0.0, 0.0),
            (10.0, 5.0),
            crate::ui::nodes::AgentNode {
                locale: crate::i18n::Locale::En,
                title: "a".into(),
                description: None,
                status: crate::state::session::AgentStatus::Running,
                tool_count: 0,
                last_tool: None,
                output_tokens: 0,
                interactive: false,
            },
        );
        app.flow.add_node(node).unwrap();
        app.flow.select_node("a");

        // Delete must NOT remove the selected node (read-only graph).
        let del = Event::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        handle_event(&del, &mut app);
        assert!(app.flow.node("a").is_some(), "Delete must be inert");

        // 'i' must NOT toggle the viewport lock.
        let i = Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        handle_event(&i, &mut app);
        assert!(!app.flow.locked, "'i' must not silently lock the viewport");
    }

    #[test]
    fn esc_closes_the_detail_panel() {
        let mut app = App::new("s".into(), Mode::Live);
        let node = rataflow::Node::new(
            "a",
            (0.0, 0.0),
            (10.0, 5.0),
            crate::ui::nodes::AgentNode {
                locale: crate::i18n::Locale::En,
                title: "a".into(),
                description: None,
                status: crate::state::session::AgentStatus::Running,
                tool_count: 0,
                last_tool: None,
                output_tokens: 0,
                interactive: false,
            },
        );
        app.flow.add_node(node).unwrap();
        app.flow.select_node("a");
        assert!(app.selected_agent_id().is_some());

        let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        handle_event(&esc, &mut app);
        assert!(
            app.selected_agent_id().is_none(),
            "esc must clear selection (close the panel)"
        );
    }

    #[test]
    fn dragging_a_node_drops_follow_to_manual() {
        // Regression: node drag emits NodeDragged (not SelectionChanged /
        // ViewportChanged), which earlier left Follow engaged. Any gesture drops.
        let mut app = App::new("s".into(), Mode::Live);
        app.camera = Camera::Follow;
        process_flow_events(
            &mut app,
            vec![FlowEvent::NodeDragged {
                node_id: "a".into(),
            }]
            .into_iter(),
        );
        assert_eq!(app.camera, Camera::Manual, "node drag must drop Follow");
    }

    #[test]
    fn selecting_in_overview_stays_in_overview() {
        // Overview yields only to viewport changes — selecting a node to inspect
        // it shouldn't break auto-framing.
        let mut app = App::new("s".into(), Mode::Live);
        assert_eq!(app.camera, Camera::Overview);
        process_flow_events(
            &mut app,
            vec![FlowEvent::SelectionChanged {
                node_ids: vec!["a".into()],
                edge_ids: vec![],
            }]
            .into_iter(),
        );
        assert_eq!(
            app.camera,
            Camera::Overview,
            "selection must not drop Overview"
        );
    }

    #[test]
    fn user_selection_drops_follow_to_manual() {
        let mut app = App::new("s".into(), Mode::Live);
        app.camera = Camera::Follow;
        app.camera_glide = Some(crate::state::CameraGlide {
            from: (0.0, 0.0),
            to: (10.0, 0.0),
            t: 0.1,
        });

        // A USER selection gesture (click or spatial nav) drops Follow so the
        // camera stops chasing activity and gliding back over the selection.
        process_flow_events(
            &mut app,
            vec![FlowEvent::SelectionChanged {
                node_ids: vec!["a".into()],
                edge_ids: vec![],
            }]
            .into_iter(),
        );
        assert_eq!(app.camera, Camera::Manual, "user selection drops Follow");
        assert!(
            app.camera_glide.is_none(),
            "the glide-back is cancelled too"
        );
        assert_eq!(
            app.pending_center.as_deref(),
            Some("a"),
            "the selected node is queued for a center-glide on the next draw"
        );
    }

    #[test]
    fn deselection_clears_a_pending_center() {
        let mut app = App::new("s".into(), Mode::Live);
        app.pending_center = Some("a".into());
        process_flow_events(
            &mut app,
            vec![FlowEvent::SelectionChanged {
                node_ids: vec![],
                edge_ids: vec![],
            }]
            .into_iter(),
        );
        assert!(
            app.pending_center.is_none(),
            "deselecting must not center a stale node"
        );
    }

    #[test]
    fn r_key_tidies_layout_on_demand() {
        let mut app = App::new("s".into(), Mode::Live);
        // Growth has accumulated but layout is never automatic now.
        app.layout_dirty = true;

        let r = Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(!handle_event(&r, &mut app));
        assert!(
            !app.layout_dirty,
            "r must apply the pending relayout (user-driven tidy)"
        );
    }

    #[test]
    fn camera_keys_name_destinations() {
        let mut app = App::new("s".into(), Mode::Live);
        app.camera = Camera::Manual;

        let f = Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(!handle_event(&f, &mut app));
        assert_eq!(app.camera, Camera::Follow);

        let o = Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        app.layout_dirty = true; // growth accumulated
        assert!(!handle_event(&o, &mut app));
        assert_eq!(app.camera, Camera::Overview);
        assert!(
            app.layout_dirty,
            "camera keys are orthogonal to layout: o must NOT relayout (only r does)"
        );
    }
}
