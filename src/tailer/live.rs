//! Live tailing.
//!
//! One poll loop ([`tail_loop`]) per session: each tick stat the root file and
//! every tracked file, read appended bytes, ask the session for files that
//! appeared since, and emit a [`UiEvent::Batch`]. Both feeders end here — live
//! after the announce, replay after the bulk hand-off — so every session keeps
//! tailing and can pick up new appends ("go live").
//!
//! Nothing here knows a format: a [`Session`] says which files there are and
//! how each is read, a [`Stream`] turns lines into statements.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;

use crate::fact::Statement;
use crate::provider::{
    OpenError, Provider, ReadMode, Scope, Session, SessionFile, Stream, Target, open, sweep,
};

use super::bytes::{ReadResult, TailState, read_appended};
use super::{Flow, TailRequest, UiEvent};

/// Poll interval for live tailing.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Run the newer-session auto-switch scan every N poll ticks (~2s at the
/// 200ms interval). The scan stats every session file in the project, which
/// scales with session HISTORY, not activity — unthrottled it would scan 5×/sec
/// on long-lived projects for an event that almost never happens.
const SWITCH_SCAN_EVERY: u32 = 10;

/// Auto-switch only after the current session has been silent this many poll
/// ticks (~30s at 200ms). Long enough that a normal mid-session lull (a slow
/// tool call, thinking) never trips it — so the switch fires only when the
/// session is genuinely done, never flapping between two live ones.
const SWITCH_IDLE_TICKS: u32 = 150;

/// How far back the auto-switch scan looks. A session newer than the one
/// being followed is by definition recent; bounding the scan keeps a
/// provider that classifies files by content from reading every file it
/// ever wrote, every two seconds.
const SWITCH_LOOKBACK: Duration = Duration::from_secs(24 * 60 * 60);

/// Tracks all files belonging to one live session.
pub(crate) struct LiveSession {
    /// The working directory whose newest session is being followed, if any:
    /// drives the newer-session auto-switch. `None` pins this session.
    pub(crate) follow: Option<PathBuf>,
    only: Option<Provider>,
    session: Session,
    /// Tail state for the root file, and the provider stream reading it.
    main_state: TailState,
    main_stream: Stream,
    /// Tail state per tracked non-root file, keyed by absolute path, with the
    /// stream reading it. One map (paths are unique) so no agent id can
    /// collide across a provider's directory trees.
    tracked: HashMap<PathBuf, (Stream, TailState)>,
    /// Whole-read sidecars already stated (by absolute path) — state once.
    seen_whole: HashSet<PathBuf>,
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
    /// The streams the bulk parse used, adopted alongside the offsets.
    seed_streams: HashMap<PathBuf, Stream>,
}

/// Per-file read positions and already-stated sidecars captured by the replay
/// bulk snapshot, used to seed a [`LiveSession`] via [`LiveSession::seed`].
#[derive(Debug, Default)]
pub(crate) struct SnapshotSeed {
    /// Bytes of each file consumed by the bulk parse (up to its last newline).
    pub(crate) offsets: HashMap<PathBuf, u64>,
    /// The stream that parsed each file, carrying whatever it learned from
    /// the lines before the offset. A Codex stream learns whose file it is
    /// from the first line; a fresh one resumed mid-file would state nothing.
    pub(crate) streams: HashMap<PathBuf, Stream>,
    /// Whole-read sidecars already stated in the bulk stream.
    pub(crate) seen_whole: HashSet<PathBuf>,
}

impl LiveSession {
    /// Tail an opened session. `follow` is the working directory to keep
    /// following for newer sessions; `None` pins this one.
    pub(crate) fn new(session: Session, follow: Option<PathBuf>, only: Option<Provider>) -> Self {
        let main_stream = session.provider.stream_for(&session.root);
        Self {
            follow,
            only,
            session,
            main_state: TailState::default(),
            main_stream,
            tracked: HashMap::new(),
            seen_whole: HashSet::new(),
            ticks: 0,
            idle_ticks: 0,
            seed_offsets: HashMap::new(),
            seed_streams: HashMap::new(),
        }
    }

    fn session_id(&self) -> String {
        self.session.id.clone()
    }

