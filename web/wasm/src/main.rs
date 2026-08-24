//! zoetrope in the browser — the `zoetrope-web` browser frontend.
//!
//! Replays a bundled demo transcript (no filesystem in the browser) through the
//! same engine the native app uses, rendered with [`ratzilla`]'s WebGl2 backend
//! sized to fill the page. The transcript is compiled in (`include_str!`) —
//! nothing leaves the page.
//!
//! This is its own (unpublished) crate: it depends on `zoetrope` with default
//! features off, so the browser stack (ratzilla, web-sys, wasm-bindgen) stays
//! out of the published library. Built for `wasm32` by trunk — see
//! `web/scripts/build-wasm.sh`.
//!
//! ratzilla events convert to `rataflow`'s through the `From` impls behind
//! rataflow's `ratzilla` feature; drag and wheel-zoom are the two exceptions
//! (see the note at the end of this file).

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;

use ratzilla::backend::webgl2::{FontAtlasConfig, WebGl2BackendOptions};
use ratzilla::event::{
    KeyCode as RKeyCode, KeyEvent as RKeyEvent, MouseButton as RMouseButton,
    MouseEvent as RMouseEvent, MouseEventKind as RMouseKind,
};
use ratzilla::{WebGl2Backend, WebRenderer};
use wasm_bindgen::prelude::*;
use web_time::Instant;

use zoetrope::i18n::Locale;
use zoetrope::state::{App, Camera, Mode};
use zoetrope::tailer::{
    DemoSubagent, Source, UiEvent, Update, replay_from_jsonl, replay_from_session,
};
use zoetrope::transcript::{SubagentMeta, parse_line};

/// The demo session's main transcript, compiled into the wasm binary.
const DEMO_MAIN: &str = include_str!("../../../assets/demo.jsonl");
/// Default replay speed (matches the native default).
const DEMO_SPEED: f64 = 8.0;

/// Bind a subagent's `agent-<id>` stem to its embedded meta + transcript.
macro_rules! demo_subagent {
    ($id:literal) => {
        DemoSubagent {
            agent_id: $id,
            meta: include_str!(concat!(
                "../../../assets/demo/subagents/agent-",
                $id,
                ".meta.json"
            )),
            transcript: include_str!(concat!(
                "../../../assets/demo/subagents/agent-",
                $id,
                ".jsonl"
            )),
            workflow: None,
            journal: false,
        }
    };
}

/// Same, for a subagent under `assets/demo/subagents/workflows/<wf>/`.
macro_rules! demo_workflow_subagent {
    ($wf:literal, $id:literal) => {
        DemoSubagent {
            agent_id: $id,
            meta: include_str!(concat!(
                "../../../assets/demo/subagents/workflows/",
                $wf,
                "/agent-",
                $id,
                ".meta.json"
            )),
            transcript: include_str!(concat!(
                "../../../assets/demo/subagents/workflows/",
                $wf,
                "/agent-",
                $id,
                ".jsonl"
            )),
            workflow: Some($wf),
            journal: false,
        }
    };
}

/// The workflow's `journal.jsonl` — no meta, folds under `Source::Journal`.
macro_rules! demo_workflow_journal {
    ($wf:literal) => {
        DemoSubagent {
            agent_id: "",
            meta: "",
            transcript: include_str!(concat!(
                "../../../assets/demo/subagents/workflows/",
                $wf,
                "/journal.jsonl"
            )),
            workflow: Some($wf),
            journal: true,
        }
    };
}
/// The DOM element the WebGl2 grid fills (see `index.html`).
const CONTAINER: &str = "terminal-container";
/// Rows moved per PageUp/PageDown in the detail panel.
const PAGE_SCROLL: i32 = 10;
/// Session id used for every user-loaded session. Loads replace the whole `App`,
/// and live appends are stamped with the App's own id, so a single constant is
/// enough (there is only ever one session in the page at a time).
const LOADED_SESSION_ID: &str = "session";

thread_local! {
    /// The live `App`, shared with the render loop. `main` stashes the same `Rc`
    /// the `draw_web` closure holds, so the JS-callable loaders below can swap the
    /// `App` in place (replace its contents) or feed it a live `Batch`, and the
    /// next animation frame renders the change. wasm is single-threaded, so these
    /// calls never interleave with a frame mid-borrow.
    static APP: RefCell<Option<Rc<RefCell<App>>>> = const { RefCell::new(None) };
}

