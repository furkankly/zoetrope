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

use zoetrope::state::{App, Camera, Mode};
use zoetrope::tailer::{Bundle, UiEvent};
use zoetrope::theme::ThemeId;

/// `localStorage` key the theme selection is saved under (the page's boot
/// script in `app.astro` reads the same key).
const WEB_STORAGE_KEY: &str = "zoetrope-theme";

/// Default replay speed (matches the native default).
const DEMO_SPEED: f64 = 8.0;

/// A subagent transcript as `(path, text)`, at its path under the demo session.
macro_rules! demo_subagent {
    ($id:literal) => {
        (
            concat!("demo/subagents/agent-", $id, ".jsonl"),
            include_str!(concat!("../../../assets/claude/demo/subagents/agent-", $id, ".jsonl")),
        )
    };
}
/// Its `meta.json` sidecar.
macro_rules! demo_meta {
    ($id:literal) => {
        (
            concat!("demo/subagents/agent-", $id, ".meta.json"),
            include_str!(concat!("../../../assets/claude/demo/subagents/agent-", $id, ".meta.json")),
        )
    };
}
/// Same, for a subagent under `subagents/workflows/<wf>/`.
macro_rules! demo_workflow_subagent {
    ($wf:literal, $id:literal) => {
        (
            concat!("demo/subagents/workflows/", $wf, "/agent-", $id, ".jsonl"),
            include_str!(concat!(
                "../../../assets/claude/demo/subagents/workflows/",
                $wf,
                "/agent-",
                $id,
                ".jsonl"
            )),
        )
    };
}
macro_rules! demo_workflow_meta {
    ($wf:literal, $id:literal) => {
        (
            concat!("demo/subagents/workflows/", $wf, "/agent-", $id, ".meta.json"),
            include_str!(concat!(
                "../../../assets/claude/demo/subagents/workflows/",
                $wf,
                "/agent-",
                $id,
                ".meta.json"
            )),
        )
    };
}
/// The demo session, compiled into the wasm binary as the files it is on disk,
/// under the paths the native discovery would see them at. The same
/// `Bundle` that loads a user's session loads this one.
const DEMO_FILES: &[(&str, &str)] = &[
    ("demo.jsonl", include_str!("../../../assets/claude/demo.jsonl")),
    demo_subagent!("a1000000000000001"),
    demo_meta!("a1000000000000001"),
    demo_subagent!("a2000000000000002"),
    demo_meta!("a2000000000000002"),
    demo_subagent!("a3000000000000003"),
    demo_meta!("a3000000000000003"),
    demo_subagent!("a4000000000000004"),
    demo_meta!("a4000000000000004"),
    demo_workflow_subagent!("wf_demo01", "w1000000000000001"),
    demo_workflow_meta!("wf_demo01", "w1000000000000001"),
    demo_workflow_subagent!("wf_demo01", "w2000000000000002"),
    demo_workflow_meta!("wf_demo01", "w2000000000000002"),
    (
        "demo/subagents/workflows/wf_demo01/journal.jsonl",
        include_str!("../../../assets/claude/demo/subagents/workflows/wf_demo01/journal.jsonl"),
    ),
];
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
    /// The loaded session's per-file streams, so live appends continue where
    /// the load stopped (a Codex stream learns whose file it is from the first
    /// line; a fresh one per append would know nothing).
    static BUNDLE: RefCell<Option<Bundle>> = const { RefCell::new(None) };
}

