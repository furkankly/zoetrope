//! Live tailing.
//!
//! One poll loop ([`tail_loop`]) per session: each tick stat the main file and
//! every tracked non-main file, read appended bytes, scan `subagents/` +
//! `subagents/workflows/*/` for new files, and emit a [`UiEvent::Batch`]. Both
//! feeders end here — live after the announce, replay after the bulk hand-off —
//! so every session keeps tailing and can pick up new appends ("go live").

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::transcript::{self, SubagentMeta};

use super::bytes::{ReadResult, TailState, read_appended};
use super::{Flow, Source, TailRequest, UiEvent, Update};

/// Poll interval for live tailing.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Tracks all files belonging to one live session.
pub(crate) struct LiveSession {
    /// The main `<uuid>.jsonl` path.
    main_path: PathBuf,
    /// The `<uuid>/subagents` directory (may not exist yet).
    subagents_dir: PathBuf,
    /// Tail state for the main file.
    main_state: TailState,
    /// Tail state per discovered non-main file, keyed by absolute path, carrying
    /// the [`Source`] to stamp its entries with. One map (paths are unique) so no
    /// agent id can collide across the direct-subagent / workflow / journal trees.
    tracked: HashMap<PathBuf, (Source, TailState)>,
    /// meta.json files already emitted (by absolute path) — emit once.
    seen_meta: std::collections::HashSet<PathBuf>,
    /// Byte offsets captured by a replay bulk snapshot, consumed when each file
    /// is first registered for tailing — so the tail resumes exactly where the
    /// snapshot stopped reading instead of at the live EOF (which would drop
    /// lines appended during the bulk parse).
    seed_offsets: HashMap<PathBuf, u64>,
}

/// Per-file read positions and already-emitted metas captured by the replay
/// bulk snapshot, used to seed a [`LiveSession`] via [`LiveSession::seed`].
#[derive(Debug, Default)]
pub(crate) struct SnapshotSeed {
    /// Bytes of each file consumed by the bulk parse (up to its last newline).
    pub(crate) offsets: HashMap<PathBuf, u64>,
    /// meta.json sidecars already emitted in the bulk stream.
    pub(crate) seen_meta: std::collections::HashSet<PathBuf>,
}

impl LiveSession {
    /// Build a session rooted at a concrete `<uuid>.jsonl` main file inside
    /// A session is pinned to the file it was asked to watch: which session is
    /// focused is the App's decision, made from discovery, not something the
    /// tailer may change underneath it.
    pub(crate) fn new(main_path: PathBuf) -> Self {
        let subagents_dir = transcript::subagents_dir(&main_path).unwrap_or_default();
        Self {
            main_path,
            subagents_dir,
            main_state: TailState::default(),
            tracked: HashMap::new(),
            seen_meta: std::collections::HashSet::new(),
            seed_offsets: HashMap::new(),
        }
    }

    fn session_id(&self) -> String {
        transcript::session_id_from_path(&self.main_path)
    }

    /// Adopt a replay bulk snapshot's read positions: the main offset applies
    /// immediately, non-main offsets when each file is first registered, and
    /// bulk-emitted metas are marked seen. Everything the snapshot did NOT
    /// consume — appends during the parse, files created since — is emitted by
    /// the subsequent tail polls.
    pub(crate) fn seed(&mut self, seed: SnapshotSeed) {
        if let Some(off) = seed.offsets.get(&self.main_path) {
            self.main_state.offset = *off;
        }
        self.seed_offsets = seed.offsets;
        self.seen_meta = seed.seen_meta;
    }

    /// Register a non-main file for tailing (idempotent), starting from its
    /// snapshot-seeded offset if one was captured.
    fn track(&mut self, path: PathBuf, source: Source) {
        if self.tracked.contains_key(&path) {
            return;
        }
        let mut state = TailState::default();
        if let Some(off) = self.seed_offsets.remove(&path) {
            state.offset = off;
        }
        self.tracked.insert(path, (source, state));
    }
}

/// Resolve a live watch target into `(project_dir, main_file)`.
///
/// The target is either a project directory (live mode discovers the latest
/// `<uuid>.jsonl` under it) or a concrete session file (its parent is the
/// project dir). Returns `None` if the target is a directory with no session
/// file yet — the caller polls until one appears.
pub(crate) fn resolve_live_target(target: &Path) -> Option<(PathBuf, PathBuf)> {
    if target.is_dir() {
        let main = transcript::latest_session_file(target)?;
        Some((target.to_path_buf(), main))
    } else {
        let project_dir = target.parent().map(Path::to_path_buf).unwrap_or_default();
        Some((project_dir, target.to_path_buf()))
    }
}

