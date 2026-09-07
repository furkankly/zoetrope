//! Replay assembly (native).
//!
//! Parse every file of a session up front, merge by timestamp into one ordered
//! [`ReplayItem`] stream, hand it to the App in a single [`UiEvent::ReplayLoaded`]
//! — then keep tailing the files via [`tail_loop`](super::live::tail_loop) (a
//! replayed file that grows "goes live" on its own). Pacing/seeking live in the
//! App's `Timeline`. The portable item + ordering live in [`super::item`].

use std::path::Path;

use tokio::sync::mpsc;

use crate::provider::{Provider, ReadMode, Session, Stream, Target, open};

use super::item::{ReplayItem, date_and_sort};
use super::live::{LiveSession, SnapshotSeed, tail_loop};
use super::{Flow, TailRequest, UiEvent};

/// Replay feeder: open the session, parse everything on disk, merge by
/// timestamp, hand the whole (sorted) stream to the App in one
/// [`UiEvent::ReplayLoaded`] — then KEEP TAILING. Completion is unknowable (any
/// session can be resumed), so a replayed file is never assumed finished: if it
/// grows, the new appends flow in and the session "goes live" on its own.
///
/// Returns [`Flow::Switch`] on a `Watch`, [`Flow::Exit`] if the channel closes.
pub(crate) async fn run_replay(
    target: &Target,
    only: Option<Provider>,
    ui_tx: &mpsc::Sender<UiEvent>,
    req_rx: &mut mpsc::Receiver<TailRequest>,
    speed: f64,
) -> Flow {
    let session = match open(target, only) {
        Ok(s) => s,
        Err(e) => {
            let _ = ui_tx.send(UiEvent::Error(e.to_string())).await;
            return match req_rx.recv().await {
                Some(TailRequest::Watch(t)) => Flow::from_watch(t),
                None => Flow::Exit,
            };
        }
    };
    let session_id = session.id.clone();

    // The up-front full parse reads every session file synchronously (the 2MB
    // transcript takes ~0.3s). Run it on a blocking thread so it never stalls a
    // runtime worker — robust even on a single-threaded runtime flavor.
    let owned = session.clone();
    let (items, info, seed) = match tokio::task::spawn_blocking(move || build_replay(&owned)).await
    {
        Ok(loaded) => loaded,
        // A panic in the parse task would otherwise `unwrap_or_default()` into an
        // empty session indistinguishable from a genuinely empty one — surface it.
        Err(e) => {
            let _ = ui_tx
                .send(UiEvent::Error(format!("failed to load session: {e}")))
                .await;
            return Flow::Exit;
        }
    };

    let speed = if speed > 0.0 { speed } else { 1.0 };

    if ui_tx
        .send(UiEvent::ReplayLoaded {
            session_id: session_id.clone(),
            items,
            speed,
            info,
        })
        .await
        .is_err()
    {
        return Flow::Exit;
    }

    // Keep tailing the SAME session for appends. A replay target is pinned (you
    // asked for this session), so no newer-session auto-switch. Tail offsets
    // are seeded from the byte positions the bulk parse actually consumed — NOT
    // the current EOF — so lines appended while the bulk parse ran are emitted,
    // not skipped.
    let mut live = LiveSession::new(session, None, only);
    live.seed(seed);

    tail_loop(live, session_id, ui_tx, req_rx).await
}

/// Build the merged, timestamp-ordered replay item list from all of a
/// session's files, plus the [`SnapshotSeed`] recording how far into each file
/// the parse read (so the follow-up tail resumes exactly there).
///
/// Parsing order within a file is preserved; the global sort is stable so
/// entries with equal (or missing, via predecessor) timestamps keep their
/// relative order. Missing-timestamp entries inherit the previous entry's
/// timestamp *within their own file* before the merge sort, so they ride along
/// with their predecessor.
pub(crate) fn build_replay(
    session: &Session,
) -> (Vec<ReplayItem>, crate::state::SessionInfo, SnapshotSeed) {
    let mut items: Vec<ReplayItem> = Vec::new();
    let mut seed = SnapshotSeed::default();
    let p = session.provider;

    for file in session.every_file() {
        match file.read {
            ReadMode::Tail => {
                let mut stream = p.stream_for(file);
                let consumed = parse_file_into(&file.path, &mut stream, &mut items);
                seed.offsets.insert(file.path.clone(), consumed);
                seed.streams.insert(file.path.clone(), stream);
            }
            // A whole-read sidecar states its facts once it parses; one that
            // does not yet (mid-write) is left for the follow-up tail to retry.
            ReadMode::Whole => {
                if let Ok(text) = std::fs::read_to_string(&file.path)
                    && let Some(statement) = p.sidecar(file, &text)
                {
                    items.push(ReplayItem::new(statement));
                    seed.seen_whole.insert(file.path.clone());
                }
            }
        }
    }

    // Route untimed session-level metadata into the info store and DROP it
    // from the timeline — it isn't activity, and being untimed it would clump at
    // the front. Done here, in file (chronological) order, so latest-wins holds.
    let mut info = crate::state::SessionInfo::default();
    items.retain_mut(|item| item.take_session_meta(&mut info));

    // Date the undated items (sidecar births, ledger endings) and stably sort.
    date_and_sort(&mut items);

    (items, info, seed)
}