fn main() -> io::Result<()> {
    console_error_panic_hook::set_once();

    // Build the App from the bundled session (main + subagents) — the same shape
    // the native replay assembles from disk (UiEvent::ReplayLoaded).
    let subagents = [
        demo_subagent!("a1000000000000001"),
        demo_subagent!("a2000000000000002"),
        demo_subagent!("a3000000000000003"),
        demo_subagent!("a4000000000000004"),
        demo_workflow_subagent!("wf_demo01", "w1000000000000001"),
        demo_workflow_subagent!("wf_demo01", "w2000000000000002"),
        demo_workflow_journal!("wf_demo01"),
    ];
    let (items, info) = replay_from_session(DEMO_MAIN, &subagents);
    let mut app = App::new("demo".to_string(), Mode::Replay);
    app.set_locale(browser_locale());
    app.handle_ui_event(UiEvent::ReplayLoaded {
        session_id: "demo".to_string(),
        items,
        speed: DEMO_SPEED,
        info,
    });

    let app = Rc::new(RefCell::new(app));
    // Stash the shared handle so the JS-callable loaders (`zoetrope_load` /
    // `zoetrope_append`) can reach the same `App` the render loop draws.
    APP.with(|cell| *cell.borrow_mut() = Some(app.clone()));
    let last_tick = Rc::new(RefCell::new(Instant::now()));
    // Last hovered cell — wheel events carry no grid position, so we remember it
    // from pointer moves to anchor zoom.
    let last_cell = Rc::new(Cell::new((0u16, 0u16)));

    // Dynamic font atlas: rasterize glyphs on demand from the browser's own
    // monospace font (canvas 2D) rather than blitting from beamterm's prebuilt
    // static atlas. The static atlas only bakes a fixed set of Unicode ranges, so
    // glyphs outside them (e.g. Dingbats `✓ ✗ ❋`) render blank and some shapes get
    // baked as color emoji. Dynamic mode covers the full Unicode the browser font
    // provides, so zoetrope's status/marker glyphs render as-is. Font stack mirrors
    // `--zoetrope-mono` in the site CSS.
    const MONO: &[&str] = &[
        "ui-monospace",
        "SF Mono",
        "Fira Code",
        "JetBrains Mono",
        "Menlo",
        "Consolas",
        "monospace",
    ];
    // The grid is a whole number of cells, so the container's last partial cell
    // on the right/bottom edge is left as padding. ratzilla clears that strip to
    // black by default, which shows against zoetrope's backdrop as the window
    // resizes (the strip is `container_size % cell_size`). Match it to the flow's
    // canvas background (`Palette::DARK.canvas_bg` = indexed 233, #121212) so the
    // same indexed color resolves to the identical RGB and the strip disappears.
    let backend = WebGl2Backend::new_with_options(
        WebGl2BackendOptions::new()
            .grid_id(CONTAINER)
            .font_atlas_config(FontAtlasConfig::dynamic(MONO, 16.0))
            .canvas_padding_color(ratatui::style::Color::Indexed(233)),
    )?;
    let mut terminal = ratatui::Terminal::new(backend)?;

    let _ = terminal.on_key_event({
        let app = app.clone();
        move |key: RKeyEvent| handle_key(key, &mut app.borrow_mut())
    });

    let _ = terminal.on_mouse_event({
        let app = app.clone();
        let last_cell = last_cell.clone();
        // ratzilla reports moves without button state; track press/release to
        // distinguish a drag (pan / scrubber-seek) from a hover.
        let mut held = false;
        move |ev: RMouseEvent| {
            match ev.kind {
                RMouseKind::ButtonDown(_) => held = true,
                RMouseKind::ButtonUp(_) => held = false,
                RMouseKind::SingleClick(_)
                | RMouseKind::DoubleClick(_)
                | RMouseKind::Entered
                | RMouseKind::Exited => return,
                _ => {}
            }
            last_cell.set((ev.col, ev.row));
            handle_mouse(&ev, held, &mut app.borrow_mut());
        }
    });

    install_wheel(app.clone(), last_cell.clone());

    // ratzilla drives this on requestAnimationFrame; advance the same per-frame
    // ticks the native loop does, then draw.
    terminal.draw_web({
        let app = app.clone();
        let last_tick = last_tick.clone();
        move |frame| {
            let now = Instant::now();
            let elapsed = now.duration_since(*last_tick.borrow());
            *last_tick.borrow_mut() = now;

            let mut app = app.borrow_mut();
            let _ = app.flow.tick_auto_pan(elapsed);
            app.flow.tick_animation(elapsed);
            app.tick_camera(elapsed);
            app.tick_timeline(elapsed);
            app.status_tick();
            zoetrope::ui::draw(frame, &mut app);
        }
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// JS-callable session loaders
//
// The page boots into the bundled demo (above). These let the `/app` route hand
// the engine a session the user picked — an uploaded `.jsonl` (+ its subagent
// sidecars), or, in Chromium, a directory opened via the File System Access API
// and tailed for live appends. The browser has no filesystem of its own, so JS
// reads the bytes and passes them straight in; the engine is the same one the
// native app and the demo use.
// ---------------------------------------------------------------------------

/// One subagent's embedded files, as passed from JS. Mirrors [`DemoSubagent`]
/// but owns its strings (deserialized from the JS-side JSON). For an append,
/// `meta` is `""` once already sent and `transcript` carries only new bytes.
#[derive(serde::Deserialize, Default)]
struct OwnedSub {
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    meta: String,
    #[serde(default)]
    transcript: String,
    /// Set for anything under `subagents/workflows/<id>/` — drives the group
    /// node, matching what the native tailer derives from the directory layout.
    #[serde(default)]
    workflow: Option<String>,
    /// True when `transcript` is a workflow's `journal.jsonl`.
    #[serde(default)]
    journal: bool,
}

/// Parse the JS-side `[{agent_id, meta, transcript}, …]` payload, tolerating an
/// empty string (no subagents) and malformed JSON (→ none) rather than panicking
/// across the wasm boundary.
fn parse_subs(json: &str) -> Vec<OwnedSub> {
    if json.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str(json).unwrap_or_default()
}

/// Detect the UI language from `navigator.language` (falling back through
/// `navigator.languages`): the first tag that maps to a known locale wins,
/// English otherwise. The environment is a stub on wasm, so the browser's own
/// language list is the only signal.
fn browser_locale() -> Locale {
    let Some(nav) = web_sys::window().map(|w| w.navigator()) else {
        return Locale::default();
    };
    let tags: Vec<String> = nav
        .language()
        .into_iter()
        .chain(nav.languages().iter().filter_map(|t| t.as_string()))
        .collect();
    tags.iter()
        .filter_map(|t| Locale::from_tag(t))
        .next()
        .unwrap_or_default()
}

/// Load a whole session into the view, replacing whatever is showing (the demo,
/// or a previously loaded one). `main_text` is the main transcript; `subagents_json`
/// is the (possibly empty) sidecar payload. `live` opens it at the edge in live
/// mode (ready for [`zoetrope_append`]) instead of replaying paced from the start.
#[wasm_bindgen]
pub fn zoetrope_load(main_text: String, subagents_json: String, live: bool) {
    let subs = parse_subs(&subagents_json);
    let sub_refs: Vec<DemoSubagent> = subs
        .iter()
        .map(|s| DemoSubagent {
            agent_id: &s.agent_id,
            meta: &s.meta,
            transcript: &s.transcript,
            workflow: s.workflow.as_deref(),
            journal: s.journal,
        })
        .collect();
    let (items, info) = if sub_refs.is_empty() {
        replay_from_jsonl(&main_text)
    } else {
        replay_from_session(&main_text, &sub_refs)
    };

    let mode = if live { Mode::Live } else { Mode::Replay };
    let mut next = App::new(LOADED_SESSION_ID.to_string(), mode);
    next.handle_ui_event(UiEvent::ReplayLoaded {
        session_id: LOADED_SESSION_ID.to_string(),
        items,
        speed: DEMO_SPEED,
        info,
    });

    APP.with(|cell| {
        if let Some(rc) = cell.borrow().as_ref() {
            *rc.borrow_mut() = next;
        }
    });
}

/// Switch the UI language of the running app (the toolbar's 中/En toggle).
/// Accepts the same tags as `--lang` (`en`, `zh`, `zh-CN`, …); an unknown tag
/// leaves the current language untouched.
#[wasm_bindgen]
pub fn zoetrope_set_lang(tag: String) {
    if let Some(locale) = Locale::from_tag(&tag) {
        APP.with(|cell| {
            if let Some(rc) = cell.borrow().as_ref() {
                rc.borrow_mut().set_locale(locale);
            }
        });
    }
}

/// Feed newly-appended bytes from a live-followed session as one batch (the wasm
/// equivalent of the native tailer's poll tick). `main_tail` is the bytes added
/// to the main transcript since the last call; each entry in `subagents_json`
/// carries a subagent's new transcript bytes (and its `meta` the first time it's
/// seen). Folds onto the edge when following — a no-op if nothing parses.
#[wasm_bindgen]
pub fn zoetrope_append(main_tail: String, subagents_json: String) {
    let mut updates: Vec<Update> = Vec::new();
    for line in main_tail.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(entry) = parse_line(line) {
            updates.push(Update::Entry {
                source: Source::Main,
                entry,
            });
        }
    }
    for sub in parse_subs(&subagents_json) {
        // A workflow journal carries no meta and folds under its own source —
        // mirrors `Source::Journal` in the native tailer. Skip one with no
        // workflow id: there is nothing to attribute it to.
        if sub.journal {
            let Some(wf) = sub.workflow.clone() else {
                continue;
            };
            for line in sub.transcript.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(entry) = parse_line(line) {
                    updates.push(Update::Entry {
                        source: Source::Journal(wf.clone()),
                        entry,
                    });
                }
            }
            continue;
        }
        if !sub.meta.trim().is_empty()
            && let Ok(meta) = serde_json::from_str::<SubagentMeta>(&sub.meta)
        {
            updates.push(Update::SubagentMeta {
                agent_id: sub.agent_id.clone(),
                workflow: sub.workflow.clone(),
                meta,
            });
        }
        for line in sub.transcript.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(entry) = parse_line(line) {
                updates.push(Update::Entry {
                    source: Source::Sub(sub.agent_id.clone()),
                    entry,
                });
            }
        }
    }

    if updates.is_empty() {
        return;
    }
    APP.with(|cell| {
        if let Some(rc) = cell.borrow().as_ref() {
            let mut app = rc.borrow_mut();
            let session_id = app.current_session_id.clone();
            app.handle_ui_event(UiEvent::Batch {
                session_id,
                updates,
            });
        }
    });
}

