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
//! playhead, no seeking. That is the ONLY difference between it and the focused
//! session, and it is the App's decision, not the tailer's: every tailer here
//! emits the same `Batch`/`SessionReset` events stamped with a `session_id`, and
//! the App routes on that.
//!
//! Each tailer is pinned to the file it was given and never follows a different
//! one, so the fleet's bookkeeping always describes the sessions actually being
//! tailed.

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

/// A running monitor. Dropping it closes the request channel, which is how
/// [`tailer::run`] is told to shut down.
struct Monitor {
    _req_tx: mpsc::Sender<TailRequest>,
}

/// Start a tailer pinned to one session's transcript.
///
/// Its events go straight to the UI channel: they carry a `session_id`, and the
/// App routes on that alone. There is no relabelling step and no second channel
/// — a monitored session and the focused one differ only in which one the App
/// has decided to focus, which the tailer neither knows nor needs to.
fn spawn_monitor(row: &RailRow, ui_tx: mpsc::Sender<UiEvent>) -> Monitor {
    let (req_tx, req_rx) = mpsc::channel(REQ_CAP);

    // Live, never replay — a monitor follows an edge.
    tokio::spawn(async move {
        let _ = tailer::run(req_rx, ui_tx, false, 1.0).await;
    });

    // Pin it to this session's file. The channel has room for exactly this.
    let path = row.main_path.clone();
    let req = req_tx.clone();
    tokio::spawn(async move {
        let _ = req.send(TailRequest::Watch(path)).await;
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

/// Which monitors to stop, and whether each one's session should also be taken
/// off the canvas (`true`).
///
/// The distinction is the whole point. A monitor stops for two unrelated
/// reasons, and only one of them is a retirement:
///
/// * the session went quiet or vanished — retire it, or its subtree lingers on
///   the canvas forever with nothing feeding it;
/// * the user focused it — do NOT retire it. The App reads a `SessionReset` for
///   the focused session as a truncation and rebuilds it from nothing, dropping
///   the model, the `Timeline` and the snapshot ladder that the focus tailer has
///   just backfilled. Focus can win this race: [`supervise`] wakes on the focus
///   change, but a sweep tick that is ready at the same instant is chosen at
///   random, and it sees the new focus in `wanted` either way.
fn retired(
    monitors: &HashMap<String, Monitor>,
    wanted: &[RailRow],
    focus: &str,
) -> Vec<(String, bool)> {
    monitors
        .keys()
        .filter(|id| !wanted.iter().any(|r| &&r.session_id == id))
        .map(|id| (id.clone(), id != focus))
        .collect()
}

/// Sweep discovery on an interval, publish the rail rows, and keep the monitor
/// fleet matching what is live.
///
/// Runs until the UI channel closes. Subsumes the plain discovery sweep — the
/// rows it publishes and the fleet it manages come from the same scan, so they
/// can never describe different workspaces.
pub async fn supervise(ui_tx: mpsc::Sender<UiEvent>, mut focused: watch::Receiver<String>) {
    let mut monitors: HashMap<String, Monitor> = HashMap::new();
    let mut ticker = tokio::time::interval(sessions::SWEEP_INTERVAL);
    let mut sweeper = sessions::Sweeper::default();

    loop {
        // React to a focus change immediately, not on the next sweep. The App
        // switches focus the moment the user asks; until this fleet notices,
        // the newly focused session still has a monitor on it, and BOTH tailers
        // feed the App events for it — which the App can no longer tell apart,
        // because telling them apart is exactly the distinction this design
        // removed. Waking on the change keeps that window to nothing.
        tokio::select! {
            _ = ticker.tick() => {}
            changed = focused.changed() => {
                if changed.is_err() {
                    return;
                }
                let focus = focused.borrow().clone();
                if monitors.remove(&focus).is_some() {
                    // Its model is the focused one's now; the App replaced it
                    // when it switched, so nothing to tell it here.
                    continue;
                }
                continue;
            }
        }

        // Discovery is blocking and touches every project directory, so it goes
        // on the blocking pool rather than occupying a runtime worker. The
        // sweeper rides along so each tick can skip what it can prove has not
        // changed since the last one.
        let mut owned = sweeper;
        let Ok((returned, rows)) = tokio::task::spawn_blocking(move || {
            let rows = owned.rows(sessions::SWEEP_WINDOW);
            (owned, rows)
        })
        .await
        else {
            // The scan panicked; try again next tick rather than killing the
            // rail and the canvas for the rest of the run. Its cache went with
            // it, so start a cold one.
            sweeper = sessions::Sweeper::default();
            continue;
        };
        sweeper = returned;

        if ui_tx.send(UiEvent::Sessions(rows.clone())).await.is_err() {
            return;
        }

        let focus = focused.borrow().clone();
        let wanted = wanted(&rows, &focus, chrono::Utc::now());

        // Stop monitors for sessions that went quiet, vanished, or became the
        // focused one — and tell the App to take each off the canvas, or its
        // subtree would linger forever with nothing feeding it.
        for (id, retire) in retired(&monitors, &wanted, &focus) {
            monitors.remove(&id);
            if !retire {
                continue;
            }
            if ui_tx
                .send(UiEvent::SessionReset { session_id: id })
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
            failures: 0,
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

    /// A monitor for a session the user just focused must stop WITHOUT a
    /// `SessionReset` — the App reads that as a truncation of the focused
    /// session and throws away the model, timeline and ladder the focus tailer
    /// has just backfilled.
    #[test]
    fn focusing_a_monitored_session_stops_it_without_retiring_it() {
        let n = now();
        let (tx, _rx) = mpsc::channel(1);
        let mut monitors: HashMap<String, Monitor> = HashMap::new();
        monitors.insert(
            "focused".to_string(),
            Monitor {
                _req_tx: tx.clone(),
            },
        );
        monitors.insert("gone".to_string(), Monitor { _req_tx: tx });

        // Neither is wanted: one because it is the focus, one because it died.
        let wanted = wanted(&[row("other", 1, n)], "focused", n);
        let mut out = retired(&monitors, &wanted, "focused");
        out.sort();
        assert_eq!(
            out,
            vec![("focused".to_string(), false), ("gone".to_string(), true)]
        );
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