/// Parse all complete lines of a file into [`ReplayItem`]s through `stream`.
/// Returns the number of bytes consumed — up to and including the last
/// newline; a trailing newline-less fragment is a mid-write line, left for the
/// follow-up tail to emit once its newline lands.
fn parse_file_into(path: &Path, stream: &mut Stream, items: &mut Vec<ReplayItem>) -> u64 {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    let consumed = bytes
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let text = String::from_utf8_lossy(&bytes[..consumed]);
    items.extend(
        text.lines()
            .filter_map(|l| stream.push(l))
            .map(ReplayItem::new),
    );
    consumed as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::FactKind;
    use crate::state::session::MAIN_ID;

    #[test]
    fn replay_dates_metas_to_first_subagent_entry() {
        use std::io::Write;

        let mut dir = std::env::temp_dir();
        dir.push(format!("zoetrope_replay_order_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let session = "22222222-2222-2222-2222-222222222222";
        let sub_dir = dir.join(session).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();

        // Main: one entry BEFORE the subagent starts, one after.
        let main = dir.join(format!("{session}.jsonl"));
        std::fs::File::create(&main)
            .unwrap()
            .write_all(
                concat!(
                    r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"start"}}"#, "\n",
                    r#"{"type":"user","uuid":"u3","parentUuid":"u1","timestamp":"2026-06-05T10:02:00.000Z","message":{"role":"user","content":"later"}}"#, "\n",
                )
                .as_bytes(),
            )
            .unwrap();

        // Subagent: first entry at 10:01, with its meta sidecar.
        std::fs::File::create(sub_dir.join("agent-aaaaaaaaaaaaaaaaa.jsonl"))
            .unwrap()
            .write_all(
                concat!(
                    r#"{"type":"user","uuid":"s1","parentUuid":null,"isSidechain":true,"agentId":"aaaaaaaaaaaaaaaaa","timestamp":"2026-06-05T10:01:00.000Z","message":{"role":"user","content":"task"}}"#, "\n",
                )
                .as_bytes(),
            )
            .unwrap();
        std::fs::File::create(sub_dir.join("agent-aaaaaaaaaaaaaaaaa.meta.json"))
            .unwrap()
            .write_all(br#"{"agentType":"guide","toolUseId":"t1"}"#)
            .unwrap();

        let session = open(&Target::Path(main.clone()), None).unwrap();
        assert_eq!(session.files.len(), 2, "agent transcript + meta");
        let (items, _info, seed) = build_replay(&session);
        assert!(
            seed.seen_whole
                .contains(&sub_dir.join("agent-aaaaaaaaaaaaaaaaa.meta.json"))
        );
        let pos = |pred: &dyn Fn(&ReplayItem) -> bool| items.iter().position(pred);

        let meta_pos = pos(&|i| {
            i.any(|f| {
                matches!(f.kind, FactKind::Agent { .. }) && f.agent.as_deref() != Some(MAIN_ID)
            })
        })
        .unwrap();
        let first_main = pos(&|i| i.any(|f| f.agent.as_deref() == Some(MAIN_ID))).unwrap();
        let sub_entry = pos(&|i| {
            i.any(|f| {
                f.agent.as_deref() == Some("aaaaaaaaaaaaaaaaa")
                    && !matches!(f.kind, FactKind::Agent { .. })
            })
        })
        .unwrap();

        // The meta is dated to the subagent's first entry (10:01): after the
        // 10:00 main entry, before the agent's own entry (tie → meta first).
        assert!(
            items[meta_pos].ts().is_some(),
            "meta must inherit a timestamp"
        );
        assert!(
            first_main < meta_pos,
            "agent must NOT spawn before main starts"
        );
        assert!(
            meta_pos < sub_entry,
            "meta must precede the agent's entries"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
