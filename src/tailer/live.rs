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

use crate::fact::Statement;
use crate::provider::claude::{Source, Stream, discovery, wire::SubagentMeta};

use super::bytes::{ReadResult, TailState, read_appended};
use super::{Flow, TailRequest, UiEvent};

/// Poll interval for live tailing.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Run the newer-session auto-switch scan every N poll ticks (~2s at the
/// 200ms interval). The scan stats every `*.jsonl` in the project dir, which
/// scales with session HISTORY, not activity — unthrottled it would scan 5×/sec
/// on long-lived projects for an event that almost never happens.
const SWITCH_SCAN_EVERY: u32 = 10;

/// Auto-switch only after the current session has been silent this many poll
/// ticks (~30s at 200ms). Long enough that a normal mid-session lull (a slow
/// tool call, thinking) never trips it — so the switch fires only when the
/// session is genuinely done, never flapping between two live ones.
const SWITCH_IDLE_TICKS: u32 = 150;

/// Tracks all files belonging to one live session.
pub(crate) struct LiveSession {
    /// Project directory containing the main file (for newer-session scans).
    pub(crate) project_dir: Option<PathBuf>,
    /// The main `<uuid>.jsonl` path.
    main_path: PathBuf,
    /// The `<uuid>/subagents` directory (may not exist yet).
    subagents_dir: PathBuf,
    /// Tail state for the main file, and the provider stream reading it.
    main_state: TailState,
    main_stream: Stream,
    /// Tail state per discovered non-main file, keyed by absolute path, carrying
    /// the [`Source`] to stamp its entries with. One map (paths are unique) so no
    /// agent id can collide across the direct-subagent / workflow / journal trees.
    tracked: HashMap<PathBuf, (Stream, TailState)>,
    /// meta.json files already emitted (by absolute path) — emit once.
    seen_meta: std::collections::HashSet<PathBuf>,
    /// Poll-tick counter, used to throttle the newer-session scan.
    ticks: u32,
    /// Consecutive poll ticks with NO activity (no appended bytes anywhere).
    /// Gates auto-switch so two concurrently-written sessions can't leapfrog.
    idle_ticks: u32,
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
    /// `project_dir`. `project_dir` drives newer-session auto-switch scans and is
    /// the watched directory (NOT `main_path.parent()`, which for a workflow or
    /// nested layout would be wrong — they are the same here, but threading the
    /// project dir explicitly keeps auto-switch correct).
    pub(crate) fn new(project_dir: PathBuf, main_path: PathBuf) -> Self {
        let subagents_dir = discovery::subagents_dir(&main_path).unwrap_or_default();
        Self {
            project_dir: Some(project_dir),
            main_path,
            subagents_dir,
            main_state: TailState::default(),
            main_stream: Stream::new(Source::Main),
            tracked: HashMap::new(),
            seen_meta: std::collections::HashSet::new(),
            ticks: 0,
            idle_ticks: 0,
            seed_offsets: HashMap::new(),
        }
    }

    fn session_id(&self) -> String {
        discovery::session_id_from_path(&self.main_path)
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
        self.tracked.insert(path, (Stream::new(source), state));
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
        let main = discovery::latest_session_file(target)?;
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
        if let Some(main) = discovery::latest_session_file(project_dir) {
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
    // Resolve the target to a (project_dir, main_file). If a project dir has no
    // session yet, wait for the first one to appear. A concrete FILE target
    // PINS to that session (no auto-switch — you named the file you want); only
    // a directory target follows the newest session in it.
    let pin = !target.is_dir();
    let (project_dir, main_path) = match resolve_live_target(target) {
        Some(pair) => pair,
        None => {
            // Directory with no session file yet — poll until one shows up.
            match await_first_session(target, req_rx).await {
                Ok(main) => (target.to_path_buf(), main),
                Err(flow) => return flow,
            }
        }
    };

    let mut session = LiveSession::new(project_dir, main_path);
    if pin {
        session.project_dir = None;
    }
    let session_id = session.session_id();

    // Announce the resolved session id so the UI adopts it before any batch
    // arrives. The UI seeds `current_session_id` from a best-effort up-front
    // discovery, which can be empty (no session yet) or stale (a newer file
    // appeared between startup and now); without this announcement those batches
    // would be dropped by the is_current gate. Idempotent when the id already
    // matches (reset of an empty model is a no-op). Live tailing backfills the
    // whole existing file on the first poll (arrival order).
    let _ = ui_tx
        .send(UiEvent::SessionReset {
            session_id: session_id.clone(),
        })
        .await;

    tail_loop(session, session_id, ui_tx, req_rx).await
}

/// The shared poll loop: every [`POLL_INTERVAL`] read appended bytes and emit a
/// [`UiEvent::Batch`], staying responsive to switch/exit requests. Both feeders
/// end here — live tailing after the announce, replay after the bulk hand-off —
/// so EVERY session keeps tailing and can pick up new appends ("go live"). Auto-
/// switch to a newer session fires only when `session.project_dir` is set (live
/// directory targets), not for a pinned replay file.
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
    let mut statements: Vec<Statement> = Vec::new();

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
        ReadResult::Lines(lines) => {
            statements.extend(lines.iter().filter_map(|l| session.main_stream.push(l)));
        }
        ReadResult::NoChange | ReadResult::Missing => {}
    }

