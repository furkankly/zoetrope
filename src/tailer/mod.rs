//! Background task: live tailing and replay assembly.
//!
//! A single tailer task owns ALL files of the watched session. It is poll-based
//! (200 ms interval, no `notify` dep): each tick it stats every tracked file,
//! reads appended bytes, splits on `\n`, parses complete lines, and buffers the
//! trailing partial. It asks the session for files that appeared since
//! ([`Session::rescan`](crate::provider::Session::rescan)) each tick. Replay
//! parses everything up front, merges by timestamp, and hands the App the
//! whole stream.
//!
//! Everything is stamped with `session_id`; the UI drops events whose session
//! id is not current (see [`crate::state::App::is_current`]).
//!
//! The feeders know nothing about any format. A [`Target`] goes into
//! [`provider::open`](crate::provider::open), a
//! [`Session`](crate::provider::Session) comes out with a
//! [`Stream`](crate::provider::Stream) per file, and lines go through it.
//!
//! Layout: this module holds the task entry (`run`) and the shared wire types
//! ([`TailRequest`] / [`UiEvent`]); `bytes` is the
//! pure incremental reader, `live` the live poll loop, `replay` the up-front
//! assembly. Both feeders converge on `live::tail_loop` so every session keeps
//! tailing.

#[cfg(feature = "native")]
use std::path::PathBuf;

#[cfg(feature = "native")]
use tokio::sync::mpsc;

use crate::fact::Statement;
#[cfg(feature = "native")]
use crate::provider::Provider;
use crate::provider::Target;

// Portable: the timeline item + its ordering (no IO → compiles on wasm).
mod item;
pub use item::Bundle;
pub use item::ReplayItem;
pub(crate) use item::Timing;
#[cfg(test)]
pub(crate) use item::date_and_sort;
pub(crate) use item::date_and_sort_live;

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
    /// Switch to watching/replaying a session: a file, an id, or the newest
    /// session at a working directory (which is then followed as newer ones
    /// appear).
    Watch(Target),
}

/// Events the tailer task sends to the UI.
#[derive(Debug)]
pub enum UiEvent {
    /// What one live poll tick read, one statement per record (appended to
    /// the timeline's head as they arrive).
    Batch {
        session_id: String,
        statements: Vec<Statement>,
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
    /// File truncation/rotation detected — the UI should reset its model.
    SessionReset { session_id: String },
    /// A non-fatal error string for display.
    Error(String),
}

// ---------------------------------------------------------------------------
// Public task entry point
// ---------------------------------------------------------------------------

/// Run the tailer task: receive [`TailRequest`]s, emit [`UiEvent`]s.
///
/// Lives for the program's duration; switches sessions on
/// [`TailRequest::Watch`]. `replay` selects up-front assembly vs live tailing (the App
/// paces either way); `speed` is the replay speed multiplier (ignored in live mode). `only`
/// forces one provider instead of reading it off the content.
#[cfg(feature = "native")]
pub async fn run(
    mut req_rx: mpsc::Receiver<TailRequest>,
    ui_tx: mpsc::Sender<UiEvent>,
    replay: bool,
    speed: f64,
    only: Option<Provider>,
) -> anyhow::Result<()> {
    // Wait for the first Watch before doing anything (Watch is the only request).
    let Some(target) = wait_for_watch(&mut req_rx).await else {
        return Ok(());
    };
    let mut current = Flow::from_watch(target);

    loop {
        let Flow::Switch { target, follow } = current else {
            return Ok(());
        };
        current = if replay {
            run_replay(&target, only, &ui_tx, &mut req_rx, speed).await
        } else {
            run_live(&target, follow, only, &ui_tx, &mut req_rx).await
        };
    }
}

/// What to do after a live/replay session loop returns.
#[cfg(feature = "native")]
pub(crate) enum Flow {
    /// Switch to a new target (live auto-switch, a re-attach, or a `Watch`
    /// request). `follow` is the working directory whose newest session is
    /// being followed, if any; a named file or id pins.
    Switch {
        target: Target,
        follow: Option<PathBuf>,
    },
    /// The request channel closed — shut down.
    Exit,
}

#[cfg(feature = "native")]
impl Flow {
    /// A `Watch` as a flow: a directory target is followed, anything else pins.
    pub(crate) fn from_watch(target: Target) -> Flow {
        let follow = match &target {
            Target::Here(cwd) => Some(cwd.clone()),
            Target::Path(_) | Target::Id(_) => None,
        };
        Flow::Switch { target, follow }
    }
}

/// Block until the first [`TailRequest::Watch`].
#[cfg(feature = "native")]
async fn wait_for_watch(req_rx: &mut mpsc::Receiver<TailRequest>) -> Option<Target> {
    match req_rx.recv().await? {
        TailRequest::Watch(target) => Some(target),
    }
}