/// Map a browser key to an app action, mirroring the native handler: app-level
/// transport/camera/overlay keys act directly; navigation/viewport keys forward
/// to the flow.
fn handle_key(key: RKeyEvent, app: &mut App) {
    match key.code {
        // Transport (DVR).
        RKeyCode::Char(' ') => return app.toggle_play_pause(),
        RKeyCode::Char('[') => return app.seek_prompt(false),
        RKeyCode::Char(']') => return app.seek_prompt(true),
        RKeyCode::End | RKeyCode::Char('g') | RKeyCode::Char('G') => return app.go_live(),

        // Pacing: toggle inactivity-skip (compress dead air vs real-time).
        // Presentation-only — mirrors native's `s`.
        RKeyCode::Char('s') | RKeyCode::Char('S') => {
            app.timeline.compress_gaps = !app.timeline.compress_gaps;
            return;
        }

        // Camera (destinations, not toggles).
        RKeyCode::Char('o') | RKeyCode::Char('O') => {
            app.camera = Camera::Overview;
            app.camera_glide = None;
            app.flow.request_fit_view();
            return;
        }
        RKeyCode::Char('f') | RKeyCode::Char('F') => {
            app.camera = Camera::Follow;
            // `track_activity` → `center_node` owns the readable-zoom bump.
            app.track_activity();
            return;
        }
        RKeyCode::Char('r') | RKeyCode::Char('R') => return app.relayout_now(),

        // Overlays.
        RKeyCode::Char('i') | RKeyCode::Char('I') => {
            app.show_info = !app.show_info;
            return;
        }
        // Cycle the UI language. Shift-L: lowercase `l` pans the graph (hjkl).
        RKeyCode::Char('L') => {
            app.cycle_locale();
            return;
        }
        RKeyCode::Char('?') => {
            app.show_help = !app.show_help;
            return;
        }

        // Detail-panel scroll when an agent is selected; else fall through so
        // h/j/k/l pan the graph.
        RKeyCode::Char('j') if scroll_detail(app, 1) => return,
        RKeyCode::Char('k') if scroll_detail(app, -1) => return,
        RKeyCode::PageDown if scroll_detail(app, PAGE_SCROLL) => return,
        RKeyCode::PageUp if scroll_detail(app, -PAGE_SCROLL) => return,

        RKeyCode::Esc => {
            if app.show_help {
                app.show_help = false;
            } else if app.show_info {
                app.show_info = false;
            } else if app.camera != Camera::Follow {
                app.flow.clear_selection();
                app.detail_scroll = 0;
                app.detail_follow = true;
            }
            return;
        }
        _ => {}
    }

    // Forward navigation/viewport keys to the flow (whitelist, as in native).
    let fk: rataflow::KeyEvent = key.clone().into();
    let response = match key.code {
        RKeyCode::Tab | RKeyCode::Up | RKeyCode::Down | RKeyCode::Left | RKeyCode::Right => {
            app.flow.handle_key_event(fk)
        }
        RKeyCode::Char('+' | '=' | '-' | '_' | '0') => app.flow.handle_controls_key_event(fk),
        RKeyCode::Char('h' | 'j' | 'k' | 'l' | 'c') => app.flow.handle_key_event(fk),
        _ => return,
    };
    let events: Vec<_> = response.into_events().collect();
    app.process_flow_events(events.into_iter());
}