    // --- discover subagent/workflow files + metas (shared transcript layout) ---
    scan_files(session, &mut statements);

    // --- read each tracked non-main file ---
    if read_tracked(&mut session.tracked, &mut statements) {
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
    let had_activity = !statements.is_empty();
    if had_activity {
        let _ = ui_tx
            .send(UiEvent::Batch {
                session_id: session_id.to_string(),
                statements,
            })
            .await;
    }
    session.idle_ticks = if had_activity {
        0
    } else {
        session.idle_ticks.saturating_add(1)
    };

    // --- newer-session auto-switch (throttled — see SWITCH_SCAN_EVERY) ---
    // Only switch once THIS session has been quiet for a while: otherwise two
    // sessions written concurrently in one project dir leapfrog each other's
    // mtime and the watcher flaps between them every scan. An idle current
    // session + a newer file = the user moved on, so follow.
    session.ticks = session.ticks.wrapping_add(1);
    if session.ticks.is_multiple_of(SWITCH_SCAN_EVERY)
        && session.idle_ticks >= SWITCH_IDLE_TICKS
        && let Some(dir) = &session.project_dir
        && let Some(latest) = discovery::latest_session_file(dir)
        && latest != session.main_path
    {
        // Stamp the reset with the NEW session id so the UI adopts the id the
        // post-switch tailer will stamp its batches with — stamping the OLD id
        // here would make the UI drop every event of the new session.
        let new_id = discovery::session_id_from_path(&latest);
        let _ = ui_tx
            .send(UiEvent::SessionReset { session_id: new_id })
            .await;
        return Some(latest);
    }

    None
}

/// Read appended bytes from each tracked non-main file, pushing parsed entries
/// stamped with the file's stored [`Source`]. Returns `true` if any tracked
/// file was truncated/rotated — the caller must re-attach the whole session
/// (re-reading in place would re-emit already-applied content as duplicates).
fn read_tracked(
    tracked: &mut HashMap<PathBuf, (Stream, TailState)>,
    statements: &mut Vec<Statement>,
) -> bool {
    let mut reset = false;
    for (path, (stream, state)) in tracked.iter_mut() {
        match read_appended(path, state) {
            ReadResult::Lines(lines) => {
                statements.extend(lines.iter().filter_map(|l| stream.push(l)));
            }
            ReadResult::Reset => reset = true,
            ReadResult::NoChange | ReadResult::Missing => {}
        }
    }
    reset
}

/// Discover all non-main session files via the shared transcript layout API
/// ([`discovery::scan_subagent_files`] etc.) and register them for tailing,
/// emitting each meta sidecar once. Direct and workflow subagents share one
/// path-keyed map; each workflow journal is tracked too.
fn scan_files(session: &mut LiveSession, statements: &mut Vec<Statement>) {
    let subagents = session.subagents_dir.clone();
    // Direct subagents under `subagents/`.
    for f in discovery::scan_subagent_files(&subagents, None) {
        register_subagent(session, f, statements);
    }
    // Workflow journals + their subagents under `subagents/workflows/<wf>/`.
    for wf_id in discovery::scan_workflow_ids(&subagents) {
        let journal = discovery::workflow_journal(&subagents, &wf_id);
        if journal.is_file() {
            session.track(journal, Source::Ledger(wf_id.clone()));
        }
        let wf_dir = discovery::workflow_dir(&subagents, &wf_id);
        for f in discovery::scan_subagent_files(&wf_dir, Some(&wf_id)) {
            register_subagent(session, f, statements);
        }
    }
}

/// Register one discovered subagent: emit its meta once (if present) and track
/// its transcript for tailing as a [`Source::Sub`].
fn register_subagent(
    session: &mut LiveSession,
    f: discovery::SubagentFile,
    statements: &mut Vec<Statement>,
) {
    if f.meta.is_file() {
        emit_meta(
            &f.meta,
            f.agent_id.clone(),
            f.workflow,
            &mut session.seen_meta,
            statements,
        );
    }
    session.track(f.transcript, Source::Sub(f.agent_id));
}

/// Parse a `meta.json` sidecar and state its facts once.
fn emit_meta(
    path: &Path,
    agent_id: String,
    workflow: Option<String>,
    seen: &mut std::collections::HashSet<PathBuf>,
    statements: &mut Vec<Statement>,
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
    statements.push(Stream::meta(&agent_id, workflow.as_deref(), &meta));
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
        let mut statements = Vec::new();

        // Mid-write garbage: must NOT be marked seen (retry next tick).
        std::fs::File::create(&tmp)
            .unwrap()
            .write_all(b"{\"agentTy")
            .unwrap();
        emit_meta(&tmp, "a1".into(), None, &mut seen, &mut statements);
        assert!(statements.is_empty());
        assert!(!seen.contains(&tmp), "failed parse must be retried");

        // The completed write parses and emits.
        std::fs::File::create(&tmp)
            .unwrap()
            .write_all(br#"{"agentType":"guide"}"#)
            .unwrap();
        emit_meta(&tmp, "a1".into(), None, &mut seen, &mut statements);
        assert_eq!(statements.len(), 1);
        assert!(seen.contains(&tmp));
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn auto_switch_follows_a_newer_session_when_idle() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("zoetrope_switch_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Current session A (empty → no new activity this poll) and a newer
        // session B. B's id sorts lexicographically greater, so it wins
        // `latest_session_file`'s deterministic tie-break regardless of mtime.
        let a = dir.join("11111111-1111-1111-1111-111111111111.jsonl");
        let b = dir.join("99999999-9999-9999-9999-999999999999.jsonl");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();

        let mut session = LiveSession::new(dir.clone(), a.clone());
        // Quiet long enough to switch, and aligned to the throttled scan tick.
        session.idle_ticks = SWITCH_IDLE_TICKS;
        session.ticks = SWITCH_SCAN_EVERY - 1;

        let (tx, mut rx) = mpsc::channel(32);
        let switched = poll_live(&mut session, "11111111", &tx).await;

        assert_eq!(
            switched,
            Some(b.clone()),
            "an idle session follows the newer file"
        );
        // The reset must carry the NEW session id, so the UI adopts the batches
        // the post-switch tailer will stamp (the OLD id would drop them all).
        match rx.try_recv() {
            Ok(UiEvent::SessionReset { session_id }) => assert_ne!(
                session_id, "11111111-1111-1111-1111-111111111111",
                "reset carries the new session id, not the old"
            ),
            other => panic!("expected a SessionReset for the new session, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
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

        let mut session = LiveSession::new(dir.clone(), main.clone());
        session.project_dir = None;
        session.seed(seed);

        let (tx, mut rx) = mpsc::channel(32);
        poll_live(&mut session, "44444444", &tx).await;

        // Exactly the appended line is emitted — not zero (dropped), not
        // three (whole-file re-emission).
        let mut emitted = 0;
        while let Ok(ev) = rx.try_recv() {
            if let UiEvent::Batch { statements, .. } = ev {
                emitted += statements.len();
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
        let mut session = LiveSession::new(dir.clone(), main.clone());
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
        let mut session = LiveSession::new(dir.clone(), main.clone());
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
