//! Monitor tailers: one per live session that is not the focused one.
//!
//! The canvas shows every live session, so every live session needs its
//! transcripts tailed — not just the one being scrubbed. This module owns that
//! fleet: it sweeps discovery on an interval, decides which sessions deserve a
//! tailer, and starts and stops them as sessions come and go.
//!
//! # Monitors are not the focus tailer
//!
//! A monitored session is folded straight off its live edge into a model that
//! only ever renders — no [`Timeline`](crate::state::timeline::Timeline), no
//! playhead, no seeking. Its events are therefore relabelled
//! ([`UiEvent::MonitorBatch`] / [`UiEvent::MonitorReset`]) before reaching the
//! App, because the focus tailer's `Batch` for an unfamiliar session id means
//! "I switched sessions" — a meaning a monitor must never accidentally speak.
//!
//! Each monitor watches a concrete `<uuid>.jsonl`, which the tailer treats as
//! pinned (`live.rs`: a file target disables the newer-session auto-switch). A
//! monitor following a project's newest session out from under the supervisor
//! would leave the fleet's bookkeeping describing sessions nobody is tailing.

use std::collections::HashMap;

use tokio::sync::{mpsc, watch};

use crate::sessions;
use crate::state::rail::{RailRow, RailStatus};
use crate::tailer::{self, TailRequest, UiEvent};

/// Most sessions monitored at once.
///
/// Each costs a task, a model, and a subtree on the canvas. Real concurrency
/// tops out at a handful — beyond that the canvas is unreadable anyway and the
/// rail is the better view — so the cap protects against a pathological
/// workspace (a scripted fleet, a restored backup) rather than normal use.
/// Sessions beyond it stay in the rail; they are simply not drawn.
pub const MAX_MONITORED: usize = 8;

/// Request-channel capacity for a monitor's tailer. One `Watch` is ever sent.
const REQ_CAP: usize = 1;

/// Event-channel capacity between a monitor's tailer and its relabeller.
const EVENT_CAP: usize = 32;

/// A running monitor. Dropping it closes the request channel, which is how
/// [`tailer::run`] is told to shut down.
struct Monitor {
    _req_tx: mpsc::Sender<TailRequest>,
}

/// Start a tailer for one session and relabel its events for the App.
fn spawn_monitor(row: &RailRow, ui_tx: mpsc::Sender<UiEvent>) -> Monitor {
    let (req_tx, req_rx) = mpsc::channel(REQ_CAP);
    let (mon_tx, mut mon_rx) = mpsc::channel(EVENT_CAP);

    // The tailer itself, live (never replay — a monitor follows an edge).
    tokio::spawn(async move {
        let _ = tailer::run(req_rx, mon_tx, false, 1.0).await;
    });

    // Pin it to this session's file. The channel has room for exactly this.
    let path = row.main_path.clone();
    let req = req_tx.clone();
    tokio::spawn(async move {
        let _ = req.send(TailRequest::Watch(path)).await;
    });

    // Relabel. `ReplayLoaded` cannot occur (live mode) and an `Error` from a
    // monitor is dropped rather than surfaced: it belongs to a session the user
    // is not looking at, and the status bar speaks for the focused one.
    tokio::spawn(async move {
        while let Some(event) = mon_rx.recv().await {
            let relabelled = match event {
                UiEvent::Batch {
                    session_id,
                    updates,
                } => UiEvent::MonitorBatch {
                    session_id,
                    updates,
                },
                UiEvent::SessionReset { session_id } => UiEvent::MonitorReset { session_id },
                _ => continue,
            };
            if ui_tx.send(relabelled).await.is_err() {
                return;
            }
        }
    });

    Monitor { _req_tx: req_tx }
}