/// Block until a session file exists under `project_dir`, polling every
/// [`POLL_INTERVAL`]. Stays responsive to requests; returns the discovered main
/// file, or a [`Flow`] outcome if a switch/exit request arrives first.
async fn await_first_session(
    project_dir: &Path,
    req_rx: &mut mpsc::Receiver<TailRequest>,
) -> Result<PathBuf, Flow> {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if let Some(main) = transcript::latest_session_file(project_dir) {
            return Ok(main);
        }
        tokio::select! {
            req = req_rx.recv() => match req {
                Some(TailRequest::Watch(p)) => return Err(Flow::Switch(p)),
                None => return Err(Flow::Exit),
            },
            _ = ticker.tick() => {}
        }
    }
}

/// Live-tail loop for a single watch target until a switch or exit.
///
/// `target` is either a project directory (the session file is discovered, and
/// re-discovered for newer sessions) or a concrete `<uuid>.jsonl` file.
pub(crate) async fn run_live(
    target: &Path,
    ui_tx: &mpsc::Sender<UiEvent>,
    req_rx: &mut mpsc::Receiver<TailRequest>,
) -> Flow {
    // Resolve the target to the file to watch. A directory resolves to its
    // newest session ONCE, here — the tailer never revisits that choice, so the
    // session it watches is the session the App asked for.
    let main_path = match resolve_live_target(target) {
        Some((_, main)) => main,
        None => {
            // Directory with no session file yet — poll until one shows up.
            match await_first_session(target, req_rx).await {
                Ok(main) => main,
                Err(flow) => return flow,
            }
        }
    };

    let session = LiveSession::new(main_path);
    let session_id = session.session_id();

    // No announce: the App decides which session is focused and already knows
    // its id before it asks for it. A reset here would be indistinguishable
    // from a monitored session's own truncation, which is exactly the ambiguity
    // that used to require a parallel set of Monitor* events.
    tail_loop(session, session_id, ui_tx, req_rx).await
}

/// The shared poll loop: every [`POLL_INTERVAL`] read appended bytes and emit a
/// [`UiEvent::Batch`], staying responsive to switch/exit requests. Both feeders
/// end here — live tailing after the target is resolved, replay after the bulk
/// hand-off — so EVERY session keeps tailing and can pick up new appends ("go
/// live"). The file is never re-targeted here; only a `Watch` request or a
/// truncation switches it.
pub(crate) async fn tail_loop(
    mut session: LiveSession,
    session_id: String,
    ui_tx: &mpsc::Sender<UiEvent>,
    req_rx: &mut mpsc::Receiver<TailRequest>,
) -> Flow {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    // Skip the immediate first tick so the loop blocks on the interval.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            req = req_rx.recv() => {
                match req {
                    Some(TailRequest::Watch(new_path)) => return Flow::Switch(new_path),
                    None => return Flow::Exit,
                }
            }
            _ = ticker.tick() => {
                if let Some(switch) = poll_live(&mut session, &session_id, ui_tx).await {
                    return Flow::Switch(switch);
                }
            }
        }
    }
}

/// One poll tick of live tailing. Returns `Some(path)` if a newer session was
/// discovered and the caller should switch to it.
async fn poll_live(
    session: &mut LiveSession,
    session_id: &str,
    ui_tx: &mpsc::Sender<UiEvent>,
) -> Option<PathBuf> {
    let mut updates: Vec<Update> = Vec::new();

    // --- main file ---
    match read_appended(&session.main_path, &mut session.main_state) {
        ReadResult::Reset => {
            let _ = ui_tx
                .send(UiEvent::SessionReset {
                    session_id: session_id.to_string(),
                })
                .await;
            // Truncation = re-attach: returning the SAME path makes run_live
            // rebuild the whole LiveSession (fresh offsets, seen_meta,
            // backfill gate). Patching individual fields in place would leave
            // the App's wiped model and the tailer's surviving state
            // inconsistent — subagents never re-emitted, and the re-read blips
            // liveness.
            return Some(session.main_path.clone());
        }
        ReadResult::Entries(entries) => {
            for entry in entries {
                updates.push(Update::Entry {
                    source: Source::Main,
                    entry,
                });
            }
        }
        ReadResult::NoChange | ReadResult::Missing => {}
    }

    // --- discover subagent/workflow files + metas (shared transcript layout) ---
    scan_files(session, &mut updates);

    // --- read each tracked non-main file ---
    if read_tracked(&mut session.tracked, &mut updates) {
        // A tracked file was truncated/rotated: its already-applied content is
        // baked into the App model, so re-reading it in place would duplicate
        // items and double-count tokens. Re-attach the whole session, same as
        // a main-file reset.
        let _ = ui_tx
            .send(UiEvent::SessionReset {
                session_id: session_id.to_string(),
            })
            .await;
        return Some(session.main_path.clone());
    }

    // --- emit batch + track idle stretch ---
    let had_activity = !updates.is_empty();
    if had_activity {
        let _ = ui_tx
            .send(UiEvent::Batch {
                session_id: session_id.to_string(),
                updates,
            })
            .await;
    }
    None
}