    fn root_path(&self) -> &Path {
        &self.session.root.path
    }

    /// Adopt a replay bulk snapshot's read positions: the root offset applies
    /// immediately, other offsets when each file is first registered, and
    /// bulk-stated sidecars are marked seen. Everything the snapshot did NOT
    /// consume — appends during the parse, files created since — is emitted by
    /// the subsequent tail polls.
    pub(crate) fn seed(&mut self, mut seed: SnapshotSeed) {
        if let Some(off) = seed.offsets.get(self.root_path()) {
            self.main_state.offset = *off;
        }
        if let Some(stream) = seed.streams.remove(self.root_path()) {
            self.main_stream = stream;
        }
        self.seed_offsets = seed.offsets;
        self.seed_streams = seed.streams;
        self.seen_whole = seed.seen_whole;
    }

    /// Register a tailed non-root file (idempotent), starting from its
    /// snapshot-seeded offset if one was captured.
    fn track(&mut self, file: &SessionFile) {
        if self.tracked.contains_key(&file.path) {
            return;
        }
        let mut state = TailState::default();
        if let Some(off) = self.seed_offsets.remove(&file.path) {
            state.offset = off;
        }
        let stream = self
            .seed_streams
            .remove(&file.path)
            .unwrap_or_else(|| self.session.provider.stream_for(file));
        self.tracked.insert(file.path.clone(), (stream, state));
    }

    /// The target that re-attaches this same session from scratch.
    fn reattach(&self) -> Target {
        Target::Path(self.root_path().to_path_buf())
    }
}

/// Block until a session exists for `cwd`, polling every [`POLL_INTERVAL`].
/// Stays responsive to requests; returns the session, or a [`Flow`] outcome
/// if a switch/exit request arrives first.
async fn await_first_session(
    cwd: &Path,
    only: Option<Provider>,
    req_rx: &mut mpsc::Receiver<TailRequest>,
) -> Result<Session, Flow> {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // A session that appears while we wait is recent by definition, so the
        // sweep behind `open` is bounded to the same lookback as the switch.
        let since = SystemTime::now()
            .checked_sub(SWITCH_LOOKBACK)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if let Some(root) = sweep(&Scope::project(cwd).since(since), only)
            .into_iter()
            .next()
            .map(|s| s.root)
            && let Ok(session) = open(&Target::Path(root.path), only)
        {
            return Ok(session);
        }
        tokio::select! {
            req = req_rx.recv() => match req {
                Some(TailRequest::Watch(t)) => return Err(Flow::from_watch(t)),
                None => return Err(Flow::Exit),
            },
            _ = ticker.tick() => {}
        }
    }
}

/// Live-tail loop for a single target until a switch or exit.
///
/// `follow` is the working directory being followed (the session is
/// re-discovered for newer ones); `None` pins the target: a named file or id
/// is what you asked for.
pub(crate) async fn run_live(
    target: &Target,
    follow: Option<PathBuf>,
    only: Option<Provider>,
    ui_tx: &mpsc::Sender<UiEvent>,
    req_rx: &mut mpsc::Receiver<TailRequest>,
) -> Flow {
    let session = match open(target, only) {
        Ok(s) => s,
        // A followed directory with no session yet: wait for the first one.
        Err(OpenError::NotFound(_)) if follow.is_some() => {
            match await_first_session(follow.as_deref().unwrap_or(Path::new(".")), only, req_rx)
                .await
            {
                Ok(s) => s,
                Err(flow) => return flow,
            }
        }
        Err(e) => {
            let _ = ui_tx.send(UiEvent::Error(e.to_string())).await;
            return match req_rx.recv().await {
                Some(TailRequest::Watch(t)) => Flow::from_watch(t),
                None => Flow::Exit,
            };
        }
    };

    let session = LiveSession::new(session, follow, only);
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
/// switch to a newer session fires only when `session.follow` is set (a
/// followed directory), not for a pinned file or id.
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
                    Some(TailRequest::Watch(t)) => return Flow::from_watch(t),
                    None => return Flow::Exit,
                }
            }
            _ = ticker.tick() => {
                if let Some(target) = poll_live(&mut session, &session_id, ui_tx).await {
                    return Flow::Switch { target, follow: session.follow.clone() };
                }
            }
        }
    }
}