/// Scroll the detail panel by `delta`, clamped by the renderer. Returns `true`
/// if an agent is selected (key consumed); `false` lets it fall through to the
/// graph. Mirrors the native handler.
fn scroll_detail(app: &mut App, delta: i32) -> bool {
    if app.selected_agent_id().is_none() {
        return false;
    }
    if delta < 0 {
        app.detail_follow = false;
    }
    app.detail_scroll = (app.detail_scroll as i32 + delta).max(0) as u16;
    true
}

/// Route a mouse event: a press/drag on the scrubber row seeks the playhead
/// (intercepted before the flow sees it); everything else pans/selects the flow.
fn handle_mouse(ev: &RMouseEvent, held: bool, app: &mut App) {
    let pressed = matches!(ev.kind, RMouseKind::ButtonDown(RMouseButton::Left));
    // Events reaching here are ButtonDown/ButtonUp or a move (clicks/enter/exit
    // were filtered upstream), so "a move" is just "not a button event".
    let moving = !matches!(ev.kind, RMouseKind::ButtonDown(_) | RMouseKind::ButtonUp(_));

    if let Some(bar) = app.scrubber_area
        && (pressed || (held && moving))
        && ev.row >= bar.y
        && ev.row < bar.y + bar.height
        && bar.width > 1
    {
        let rel = ev.col.saturating_sub(bar.x).min(bar.width - 1);
        // Queue rather than seek now (same as the native handler): a drag
        // delivers many events per frame and a backward seek rebuilds the
        // whole model — the rAF tick applies only the latest target.
        app.pending_seek = Some(rel as f64 / (bar.width - 1) as f64);
        return;
    }

    let mut me: rataflow::MouseEvent = ev.clone().into();
    // Inject a drag when the button is held during a move (ratzilla moves carry
    // no button), so the flow pans.
    if held && matches!(me.kind, rataflow::MouseEventKind::Moved) {
        me.kind = rataflow::MouseEventKind::Drag(rataflow::MouseButton::Left);
    }
    let events: Vec<_> = app.flow.handle_mouse_event(me).into_events().collect();
    app.process_flow_events(events.into_iter());
}