/// Read appended bytes from each tracked non-main file, pushing parsed entries
/// stamped with the file's stored [`Source`]. Returns `true` if any tracked
/// file was truncated/rotated — the caller must re-attach the whole session
/// (re-reading in place would re-emit already-applied content as duplicates).
fn read_tracked(
    tracked: &mut HashMap<PathBuf, (Source, TailState)>,
    updates: &mut Vec<Update>,
) -> bool {
    let mut reset = false;
    for (path, (source, state)) in tracked.iter_mut() {
        match read_appended(path, state) {
            ReadResult::Entries(entries) => {
                for entry in entries {
                    updates.push(Update::Entry {
                        source: source.clone(),
                        entry,
                    });
                }
            }
            ReadResult::Reset => reset = true,
            ReadResult::NoChange | ReadResult::Missing => {}
        }
    }
    reset
}

/// Discover all non-main session files via the shared transcript layout API
/// ([`transcript::scan_subagent_files`] etc.) and register them for tailing,
/// emitting each meta sidecar once. Direct and workflow subagents share one
/// path-keyed map; each workflow journal is tracked too.
fn scan_files(session: &mut LiveSession, updates: &mut Vec<Update>) {
    let subagents = session.subagents_dir.clone();
    // Direct subagents under `subagents/`.
    for f in transcript::scan_subagent_files(&subagents, None) {
        register_subagent(session, f, updates);
    }
    // Workflow journals + their subagents under `subagents/workflows/<wf>/`.
    for wf_id in transcript::scan_workflow_ids(&subagents) {
        let journal = transcript::workflow_journal(&subagents, &wf_id);
        if journal.is_file() {
            session.track(journal, Source::Journal(wf_id.clone()));
        }
        let wf_dir = transcript::workflow_dir(&subagents, &wf_id);
        for f in transcript::scan_subagent_files(&wf_dir, Some(&wf_id)) {
            register_subagent(session, f, updates);
        }
    }
}

/// Register one discovered subagent: emit its meta once (if present) and track
/// its transcript for tailing as a [`Source::Sub`].
fn register_subagent(
    session: &mut LiveSession,
    f: transcript::SubagentFile,
    updates: &mut Vec<Update>,
) {
    if f.meta.is_file() {
        emit_meta(
            &f.meta,
            f.agent_id.clone(),
            f.workflow,
            &mut session.seen_meta,
            updates,
        );
    }
    session.track(f.transcript, Source::Sub(f.agent_id));
}