/// Which sessions should have a monitor right now.
///
/// **Running only.** The rail lists everything touched in the last day, but the
/// canvas is for what is happening: folding a model and drawing a subtree for a
/// session that stopped hours ago costs real memory and screen to say "this is
/// over". Idle sessions stay one keypress away in the rail.
fn wanted(rows: &[RailRow], focus: &str, now: chrono::DateTime<chrono::Utc>) -> Vec<RailRow> {
    rows.iter()
        .filter(|r| r.status(now) == RailStatus::Running)
        // The focused session already has the focus tailer. A second tailer on
        // it would double every append.
        .filter(|r| r.session_id != focus)
        .take(MAX_MONITORED)
        .cloned()
        .collect()
}

/// Sweep discovery on an interval, publish the rail rows, and keep the monitor
/// fleet matching what is live.
///
/// Runs until the UI channel closes. Subsumes the plain discovery sweep — the
/// rows it publishes and the fleet it manages come from the same scan, so they
/// can never describe different workspaces.
pub async fn supervise(ui_tx: mpsc::Sender<UiEvent>, focused: watch::Receiver<String>) {
    let mut monitors: HashMap<String, Monitor> = HashMap::new();
    let mut ticker = tokio::time::interval(sessions::SWEEP_INTERVAL);

    loop {
        ticker.tick().await;

        // Discovery is blocking and touches every project directory, so it goes
        // on the blocking pool rather than occupying a runtime worker.
        let Ok(rows) =
            tokio::task::spawn_blocking(|| sessions::sweep(sessions::SWEEP_WINDOW)).await
        else {
            // The scan panicked; try again next tick rather than killing the
            // rail and the canvas for the rest of the run.
            continue;
        };

        if ui_tx.send(UiEvent::Sessions(rows.clone())).await.is_err() {
            return;
        }

        let focus = focused.borrow().clone();
        let wanted = wanted(&rows, &focus, chrono::Utc::now());

        // Stop monitors for sessions that went quiet, vanished, or became the
        // focused one — and tell the App to take each off the canvas, or its
        // subtree would linger forever with nothing feeding it.
        let stale: Vec<String> = monitors
            .keys()
            .filter(|id| !wanted.iter().any(|r| &&r.session_id == id))
            .cloned()
            .collect();
        for id in stale {
            monitors.remove(&id);
            if ui_tx
                .send(UiEvent::MonitorReset { session_id: id })
                .await
                .is_err()
            {
                return;
            }
        }

        for row in &wanted {
            if !monitors.contains_key(&row.session_id) {
                monitors.insert(row.session_id.clone(), spawn_monitor(row, ui_tx.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn row(id: &str, ago: i64, now: chrono::DateTime<chrono::Utc>) -> RailRow {
        RailRow {
            session_id: id.to_string(),
            project: "p".to_string(),
            main_path: PathBuf::from(format!("/p/{id}.jsonl")),
            agents: 1,
            tool_calls: 0,
            failures: 0,
            prompts: 0,
            last_activity: Some(now - chrono::Duration::seconds(ago)),
            sidecar_active: false,
        }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        "2026-08-26T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn only_running_unfocused_sessions_are_monitored() {
        let n = now();
        let rows = vec![
            row("focused", 1, n),
            row("live", 5, n),
            // Past the liveness threshold: in the rail, off the canvas.
            row("stale", crate::state::rail::RAIL_IDLE_SECS + 60, n),
        ];
        let ids: Vec<String> = wanted(&rows, "focused", n)
            .into_iter()
            .map(|r| r.session_id)
            .collect();
        assert_eq!(ids, vec!["live".to_string()]);
    }

    #[test]
    fn the_fleet_is_capped() {
        let n = now();
        let rows: Vec<_> = (0..MAX_MONITORED + 5)
            .map(|i| row(&format!("s{i}"), 1, n))
            .collect();
        assert_eq!(wanted(&rows, "none", n).len(), MAX_MONITORED);
    }

    #[test]
    fn an_undated_session_is_never_monitored() {
        let n = now();
        let mut r = row("s", 1, n);
        r.last_activity = None;
        assert!(wanted(&[r], "none", n).is_empty());
    }
}
