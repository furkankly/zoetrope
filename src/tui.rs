//! Terminal lifecycle and the central event loop.
//!
//! A single-task UI loop with `tick_animation` for marching-ant edge animation.
//! Installs mouse capture and a panic hook that also disables mouse capture
//! (ratatui's default hook restores the screen but not mouse capture). Draws
//! every iteration so animation never freezes; drains both channels after the
//! `select!` so input never lags.

use std::io::stdout;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::handler;
use crate::state::App;
use crate::tailer::{TailRequest, UiEvent};
use crate::ui;

/// Frame/tick cadence. 16 ms ≈ 60 fps so marching-ant edges stay smooth.
const TICK: Duration = Duration::from_millis(16);

/// Interval for re-deriving time-based agent status (see `App::status_tick`).
const STATUS_TICK: Duration = Duration::from_secs(1);

/// Run the TUI to completion.
///
/// Owns the terminal, spawns the crossterm `EventStream` reader into an
/// unbounded channel, ticks at 16 ms, and routes UI events / input each
/// iteration. Returns when the user quits.
pub async fn run(
    mut app: App,
    // Kept open for the whole run: the tailer treats a closed channel as
    // "exit". Also carries session switches from the rail — the tailer's own
    // auto-switch remains internal to it.
    tail_tx: mpsc::Sender<TailRequest>,
    mut ui_rx: mpsc::Receiver<UiEvent>,
    // Publishes which session is focused, so the monitor supervisor does not
    // put a second tailer on it (which would double every append).
    focus_tx: tokio::sync::watch::Sender<String>,
) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    install_panic_hook();

    // Crossterm event reader → unbounded channel (input must never block the
    // tailer's bounded channel, and bursts of mouse motion must not be dropped).
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        use futures::StreamExt;
        let mut reader = crossterm::event::EventStream::new();
        while let Some(Ok(event)) = reader.next().await {
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });

    let mut tick = tokio::time::interval(TICK);
    let mut last_tick = Instant::now();
    let mut last_status_tick = Instant::now();

    // Recording-only scripted pointer (see crate::autopilot). Off unless
    // ZOETROPE_DEMO=1, so this is inert for every real user. Armed at startup
    // but only fired by the trigger key, so the tape picks the moment — the
    // waypoints are read off the laid-out graph at that instant.
    let demo = crate::autopilot::requested();
    let mut pilot: Option<crate::autopilot::Autopilot> = None;

    let result = loop {
        // Advance animation/auto-pan EVERY iteration before drawing — otherwise
        // marching-ant edges freeze (the tick_auto_pan return is ignored).
        let now = Instant::now();
        let elapsed = now - last_tick;
        let _ = app.flow.tick_auto_pan(elapsed);
        app.flow.tick_animation(elapsed);
        app.tick_camera(elapsed);
        // Advance the replay playhead (paces replay; no-op while following a
        // live edge, where folding happens as Batches arrive).
        app.tick_timeline(elapsed);
        last_tick = now;

        // Drive the scripted pointer through the REAL event path, so hit
        // testing, the drag threshold, and the scrubber intercept all run.
        if let Some(p) = pilot.as_mut() {
            for ev in p.tick(elapsed) {
                handler::handle_event(&ev, &mut app);
            }
        }

        // Interactive liveness (main/forks) is time-derived: a quiet session
        // produces no batches, so running→idle transitions need their own
        // clock. ~1s granularity is plenty for a minutes-scale idle window.
        if now - last_status_tick >= STATUS_TICK {
            app.status_tick();
            last_status_tick = now;
        }

        // Draw at the top of the loop, every iteration, so animation is smooth
        // and state changes from the previous iteration are reflected.
        let cursor = pilot.as_ref().map(|p| p.cell());
        if let Err(e) = terminal.draw(|frame| {
            ui::draw(frame, &mut app);
            // Painted after the UI so the pointer sits above what it points at.
            if let (Some(p), Some(_)) = (pilot.as_ref(), cursor) {
                p.draw(frame.buffer_mut());
            }
        }) {
            break Err(e.into());
        }

        tokio::select! {
            _ = tick.tick() => {}
            Some(ev) = ui_rx.recv() => app.handle_ui_event(ev),
            Some(ev) = event_rx.recv() => {
                if route(&ev, &mut app, &mut pilot, demo) {
                    break Ok(());
                }
            }
        }

        // Drain both channels after the select so neither input nor tailer
        // batches lag behind a single per-frame wakeup.
        let mut quit = false;
        while let Ok(ev) = event_rx.try_recv() {
            if route(&ev, &mut app, &mut pilot, demo) {
                quit = true;
                break;
            }
        }
        while let Ok(ev) = ui_rx.try_recv() {
            app.handle_ui_event(ev);
        }

        // A rail focus change reaches the tailer here: input handling is
        // synchronous and owns no channel, so it queues the request instead
        // (same shape as `pending_seek`). `try_send` keeps the render loop
        // non-blocking; on a full channel the request stays queued and goes out
        // next frame rather than being dropped, because a swallowed switch
        // leaves the rail marker pointing at a session nobody is watching.
        if let Some(path) = app.pending_watch.take()
            && tail_tx.try_send(TailRequest::Watch(path.clone())).is_err()
        {
            app.pending_watch = Some(path);
        }

        // The focus can change without any input — the tailer's own
        // auto-switch moves it — so publish from the loop rather than from the
        // key handler.
        if *focus_tx.borrow() != app.current_session_id {
            let _ = focus_tx.send(app.current_session_id.clone());
        }

        if quit || app.should_quit {
            break Ok(());
        }
    };

    // Clean restore regardless of how the loop ended.
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Install a panic hook that disables mouse capture before delegating to
/// ratatui's screen-restoring hook, so a panic leaves the terminal usable.
///
/// Called after [`fn@ratatui::init`] (which installs the screen-restore hook), so
/// our layer wraps it: we disable mouse capture first, then chain to the prior
/// hook which leaves raw mode / the alternate screen.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        prev(info);
    }));
}

/// Route one input event, returning whether the app should quit.
///
/// Both the `select!` arm and the post-select drain go through here so the
/// demo trigger cannot depend on which path an event arrives by.
fn route(
    event: &crossterm::event::Event,
    app: &mut App,
    pilot: &mut Option<crate::autopilot::Autopilot>,
    demo: bool,
) -> bool {
    // Arm the scripted pointer. Waypoints are read off the CURRENT layout, so
    // the script adapts to whatever the graph settled into.
    if demo && pilot.is_none() && crate::autopilot::is_trigger(event) {
        let start = app
            .scrubber_area
            .map_or((40, 10), |b| (b.x + b.width / 2, b.y.saturating_sub(6)));
        *pilot = Some(crate::autopilot::Autopilot::new(
            start,
            crate::autopilot::tour(app),
        ));
        return false;
    }
    // While the pilot drives, a stray keypress in the recording terminal must
    // not desync the script. Its own synthesized keys bypass this — they go
    // straight to the handler from the tick loop.
    if pilot.is_some() && crate::autopilot::is_key_press(event) {
        return false;
    }
    handler::handle_event(event, app)
}