/// Parse a `meta.json` sidecar and push a [`Update::SubagentMeta`] once.
fn emit_meta(
    path: &Path,
    agent_id: String,
    workflow: Option<String>,
    seen: &mut std::collections::HashSet<PathBuf>,
    updates: &mut Vec<Update>,
) {
    if seen.contains(path) {
        return;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let Ok(meta) = serde_json::from_slice::<SubagentMeta>(&bytes) else {
        // Do NOT mark as seen: a failed parse is usually a mid-write read
        // (meta.json caught between create and flush) — retry next tick.
        // Without the meta the agent would render parentless forever. The
        // retry costs one small read per tick in the rare permanently-corrupt
        // case.
        return;
    };
    seen.insert(path.to_path_buf());
    updates.push(Update::SubagentMeta {
        agent_id,
        workflow,
        meta,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tailer::UiEvent;

    #[test]
    fn emit_meta_retries_after_failed_parse() {
        use std::io::Write;

        let mut tmp = std::env::temp_dir();
        tmp.push(format!(
            "zoetrope_meta_retry_{}.meta.json",
            std::process::id()
        ));
        let mut seen = std::collections::HashSet::new();
        let mut updates = Vec::new();

        // Mid-write garbage: must NOT be marked seen (retry next tick).
        std::fs::File::create(&tmp)
            .unwrap()
            .write_all(b"{\"agentTy")
            .unwrap();
        emit_meta(&tmp, "a1".into(), None, &mut seen, &mut updates);
        assert!(updates.is_empty());
        assert!(!seen.contains(&tmp), "failed parse must be retried");

        // The completed write parses and emits.
        std::fs::File::create(&tmp)
            .unwrap()
            .write_all(br#"{"agentType":"guide"}"#)
            .unwrap();
        emit_meta(&tmp, "a1".into(), None, &mut seen, &mut updates);
        assert_eq!(updates.len(), 1);
        assert!(seen.contains(&tmp));
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn replay_seed_resumes_where_snapshot_stopped() {
        use std::io::Write;

        let mut dir = std::env::temp_dir();
        dir.push(format!("zoetrope_seed_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("44444444-4444-4444-4444-444444444444.jsonl");
        std::fs::write(
            &main,
            concat!(
                r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"one"}}"#, "\n",
                r#"{"type":"user","uuid":"u2","parentUuid":"u1","timestamp":"2026-06-05T10:01:00.000Z","message":{"role":"user","content":"two"}}"#, "\n",
            ),
        )
        .unwrap();

        let (items, _info, seed) = crate::tailer::replay::build_replay(&main);
        assert_eq!(items.len(), 2);

        // A line lands AFTER the bulk snapshot but BEFORE tailing starts — the
        // window the snapshot-seed resume must still emit, not silently drop.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&main)
            .unwrap()
            .write_all(
                concat!(
                    r#"{"type":"user","uuid":"u3","parentUuid":"u2","timestamp":"2026-06-05T10:02:00.000Z","message":{"role":"user","content":"three"}}"#, "\n",
                )
                .as_bytes(),
            )
            .unwrap();

        let mut session = LiveSession::new(main.clone());
        session.seed(seed);

        let (tx, mut rx) = mpsc::channel(32);
        poll_live(&mut session, "44444444", &tx).await;

        // Exactly the appended line is emitted — not zero (dropped), not
        // three (whole-file re-emission).
        let mut emitted = 0;
        while let Ok(ev) = rx.try_recv() {
            if let UiEvent::Batch { updates, .. } = ev {
                emitted += updates.len();
            }
        }
        assert_eq!(emitted, 1, "only the post-snapshot append is emitted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tracked_file_truncation_reattaches() {
        use std::io::Write;

        let mut dir = std::env::temp_dir();
        dir.push(format!("zoetrope_subtrunc_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let session_uuid = "55555555-5555-5555-5555-555555555555";
        let sub_dir = dir.join(session_uuid).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let main = dir.join(format!("{session_uuid}.jsonl"));
        std::fs::write(&main, concat!(
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"start"}}"#, "\n",
        )).unwrap();
        let sub = sub_dir.join("agent-bbbbbbbbbbbbbbbbb.jsonl");
        std::fs::write(&sub, concat!(
            r#"{"type":"user","uuid":"s1","parentUuid":null,"isSidechain":true,"agentId":"bbbbbbbbbbbbbbbbb","timestamp":"2026-06-05T10:01:00.000Z","message":{"role":"user","content":"task"}}"#, "\n",
            r#"{"type":"user","uuid":"s2","parentUuid":"s1","isSidechain":true,"agentId":"bbbbbbbbbbbbbbbbb","timestamp":"2026-06-05T10:02:00.000Z","message":{"role":"user","content":"more"}}"#, "\n",
        )).unwrap();

        let (tx, mut rx) = mpsc::channel(32);
        let mut session = LiveSession::new(main.clone());
        assert!(poll_live(&mut session, "55555555", &tx).await.is_none()); // backfill

        // Truncate the SUBAGENT file: already-applied content would be
        // re-emitted as duplicates if handled in place — must re-attach.
        std::fs::File::create(&sub)
            .unwrap()
            .write_all(b"{}\n")
            .unwrap();
        let switch = poll_live(&mut session, "55555555", &tx).await;
        assert_eq!(
            switch.as_deref(),
            Some(main.as_path()),
            "tracked-file truncation must re-attach the session"
        );
        let mut saw_reset = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(&ev, UiEvent::SessionReset { session_id } if session_id == "55555555") {
                saw_reset = true;
            }
        }
        assert!(saw_reset);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn truncation_triggers_full_reattach() {
        use std::io::Write;

        let mut dir = std::env::temp_dir();
        dir.push(format!("zoetrope_trunc_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("33333333-3333-3333-3333-333333333333.jsonl");
        std::fs::write(&main, b"{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,\"message\":{\"role\":\"user\",\"content\":\"abcdef\"}}\n").unwrap();

        let (tx, mut rx) = mpsc::channel(32);
        let mut session = LiveSession::new(main.clone());
        poll_live(&mut session, "33333333", &tx).await; // backfill

        // Truncate in place (shorter content) — must return the SAME path so
        // run_live rebuilds the whole LiveSession (fresh offsets/seen_meta).
        std::fs::File::create(&main)
            .unwrap()
            .write_all(b"{}\n")
            .unwrap();
        let switch = poll_live(&mut session, "33333333", &tx).await;
        assert_eq!(
            switch.as_deref(),
            Some(main.as_path()),
            "truncation must re-attach, not patch in place"
        );
        // And the UI got a SessionReset for the same session.
        let mut saw_reset = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(&ev, UiEvent::SessionReset { session_id } if session_id == "33333333") {
                saw_reset = true;
            }
        }
        assert!(saw_reset);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