fn main() -> io::Result<()> {
    console_error_panic_hook::set_once();

    // Build the App from the bundled session, the way a dropped one is built.
    let boot_theme = stored_theme();
    ACTIVE_THEME.with(|c| c.set(boot_theme));
    let (bundle, items, info) = Bundle::load(DEMO_FILES).expect("the bundled demo is a session");
    let mut app = App::new("demo".to_string(), Mode::Replay);
    app.set_theme(boot_theme);
    app.handle_ui_event(UiEvent::ReplayLoaded {
        session_id: "demo".to_string(),
        items,
        speed: DEMO_SPEED,
        info,
    });
    BUNDLE.with(|cell| *cell.borrow_mut() = Some(bundle));

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
    // on the right/bottom edge is left as padding, cleared to the backend's
    // `canvas_padding_color`. That colour is fixed at creation, so the boot
    // theme's is set here and `paint_padding` follows later theme switches.
    let [pr, pg, pb] = boot_theme.canvas_rgb();
    let backend = WebGl2Backend::new_with_options(
        WebGl2BackendOptions::new()
            .grid_id(CONTAINER)
            .font_atlas_config(FontAtlasConfig::dynamic(MONO, 16.0))
            .canvas_padding_color(ratatui::style::Color::Rgb(pr, pg, pb)),
    )?;
    let mut terminal = ratatui::Terminal::new(backend)?;

    let _ = terminal.on_key_event({
        let app = app.clone();
        move |key: RKeyEvent| {
            let before = current_theme();
            handle_key(key, &mut app.borrow_mut());
            // The `App` borrow is released: now the page may react. Notifying
            // while it was held re-entered the wasm export and panicked.
            if current_theme() != before {
                notify_theme();
            }
        }
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
            paint_padding(app.theme);
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

/// One file as passed from JS: the path it came with (what discovery would
/// see) and its text. For an append, `text` carries only the new bytes of a
/// file already loaded, or the whole of a file seen for the first time. The
/// page does not say what a file is; the engine reads that off the path and
/// the content.
#[derive(serde::Deserialize, Default)]
struct OwnedFile {
    #[serde(default)]
    path: String,
    #[serde(default)]
    text: String,
}

/// Parse the JS-side `[{path, text}, …]` payload, tolerating an empty string
/// and malformed JSON (→ none) rather than panicking across the wasm boundary.
fn parse_files(json: &str) -> Vec<OwnedFile> {
    if json.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str(json).unwrap_or_default()
}

/// Load a whole session into the view, replacing whatever is showing (the demo,
/// or a previously loaded one). `files_json` is every file of the session as
/// `[{path, text}]`; the provider is read off the content. `live` opens it at
/// the edge in live mode (ready for [`zoetrope_append`]) instead of replaying
/// paced from the start. Returns a JSON summary, `{provider, session, files}`,
/// or `{error}` when no file is a session's root.
#[wasm_bindgen]
pub fn zoetrope_load(files_json: String, live: bool) -> String {
    let owned = parse_files(&files_json);
    let files: Vec<(&str, &str)> = owned
        .iter()
        .map(|f| (f.path.as_str(), f.text.as_str()))
        .collect();
    let Some((bundle, items, info)) = Bundle::load(&files) else {
        return r#"{"error":"no transcript any provider reads, or no session root among the files"}"#
            .to_string();
    };
    let summary = serde_json::json!({
        "provider": bundle.provider().name(),
        "session": bundle.session(),
        "files": bundle.file_count(),
        "accepted": bundle.accepted(),
    })
    .to_string();

    let mode = if live { Mode::Live } else { Mode::Replay };
    let mut next = App::new(LOADED_SESSION_ID.to_string(), mode);
    next.set_theme(current_theme());
    next.handle_ui_event(UiEvent::ReplayLoaded {
        session_id: LOADED_SESSION_ID.to_string(),
        items,
        speed: DEMO_SPEED,
        info,
    });

    BUNDLE.with(|cell| *cell.borrow_mut() = Some(bundle));
    APP.with(|cell| {
        if let Some(rc) = cell.borrow().as_ref() {
            *rc.borrow_mut() = next;
        }
    });
    summary
}

/// Feed newly-read bytes from a live-followed session as one batch (the wasm
/// equivalent of the native tailer's poll tick): `[{path, text}]`, each the
/// bytes added to that file since the last call, or a whole file seen for the
/// first time. Folds onto the edge when following. Returns `{accepted}`, the
/// whole-read files stated so far, so the page knows which ones to stop
/// resending; a sidecar caught mid-write is not in it until it parses.
#[wasm_bindgen]
pub fn zoetrope_append(files_json: String) -> String {
    let owned = parse_files(&files_json);
    let files: Vec<(&str, &str)> = owned
        .iter()
        .map(|f| (f.path.as_str(), f.text.as_str()))
        .collect();
    let (statements, accepted) = BUNDLE.with(|cell| {
        let mut b = cell.borrow_mut();
        match b.as_mut() {
            Some(b) => (b.append(&files), b.accepted()),
            None => (Vec::new(), Vec::new()),
        }
    });
    let summary = serde_json::json!({ "accepted": accepted }).to_string();
    if statements.is_empty() {
        return summary;
    }
    APP.with(|cell| {
        if let Some(rc) = cell.borrow().as_ref() {
            let mut app = rc.borrow_mut();
            let session_id = app.current_session_id.clone();
            app.handle_ui_event(UiEvent::Batch {
                session_id,
                statements,
            });
        }
    });
    summary
}

/// What a file is, from the path it came with and its first line: the same
/// answer the engine gives itself, so the page's session picker never
/// re-implements a format's rules. Returns `{provider, session, root}` as
/// JSON, or `{}` when no provider recognises the head.
#[wasm_bindgen]
pub fn zoetrope_session_file(path: String, head: String) -> String {
    use zoetrope::provider::{FileRole, provider_of};
    let Some(p) = provider_of(&head) else {
        return "{}".to_string();
    };
    match p.session_file_from(std::path::Path::new(&path), &head) {
        Some(f) => serde_json::json!({
            "provider": p.name(),
            "session": f.session,
            "root": f.role == FileRole::Root,
            "project": f.project_key,
        })
        .to_string(),
        None => "{}".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Theme
//
// The page owns the `<select>`; the engine owns the palette. The selection
// lives in `localStorage` under `WEB_STORAGE_KEY`, read here at boot and by the
// page's own boot script. A switch swaps only `flow.theme`: session, camera,
// selection and playhead are untouched. The canvas padding strip (the partial
// cell at the right/bottom edge) is cleared by the backend to a colour fixed at
// creation; `paint_padding` re-asserts the live theme's colour each frame on
// the same GL context.
// ---------------------------------------------------------------------------

/// The theme saved in `localStorage`, or the default when absent, unreadable
/// or not a registered id.
fn stored_theme() -> ThemeId {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(WEB_STORAGE_KEY).ok().flatten())
        .and_then(|v| ThemeId::parse(&v))
        .unwrap_or_default()
}

fn store_theme(theme: ThemeId) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = s.set_item(WEB_STORAGE_KEY, theme.id());
    }
}

thread_local! {
    /// The grid canvas's WebGL2 context, looked up once.
    static GL: RefCell<Option<web_sys::WebGl2RenderingContext>> = const { RefCell::new(None) };
    /// The theme the page and the canvas show. The one source JS-facing reads
    /// use, so `zoetrope_themes` never borrows the `App`: it runs from inside
    /// the `zoetrope-theme` event, which a key handler fires after releasing
    /// the `App`, and must stay safe even if that ever changes.
    static ACTIVE_THEME: Cell<ThemeId> = const { Cell::new(ThemeId::DEFAULT) };
}

/// The theme the running page shows.
fn current_theme() -> ThemeId {
    ACTIVE_THEME.with(Cell::get)
}

/// Set the GL clear colour to `theme`'s canvas background so the padding strip
/// follows a live theme switch. Called each frame, before ratzilla flushes.
///
/// Why raw GL is safe: beamterm caches the clear colour and only issues
/// `glClearColor` when the requested colour differs from its cache. Its request
/// is always the boot colour, which equals its cache, so it never overwrites
/// ours. After a context restore the cache resets and its first frame re-issues
/// the boot colour once (a one-frame flash of a strip under one cell wide).
fn paint_padding(theme: ThemeId) {
    GL.with(|cell| {
        let mut gl = cell.borrow_mut();
        if gl.is_none() {
            *gl = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.query_selector("#terminal-container canvas").ok().flatten())
                .and_then(|c| c.dyn_into::<web_sys::HtmlCanvasElement>().ok())
                .and_then(|c| c.get_context("webgl2").ok().flatten())
                .and_then(|o| o.dyn_into::<web_sys::WebGl2RenderingContext>().ok());
        }
        if let Some(gl) = gl.as_ref() {
            let [r, g, b] = theme.canvas_rgb();
            gl.clear_color(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0);
        }
    });
}

/// Apply `theme` to `app`, record it as active and save it. Does not notify
/// the page: callers do that with [`notify_theme`] once no `App` borrow is held.
fn apply_theme(app: &mut App, theme: ThemeId) {
    app.set_theme(theme);
    ACTIVE_THEME.with(|c| c.set(theme));
    store_theme(theme);
}

/// Tell the page (a `zoetrope-theme` event on `window`) so its `<select>` and
/// CSS follow. Listeners run synchronously and read only [`current_theme`].
fn notify_theme() {
    if let Some(w) = web_sys::window()
        && let Ok(ev) = web_sys::Event::new("zoetrope-theme")
    {
        let _ = w.dispatch_event(&ev);
    }
}

/// Switch theme by registered id. Returns `{theme}` on success or
/// `{error, known}` for an unknown name (the selection is left unchanged).
#[wasm_bindgen]
pub fn zoetrope_set_theme(name: String) -> String {
    let Some(theme) = ThemeId::parse(&name) else {
        return serde_json::json!({ "error": format!("unknown theme {name:?}"), "known": ThemeId::known() })
            .to_string();
    };
    // Clone the handle so no thread-local borrow is held while the page reacts.
    let rc = APP.with(|cell| cell.borrow().clone());
    if let Some(rc) = rc {
        apply_theme(&mut rc.borrow_mut(), theme);
        notify_theme();
    }
    serde_json::json!({ "theme": theme.id() }).to_string()
}

/// The active theme id plus the registry, as `{theme, themes:[{id,label}]}`,
/// so the page builds its `<select>` from the engine instead of a copy.
#[wasm_bindgen]
pub fn zoetrope_themes() -> String {
    let themes: Vec<_> = ThemeId::ALL
        .iter()
        .map(|t| serde_json::json!({ "id": t.id(), "label": t.label() }))
        .collect();
    serde_json::json!({ "theme": current_theme().id(), "themes": themes }).to_string()
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

        // Theme: `t` next, `T` previous (mirrors native). The page is notified
        // by the key callback, after this `App` borrow ends.
        RKeyCode::Char(c @ ('t' | 'T')) => {
            let theme = app.theme.next(c == 'T');
            return apply_theme(app, theme);
        }

        // Overlays.
        RKeyCode::Char('i') | RKeyCode::Char('I') => {
            app.show_info = !app.show_info;
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
