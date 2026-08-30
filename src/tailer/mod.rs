//! Background task: live tailing and replay pacing.
//!
//! A single tailer task owns ALL files of the watched session. It is poll-based
//! (200 ms interval, no `notify` dep): each tick it stats every tracked file,
//! reads appended bytes, splits on `\n`, parses complete lines, and buffers the
//! trailing partial. It scans `subagents/` and `subagents/workflows/*/` each
//! tick for newly created files. Replay parses everything up front, merges by
//! timestamp, and emits wall-clock-paced batches.
//!
//! Everything is stamped with `session_id`; the UI drops events whose session
//! id is not current (see [`crate::state::App::is_current`]).
//!
//! Layout: this module holds the task entry (`run`) and the shared wire types
//! ([`TailRequest`] / [`UiEvent`] / [`Update`] / [`Source`]); `bytes` is the
//! pure incremental reader, `live` the live poll loop, `replay` the up-front
//! assembly. Both feeders converge on `live::tail_loop` so every session keeps
//! tailing.

use std::path::PathBuf;

#[cfg(feature = "native")]
use tokio::sync::mpsc;

use crate::transcript::{Entry, SubagentMeta};

// Portable: the timeline item + its ordering (no IO → compiles on wasm).
mod item;
pub use item::ReplayItem;
pub(crate) use item::Timing;
#[cfg(test)]
pub(crate) use item::date_and_sort;
pub(crate) use item::date_and_sort_live;
pub use item::{DemoSubagent, replay_from_jsonl, replay_from_session};

// Native-only feeders: incremental byte reading, live polling, replay assembly —
// they pull tokio + the filesystem, so the `native` feature gates them out of the
// portable core (the browser frontend feeds bytes straight in, no tailing).
#[cfg(feature = "native")]
mod bytes;
#[cfg(feature = "native")]
mod live;
#[cfg(feature = "native")]
mod replay;

#[cfg(feature = "native")]
use live::run_live;
#[cfg(feature = "native")]
use replay::run_replay;

/// Requests the UI sends to the tailer task.
///
/// The App owns the playhead (unified Timeline model), so the tailer is a pure
/// feeder — its only request is which session to watch.
#[derive(Debug, Clone)]
pub enum TailRequest {
    /// Switch to watching/replaying a session. In live mode the tailer
    /// discovers the session file under the project dir; in replay it is the
    /// explicit transcript path.
    Watch(PathBuf),
}

/// Events the tailer task sends to the UI.
#[derive(Debug)]
pub enum UiEvent {
    /// A batch of updates produced in one live poll tick (appended to the
    /// timeline's head as they arrive).
    Batch {
        session_id: String,
        updates: Vec<Update>,
    },
    /// The whole merged, timestamp-ordered replay stream, handed to the App
    /// once. The App owns pacing/seeking from here (the tailer does not pace).
    /// `info` carries the untimed session-level metadata (kept off the timeline).
    ReplayLoaded {
        session_id: String,
        items: Vec<ReplayItem>,
        speed: f64,
        info: crate::state::SessionInfo,
    },
    /// File truncation/rotation detected for `session_id` — the UI should drop
    /// what it has of that session.
    ///
    /// Always about the named session and nothing else. It used to be able to
    /// mean "the focused session is now a different one" (the tailer followed a
    /// newer file on its own), which made an unfamiliar id ambiguous and forced
    /// a parallel set of monitor-only events. Which session is focused is now
    /// the App's decision, so routing on `session_id` alone is unambiguous.
    SessionReset { session_id: String },
    /// A completed multi-session discovery sweep. Carries finished rail rows
    /// (see [`crate::sessions::sweep`]), never raw paths: discovery is native
    /// and blocking, the App is neither, and the rail is portable state.
    Sessions(Vec<crate::state::rail::RailRow>),
    /// A non-fatal error string for display.
    Error(String),
}

/// A single unit of parsed transcript activity inside a [`UiEvent::Batch`].
#[derive(Debug)]
pub enum Update {
    /// A parsed transcript entry, tagged with which file it came from.
    Entry { source: Source, entry: Entry },
    /// A subagent `meta.json` sidecar was discovered/parsed.
    SubagentMeta {
        agent_id: String,
        /// `Some(wf_id)` if this is a workflow subagent.
        workflow: Option<String>,
        meta: SubagentMeta,
    },
}

/// Which file an [`Update::Entry`] originated from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The main `<session-uuid>.jsonl`.
    Main,
    /// A subagent file, keyed by its 17-hex-char `agentId`.
    Sub(String),
    /// A workflow `journal.jsonl`, keyed by its workflow id.
    Journal(String),
}

// ---------------------------------------------------------------------------
// Public task entry point
// ---------------------------------------------------------------------------

/// Run the tailer task: receive [`TailRequest`]s, emit [`UiEvent`]s.
///
/// Lives for the program's duration; switches sessions on
/// [`TailRequest::Watch`]. `replay` selects live-tail vs timestamp-paced replay;
/// `speed` is the replay speed multiplier (ignored in live mode).
#[cfg(feature = "native")]
pub async fn run(
    mut req_rx: mpsc::Receiver<TailRequest>,
    ui_tx: mpsc::Sender<UiEvent>,
    replay: bool,
    speed: f64,
) -> anyhow::Result<()> {
    // Wait for the first Watch before doing anything (Watch is the only request).
    let mut current = match wait_for_watch(&mut req_rx).await {
        Some(path) => path,
        None => return Ok(()),
    };

    loop {
        let next = if replay {
            run_replay(&current, &ui_tx, &mut req_rx, speed).await
        } else {
            run_live(&current, &ui_tx, &mut req_rx).await
        };

        match next {
            Flow::Switch(path) => current = path,
            Flow::Exit => return Ok(()),
        }
    }
}

/// What to do after a live/replay session loop returns.
#[cfg(feature = "native")]
pub(crate) enum Flow {
    /// Switch to a new file: a `Watch` request, or a re-attach after the
    /// watched file was truncated or rotated.
    Switch(PathBuf),
    /// The request channel closed — shut down.
    Exit,
}

/// Block until the first [`TailRequest::Watch`].
#[cfg(feature = "native")]
async fn wait_for_watch(req_rx: &mut mpsc::Receiver<TailRequest>) -> Option<PathBuf> {
    match req_rx.recv().await? {
        TailRequest::Watch(path) => Some(path),
    }
}