/// One poll tick of live tailing. Returns `Some(target)` if the caller should
/// switch to it: a newer session was discovered, or this one must re-attach.
async fn poll_live(
    session: &mut LiveSession,
    session_id: &str,
    ui_tx: &mpsc::Sender<UiEvent>,
) -> Option<Target> {
    let mut statements: Vec<Statement> = Vec::new();

    // --- root file ---
    let root_path = session.root_path().to_path_buf();
    match read_appended(&root_path, &mut session.main_state) {
        ReadResult::Reset => {
            let _ = ui_tx
                .send(UiEvent::SessionReset {
                    session_id: session_id.to_string(),
                })
                .await;
            // Truncation = re-attach: returning the SAME session makes run_live
            // rebuild the whole LiveSession (fresh offsets, seen sidecars,
            // backfill gate). Patching individual fields in place would leave
            // the App's wiped model and the tailer's surviving state
            // inconsistent — agents never re-emitted, and the re-read blips
            // liveness.
            return Some(session.reattach());
        }
        ReadResult::Lines(lines) => {
            statements.extend(lines.iter().filter_map(|l| session.main_stream.push(l)));
        }
        ReadResult::NoChange | ReadResult::Missing => {}
    }

    // --- the session's other files: known ones and any that appeared ---
    scan_files(session, &mut statements);

    // --- read each tracked non-root file ---
    if read_tracked(&mut session.tracked, &mut statements) {
        // A tracked file was truncated/rotated: its already-applied content is
        // baked into the App model, so re-reading it in place would duplicate
        // items and double-count tokens. Re-attach the whole session, same as
        // a root-file reset.
        let _ = ui_tx
            .send(UiEvent::SessionReset {
                session_id: session_id.to_string(),
            })
            .await;
        return Some(session.reattach());
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
    // sessions written concurrently in one project leapfrog each other's
    // mtime and the watcher flaps between them every scan. An idle current
    // session + a newer one = the user moved on, so follow.
    session.ticks = session.ticks.wrapping_add(1);
    if session.ticks.is_multiple_of(SWITCH_SCAN_EVERY)
        && session.idle_ticks >= SWITCH_IDLE_TICKS
        && let Some(cwd) = &session.follow
    {
        let since = SystemTime::now()
            .checked_sub(SWITCH_LOOKBACK)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let candidates = sweep(&Scope::project(cwd).since(since), session.only);
        if let Some(newer) = newer_session(&session.session, candidates) {
            // Stamp the reset with the NEW session id so the UI adopts the id
            // the post-switch tailer will stamp its batches with — stamping
            // the OLD id here would make the UI drop every event of the new
            // session.
            let _ = ui_tx
                .send(UiEvent::SessionReset {
                    session_id: newer.id,
                })
                .await;
            return Some(Target::Path(newer.root.path));
        }
    }

    None
}

/// The newest of `candidates` if it is a different session than `current`.
/// Candidates come newest first from [`sweep`].
fn newer_session(current: &Session, candidates: Vec<Session>) -> Option<Session> {
    candidates
        .into_iter()
        .next()
        .filter(|s| s.root.path != current.root.path)
}

/// Read appended bytes from each tracked non-root file through its stream.
/// Returns `true` if any tracked file was truncated/rotated — the caller must
/// re-attach the whole session (re-reading in place would re-emit
/// already-applied content as duplicates).
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

/// Register every file the session has, including any that appeared since
/// the last tick: tailed files are tracked (idempotent), whole-read sidecars
/// are stated once they parse.
fn scan_files(session: &mut LiveSession, statements: &mut Vec<Statement>) {
    session.session.rescan();
    let files = session.session.files.clone();
    let provider = session.session.provider;
    for file in &files {
        match file.read {
            ReadMode::Tail => session.track(file),
            ReadMode::Whole => {
                emit_whole(provider, file, &mut session.seen_whole, statements);
            }
        }
    }
}

/// State a whole-read sidecar's facts once its text parses.
fn emit_whole(
    provider: Provider,
    file: &SessionFile,
    seen: &mut HashSet<PathBuf>,
    statements: &mut Vec<Statement>,
) {
    if seen.contains(&file.path) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(&file.path) else {
        return;
    };
    let Some(statement) = provider.sidecar(file, &text) else {
        // Do NOT mark as seen: a failed parse is usually a mid-write read
        // (the sidecar caught between create and flush) — retry next tick.
        // Without it the agent would render parentless forever. The retry
        // costs one small read per tick in the rare permanently-corrupt case.
        return;
    };
    seen.insert(file.path.clone());
    statements.push(statement);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tailer::UiEvent;

    /// A temp Claude-shaped session: `<dir>/<uuid>.jsonl` with `main_lines`.
    fn claude_session(tag: &str, uuid: &str, main_lines: &str) -> (PathBuf, PathBuf) {
        let mut dir = std::env::temp_dir();
        dir.push(format!("zoetrope_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join(format!("{uuid}.jsonl"));
        std::fs::write(&main, main_lines).unwrap();
        (dir, main)
    }

    #[test]
    fn emit_whole_retries_after_failed_parse() {
        use std::io::Write;

        let mut dir = std::env::temp_dir();
        dir.push(format!("zoetrope_meta_retry_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub_dir = dir
            .join("66666666-6666-6666-6666-666666666666")
            .join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let path = sub_dir.join("agent-a1.meta.json");
        let file = SessionFile {
            provider: Provider::Claude,
            path: path.clone(),
            session: "66666666-6666-6666-6666-666666666666".into(),
            role: crate::provider::FileRole::Sidecar,
            read: ReadMode::Whole,
            project_key: String::new(),
            modified: SystemTime::UNIX_EPOCH,
        };
        let mut seen = HashSet::new();
        let mut statements = Vec::new();

        // Mid-write garbage: must NOT be marked seen (retry next tick).
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"{\"agentTy")
            .unwrap();
        emit_whole(Provider::Claude, &file, &mut seen, &mut statements);
        assert!(statements.is_empty());
        assert!(!seen.contains(&path), "failed parse must be retried");

        // The completed write parses and emits.
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"agentType":"guide"}"#)
            .unwrap();
        emit_whole(Provider::Claude, &file, &mut seen, &mut statements);
        assert_eq!(statements.len(), 1);
        assert!(seen.contains(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_session_is_a_switch_and_the_same_one_is_not() {
        let (dir, a) = claude_session("switch", "11111111-1111-1111-1111-111111111111", "");
        let b = dir.join("99999999-9999-9999-9999-999999999999.jsonl");
        std::fs::write(&b, "").unwrap();
        let current = open(&Target::Path(a.clone()), None).unwrap();
        let newer = open(&Target::Path(b.clone()), None).unwrap();
        // Newest first, as sweep hands them over.
        assert_eq!(
            newer_session(&current, vec![newer.clone(), current.clone()]).map(|s| s.root.path),
            Some(b.clone()),
            "an idle session follows the newer file"
        );
        assert!(
            newer_session(&current, vec![current.clone()]).is_none(),
            "the same session is never a switch"
        );
        assert!(newer_session(&current, vec![]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn replay_seed_resumes_where_snapshot_stopped() {
        use std::io::Write;

        let (dir, main) = claude_session(
            "seed",
            "44444444-4444-4444-4444-444444444444",
            concat!(
                r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"one"}}"#,
                "\n",
                r#"{"type":"user","uuid":"u2","parentUuid":"u1","timestamp":"2026-06-05T10:01:00.000Z","message":{"role":"user","content":"two"}}"#,
                "\n",
            ),
        );

        let session = open(&Target::Path(main.clone()), None).unwrap();
        let (items, _info, seed) = crate::tailer::replay::build_replay(&session);
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

        let mut live = LiveSession::new(session, None, None);
        live.seed(seed);

        let (tx, mut rx) = mpsc::channel(32);
        poll_live(&mut live, "44444444", &tx).await;

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

    /// The stream that parsed the bulk carries into the tail. For Codex that
    /// is the difference between appends being attributed and being dropped:
    /// a fresh stream would never see the first line that says whose file it is.
    #[tokio::test]
    async fn replay_hands_its_streams_to_the_tail() {
        use std::io::Write;

        let Some(fixtures) = crate::provider::harness::fixture_dir("codex") else {
            return;
        };
        let src = fixtures.join(
            "cli-0.153.4/2026/09/07/rollout-2026-09-07T16-25-14-01a07c0b-5e16-70f2-ad21-7c2a3a228b22.jsonl",
        );
        let mut dir = std::env::temp_dir();
        dir.push(format!("zoetrope_codex_seed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let day = dir.join("sessions/2026/09/07");
        std::fs::create_dir_all(&day).unwrap();
        let root = day.join(src.file_name().unwrap());
        std::fs::copy(&src, &root).unwrap();

        let session = open(&Target::Path(root.clone()), None).unwrap();
        assert_eq!(session.provider, Provider::Codex);
        let (items, _info, seed) = crate::tailer::replay::build_replay(&session);
        assert!(!items.is_empty());

        // A line lands after the bulk parse: a message by the root thread.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&root)
            .unwrap()
            .write_all(
                concat!(
                    r#"{"timestamp":"2026-09-07T13:29:00.000Z","ordinal":999,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"after the replay"}]}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .unwrap();

        let mut live = LiveSession::new(session, None, None);
        live.seed(seed);
        let (tx, mut rx) = mpsc::channel(32);
        poll_live(&mut live, "x", &tx).await;

        let mut said = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let UiEvent::Batch { statements, .. } = ev {
                said.extend(statements);
            }
        }
        assert_eq!(said.len(), 1, "exactly the appended line is stated");
        assert_eq!(said[0].facts[0].agent.as_deref(), Some("main"));
        assert!(
            matches!(&said[0].facts[0].kind, crate::fact::FactKind::Reasoning(t) if t == "after the replay")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tracked_file_truncation_reattaches() {
        use std::io::Write;

        let session_uuid = "55555555-5555-5555-5555-555555555555";
        let (dir, main) = claude_session(
            "subtrunc",
            session_uuid,
            concat!(
                r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"start"}}"#,
                "\n",
            ),
        );
        let sub_dir = dir.join(session_uuid).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let sub = sub_dir.join("agent-bbbbbbbbbbbbbbbbb.jsonl");
        std::fs::write(&sub, concat!(
            r#"{"type":"user","uuid":"s1","parentUuid":null,"isSidechain":true,"agentId":"bbbbbbbbbbbbbbbbb","timestamp":"2026-06-05T10:01:00.000Z","message":{"role":"user","content":"task"}}"#, "\n",
            r#"{"type":"user","uuid":"s2","parentUuid":"s1","isSidechain":true,"agentId":"bbbbbbbbbbbbbbbbb","timestamp":"2026-06-05T10:02:00.000Z","message":{"role":"user","content":"more"}}"#, "\n",
        )).unwrap();

        let (tx, mut rx) = mpsc::channel(32);
        let session = open(&Target::Path(main.clone()), None).unwrap();
        let mut live = LiveSession::new(session, None, None);
        assert!(poll_live(&mut live, "55555555", &tx).await.is_none()); // backfill

        // Truncate the SUBAGENT file: already-applied content would be
        // re-emitted as duplicates if handled in place — must re-attach.
        std::fs::File::create(&sub)
            .unwrap()
            .write_all(b"{}\n")
            .unwrap();
        let switch = poll_live(&mut live, "55555555", &tx).await;
        assert_eq!(
            switch,
            Some(Target::Path(main.clone())),
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

        let (dir, main) = claude_session(
            "trunc",
            "33333333-3333-3333-3333-333333333333",
            "{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,\"message\":{\"role\":\"user\",\"content\":\"abcdef\"}}\n",
        );

        let (tx, mut rx) = mpsc::channel(32);
        let session = open(&Target::Path(main.clone()), None).unwrap();
        let mut live = LiveSession::new(session, None, None);
        poll_live(&mut live, "33333333", &tx).await; // backfill

        // Truncate in place (shorter content) — must return the SAME session so
        // run_live rebuilds the whole LiveSession (fresh offsets/seen sidecars).
        std::fs::File::create(&main)
            .unwrap()
            .write_all(b"{}\n")
            .unwrap();
        let switch = poll_live(&mut live, "33333333", &tx).await;
        assert_eq!(
            switch,
            Some(Target::Path(main.clone())),
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