/// Wheel → zoom the flow at the last hovered cell (ratzilla doesn't surface wheel
/// through `on_mouse_event`, so listen on the container directly).
fn install_wheel(app: Rc<RefCell<App>>, last_cell: Rc<Cell<(u16, u16)>>) {
    let Some(container) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(CONTAINER))
    else {
        return;
    };
    let closure = Closure::<dyn Fn(web_sys::WheelEvent)>::new(move |e: web_sys::WheelEvent| {
        e.prevent_default();
        if e.delta_y() == 0.0 {
            return;
        }
        let (column, row) = last_cell.get();
        let mut app = app.borrow_mut();
        // `handle_wheel` lives in rataflow: it normalizes browser wheel
        // frequency/deltaMode into discrete zoom notches, so wasm zoom matches the
        // native scroll feel instead of racing. (Terminals keep using scroll events.)
        let events: Vec<_> = app
            .flow
            .handle_wheel(e.delta_y(), e.delta_mode(), column, row)
            .into_events()
            .collect();
        app.process_flow_events(events.into_iter());
    });
    let _ = container.add_event_listener_with_callback("wheel", closure.as_ref().unchecked_ref());
    closure.forget();
}

// ratzilla → rataflow event conversion is provided by rataflow's
// `ratzilla` feature (the `From` impls); we use `.into()` at the call sites.
// Drag is still synthesized in `handle_mouse` (ratzilla reports button-less
// moves), and wheel-zoom goes through `Flow::handle_wheel` (see `install_wheel`).
