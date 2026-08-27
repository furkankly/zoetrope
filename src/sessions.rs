//! Multi-session discovery: find every session worth watching, accurately.
//!
//! The single-session paths ask a project directory for its newest transcript
//! ([`crate::transcript::latest_session_file`]). Watching *all* live sessions
//! is a different question — every project directory, every transcript in it,
//! and the sidecars underneath — where being wrong is worse than being slow:
//! a missed session is a session the user believes they are watching.
//!
//! # Why activity is not the main file's mtime
//!
//! A session's recent work often lands entirely in a subagent sidecar while the
//! main transcript sits untouched — the main agent is blocked on the subagent,
//! so it writes nothing. Filtering on the main file's mtime alone drops exactly
//! the sessions a user most wants to see: the ones with agents running right
//! now. [`SessionRef::last_touched`] is therefore the newest mtime across the
//! main transcript *and* every sidecar under it.
//!
//! # Two phases, and why the first one is sound
//!
//! 1. [`discover`] — a stat-only sweep. Cheap enough to run over every project
//!    on every scan tick.
//! 2. [`Summary::of`] — the [`crate::index`] skeleton scan over the candidates,
//!    giving true first/last activity and activity counts with no body parsing.
//!
//! Phase 1 filters on mtime, which can only ever *over*-include: appending to a
//! file always advances its mtime, so no active session can be filtered out.
//! Phase 2 then reports ground truth from the bytes. The one case mtime gets
//! wrong is a transcript restored from a backup with its old mtime preserved —
//! it looks older than its contents. That is a restore, not a live session, so
//! the window filter excluding it is correct behaviour rather than a miss.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::index::{self, Index, flags};
use crate::state::rail::{RailRow, project_label};
use crate::transcript::{self, Entry};

/// One discovered session and everything the stat sweep learned about it.
#[derive(Debug, Clone)]
pub struct SessionRef {
    /// The session uuid (the main transcript's file stem).
    pub session_id: String,
    /// `~/.claude/projects/<sanitized-cwd>` — the session's project directory.
    pub project_dir: PathBuf,
    /// The main `<uuid>.jsonl`.
    pub main_path: PathBuf,
    /// Newest mtime across the main transcript and every sidecar. See the
    /// module docs for why the main file alone is not enough.
    pub last_touched: SystemTime,
    /// Whether [`last_touched`](Self::last_touched) came from a sidecar rather
    /// than the main transcript — i.e. the visible work is a subagent's.
    pub touched_by_sidecar: bool,
    /// Total bytes across the main transcript and every sidecar.
    pub bytes: u64,
    /// Sidecar transcripts found (direct subagents, workflow subagents,
    /// journals), captured by the same walk that produced the stats above.
    pub sidecars: Vec<PathBuf>,
}

impl SessionRef {
    /// Every transcript file belonging to this session, main first.
    ///
    /// The sidecars come from the sweep that discovered the session rather than
    /// a fresh directory walk — indexing them would otherwise re-enumerate the
    /// very directories discovery just read, twice per session per sweep. A
    /// sidecar created since then is picked up by the next sweep.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut out = Vec::with_capacity(self.sidecars.len() + 1);
        out.push(self.main_path.clone());
        out.extend(self.sidecars.iter().cloned());
        out
    }

    /// Sidecar transcripts found for this session.
    pub fn sidecar_count(&self) -> usize {
        self.sidecars.len()
    }
}

/// Everything one walk of a session's sidecar tree yields.
struct Sidecars {
    paths: Vec<PathBuf>,
    newest: SystemTime,
    bytes: u64,
}

/// Walk a session's sidecar tree once, collecting the transcripts AND their
/// stats together.
///
/// One walk rather than two: discovery needs the newest mtime and total size,
/// summarizing needs the file list, and re-deriving one from a second
/// `read_dir` of the same directories is the sweep's largest avoidable cost.
///
/// Which files count is [`transcript::scan_subagent_files`]'s rule — the
/// `agent-<id>.jsonl` pairs plus each workflow's journal — not "every `.jsonl`
/// here", so the stats describe exactly the files that get indexed.
fn sidecars(main_path: &Path) -> Option<Sidecars> {
    let subs = transcript::subagents_dir(main_path)?;
    if !subs.is_dir() {
        return None;
    }

    let mut paths: Vec<PathBuf> = transcript::scan_subagent_files(&subs, None)
        .into_iter()
        .map(|f| f.transcript)
        .collect();
    for wf_id in transcript::scan_workflow_ids(&subs) {
        let dir = transcript::workflow_dir(&subs, &wf_id);
        paths.extend(
            transcript::scan_subagent_files(&dir, Some(&wf_id))
                .into_iter()
                .map(|f| f.transcript),
        );
        let journal = transcript::workflow_journal(&subs, &wf_id);
        if journal.is_file() {
            paths.push(journal);
        }
    }
    if paths.is_empty() {
        return None;
    }

    let mut newest = SystemTime::UNIX_EPOCH;
    let mut bytes = 0u64;
    for path in &paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        bytes += meta.len();
        if let Ok(m) = meta.modified() {
            newest = newest.max(m);
        }
    }

    Some(Sidecars {
        paths,
        newest,
        bytes,
    })
}

/// Stat one main transcript into a [`SessionRef`].
fn session_ref(project_dir: &Path, main_path: PathBuf) -> Option<SessionRef> {
    let meta = std::fs::metadata(&main_path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let main_mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let mut last_touched = main_mtime;
    let mut bytes = meta.len();
    let mut sidecar_paths = Vec::new();
    let mut touched_by_sidecar = false;

    if let Some(found) = sidecars(&main_path) {
        bytes += found.bytes;
        sidecar_paths = found.paths;
        if found.newest > last_touched {
            last_touched = found.newest;
            touched_by_sidecar = true;
        }
    }

    Some(SessionRef {
        session_id: transcript::session_id_from_path(&main_path),
        project_dir: project_dir.to_path_buf(),
        main_path,
        last_touched,
        touched_by_sidecar,
        bytes,
        sidecars: sidecar_paths,
    })
}

/// Every session under `root` touched at or after `since`, newest first.
///
/// `root` is normally [`transcript::claude_projects_root`]. Unreadable project
/// directories are skipped rather than failing the sweep — a permissions
/// problem in one project must not blind the user to every other.
pub fn discover(root: &Path, since: SystemTime) -> Vec<SessionRef> {
    let Ok(projects) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for project in projects.flatten() {
        let dir = project.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !transcript::is_session_file(&path) {
                continue;
            }
            // Stat the main file first and skip the sidecar sweep for anything
            // far outside the window — but only when the main file itself is
            // old AND has no sidecar directory to check, which `session_ref`
            // resolves. The cheap rejection is the extension/uuid filter above.
            if let Some(s) = session_ref(&dir, path)
                && s.last_touched >= since
            {
                out.push(s);
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.last_touched));
    out
}

/// Every session in the default projects root touched within `window`.
pub fn discover_recent(window: std::time::Duration) -> Vec<SessionRef> {
    let Some(root) = transcript::claude_projects_root() else {
        return Vec::new();
    };
    let since = SystemTime::now()
        .checked_sub(window)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    discover(&root, since)
}

/// What a session looks like from its skeleton index alone — enough to render a
/// session list row, with no transcript body parsed.
///
/// Every field is folded from [`index::Rec`]s, so building one costs a stat and
/// a header read on a warm cache.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub session_id: String,
    /// Earliest and latest timestamp across every file. Ground truth from the
    /// bytes, unlike [`SessionRef::last_touched`], which is the mtime proxy.
    pub first_activity: Option<chrono::DateTime<chrono::Utc>>,
    pub last_activity: Option<chrono::DateTime<chrono::Utc>>,
    /// Indexed lines across every file.
    pub lines: usize,
    /// Lines the model would fold onto the timeline (excludes flat metadata).
    pub timeline_items: usize,
    /// Human prompts the index can vouch for (`origin.kind == "human"`).
    pub prompts: usize,
    /// Legacy `user` lines with no `origin` field, where prompt-ness needs the
    /// body. Non-zero means [`prompts`](Self::prompts) is a lower bound — the
    /// index reports the gap rather than guessing across it.
    pub ambiguous_prompts: usize,
    pub tool_calls: usize,
    pub spawns: usize,
    pub failures: usize,
    /// Distinct `agentId`s seen across the sidecars — the session's agent count
    /// without folding a model.
    pub agents: usize,
    /// Bytes indexed across every file.
    pub bytes: u64,
    /// Files indexed (main + sidecars).
    pub files: usize,
    /// The working directory the session ran in, read from the first
    /// envelope-bearing line of the main transcript.
    ///
    /// The one field here that costs a parsed body — exactly one line per
    /// session. Worth it: the alternative is guessing the project name from the
    /// `~/.claude/projects` directory name, which is a sanitized cwd and not
    /// invertible (see [`project_label`]). A session's cwd never changes, so
    /// the first line that has one is as good as any.
    pub cwd: Option<String>,
}

impl Summary {
    /// Fold every record of every file belonging to `session`.
    ///
    /// Files that cannot be opened are skipped: a sidecar deleted between
    /// discovery and indexing is normal (a workflow cleaning up), not an error.
    pub fn of(session: &SessionRef) -> Summary {
        let mut s = Summary {
            session_id: session.session_id.clone(),
            ..Default::default()
        };
        let mut agent_keys = std::collections::HashSet::new();

        for (n, path) in session.files().iter().enumerate() {
            let Ok((idx, map)) = index::open(path) else {
                continue;
            };
            // `files()` yields the main transcript first.
            if n == 0 {
                s.cwd = first_cwd(&idx, &map);
            }
            s.files += 1;
            s.bytes += map.len() as u64;
            s.fold(&idx, &mut agent_keys);
        }
        s.agents = agent_keys.len();
        s
    }

    /// Fold one file's records in.
    fn fold(&mut self, idx: &Index, agent_keys: &mut std::collections::HashSet<u32>) {
        for rec in &idx.recs {
            self.lines += 1;
            if rec.kind.is_timeline_item() {
                self.timeline_items += 1;
            }
            if let Some(ts) = rec.ts() {
                self.first_activity = Some(self.first_activity.map_or(ts, |f| f.min(ts)));
                self.last_activity = Some(self.last_activity.map_or(ts, |l| l.max(ts)));
            }
            if rec.flags & flags::HUMAN_ORIGIN != 0 {
                self.prompts += 1;
            } else if rec.flags & flags::AMBIGUOUS_PROMPT != 0 {
                self.ambiguous_prompts += 1;
            }
            if rec.agent_key != 0 {
                agent_keys.insert(rec.agent_key);
            }
            self.tool_calls += rec.tool_uses as usize;
            self.spawns += rec.spawns as usize;
            self.failures += rec.failures as usize;
        }
    }
}

/// The `cwd` of the first envelope-bearing line, parsed properly.
///
/// Scans records (free) for the first line that can carry an envelope and
/// parses just that one. Stops at the first line that yields a cwd; a
/// transcript whose early lines predate the field falls through to `None`
/// rather than scanning the whole file, since the field's absence is a
/// property of the session's Claude Code version, not of the line.
fn first_cwd(idx: &Index, bytes: &[u8]) -> Option<String> {
    let rec = idx.recs.iter().find(|r| r.kind.has_envelope())?;
    let entry = std::str::from_utf8(rec.slice(bytes))
        .ok()
        .and_then(transcript::parse_line)?;
    match entry {
        Entry::User(e) => e.envelope.cwd,
        Entry::Assistant(e) => e.envelope.cwd,
        Entry::System(e) => e.envelope.cwd,
        Entry::Attachment(e) => e.envelope.cwd,
        _ => None,
    }
}

/// Build the rail row for a discovered session.
///
/// The project label prefers the recorded `cwd`'s last component and falls back
/// to the (lossy) project-directory name only when the transcript has none.
pub fn rail_row(session: &SessionRef, summary: &Summary) -> RailRow {
    let project = summary
        .cwd
        .as_deref()
        .map(Path::new)
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            let dir = session
                .project_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            project_label(&dir)
        });

    RailRow {
        session_id: session.session_id.clone(),
        project,
        main_path: session.main_path.clone(),
        // `Summary::agents` counts distinct sidecar `agentId`s; the main agent
        // has none, and it is always there.
        agents: summary.agents + 1,
        failures: summary.failures,
        last_activity: summary.last_activity,
        sidecar_active: session.touched_by_sidecar,
    }
}

/// Discover and summarize every session touched within `window`, as rail rows.
pub fn sweep(window: std::time::Duration) -> Vec<RailRow> {
    discover_recent(window)
        .iter()
        .map(|s| rail_row(s, &Summary::of(s)))
        .collect()
}

/// How far back the rail looks. A day: long enough that "what was I running
/// this morning?" is answerable, short enough that the sweep stays a stat sweep
/// over a handful of candidates rather than the whole history.
pub const SWEEP_WINDOW: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// How often the rail re-sweeps. Matches the tailer's newer-session scan
/// cadence (`SWITCH_SCAN_EVERY`, ~2 s): the rail and the auto-switch are
/// answering the same question about the same directories, so they should not
/// disagree for longer than one of them takes to notice.
pub const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A scratch project tree: `<root>/<project>/<uuid>.jsonl` plus sidecars.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let root = std::env::temp_dir().join(format!(
                "zoetrope-sessions-{}-{}-{tag}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Fixture { root }
        }

        /// Write a main transcript for `uuid` under project `project`.
        fn session(&self, project: &str, uuid: &str, lines: &str) -> PathBuf {
            let dir = self.root.join(project);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{uuid}.jsonl"));
            std::fs::write(&path, lines).unwrap();
            path
        }

        /// Write a direct-subagent sidecar under a session.
        fn sidecar(&self, main: &Path, agent_id: &str, lines: &str) -> PathBuf {
            let subs = transcript::subagents_dir(main).unwrap();
            std::fs::create_dir_all(&subs).unwrap();
            let path = subs.join(format!("agent-{agent_id}.jsonl"));
            std::fs::write(&path, lines).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn user_line(uuid: &str, ts: &str, agent: Option<&str>) -> String {
        let agent = agent.map_or(String::new(), |a| format!(",\"agentId\":\"{a}\""));
        format!(
            "{{\"type\":\"user\",\"uuid\":\"{uuid}\",\"parentUuid\":null,\
             \"timestamp\":\"{ts}\",\"origin\":{{\"kind\":\"human\"}}{agent},\
             \"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n"
        )
    }

    /// Set a file's mtime by rewriting it — portable, and enough to order two
    /// files in time without reaching for a platform-specific utimes call.
    fn touch(path: &Path) {
        let content = std::fs::read(path).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn discovers_sessions_across_every_project() {
        let f = Fixture::new("across");
        f.session(
            "proj-a",
            "aaaaaaaa-1111-2222-3333-444444444444",
            &user_line("u1", "2026-06-01T09:00:00.000Z", None),
        );
        f.session(
            "proj-b",
            "bbbbbbbb-1111-2222-3333-444444444444",
            &user_line("u1", "2026-06-01T09:00:00.000Z", None),
        );
        // A second session in the same project must not shadow the first: the
        // single-session path takes only the newest, this one takes both.
        f.session(
            "proj-a",
            "cccccccc-1111-2222-3333-444444444444",
            &user_line("u1", "2026-06-01T09:00:00.000Z", None),
        );

        let found = discover(&f.root, SystemTime::UNIX_EPOCH);
        assert_eq!(found.len(), 3, "every session in every project");
    }

    #[test]
    fn ignores_non_transcript_files() {
        let f = Fixture::new("filter");
        let dir = f.root.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        // The sidecars that live next to real transcripts, none of which is one.
        for name in [
            "skill-injections.jsonl",
            "sessions-index.json",
            "notes.txt",
            "not-a-uuid.jsonl",
        ] {
            std::fs::write(dir.join(name), "{}\n").unwrap();
        }
        f.session("proj", "dddddddd-1111-2222-3333-444444444444", "{}\n");

        let found = discover(&f.root, SystemTime::UNIX_EPOCH);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "dddddddd-1111-2222-3333-444444444444");
    }

    #[test]
    fn a_session_active_only_in_a_subagent_is_still_found() {
        // The case the main-file mtime filter gets wrong: the main agent is
        // blocked on a subagent, so only the sidecar is being written.
        let f = Fixture::new("sidecar-activity");
        let main = f.session(
            "proj",
            "eeeeeeee-1111-2222-3333-444444444444",
            &user_line("u1", "2026-06-01T09:00:00.000Z", None),
        );
        let main_mtime = std::fs::metadata(&main).unwrap().modified().unwrap();

        let sidecar = f.sidecar(
            &main,
            "a1000000000000001",
            &user_line("s1", "2026-06-01T09:05:00.000Z", Some("a1000000000000001")),
        );
        touch(&sidecar);
        let sidecar_mtime = std::fs::metadata(&sidecar).unwrap().modified().unwrap();

        let found = discover(&f.root, SystemTime::UNIX_EPOCH);
        assert_eq!(found.len(), 1);
        let s = &found[0];
        assert_eq!(s.sidecar_count(), 1);
        assert!(
            s.last_touched >= sidecar_mtime.min(main_mtime),
            "activity is the newest across all files"
        );
        // Filtering from just after the main file's mtime must still find it.
        let since = main_mtime + Duration::from_nanos(1);
        if sidecar_mtime > main_mtime {
            assert_eq!(
                discover(&f.root, since).len(),
                1,
                "sidecar activity keeps the session in the window"
            );
            assert!(s.touched_by_sidecar);
        }
    }

    #[test]
    fn the_window_excludes_old_sessions() {
        let f = Fixture::new("window");
        f.session("proj", "ffffffff-1111-2222-3333-444444444444", "{}\n");
        let future = SystemTime::now() + Duration::from_secs(3600);
        assert!(discover(&f.root, future).is_empty());
        assert_eq!(discover(&f.root, SystemTime::UNIX_EPOCH).len(), 1);
    }

    #[test]
    fn summary_counts_across_main_and_sidecars() {
        let f = Fixture::new("summary");
        let main = f.session(
            "proj",
            "12345678-1111-2222-3333-444444444444",
            &format!(
                "{}{}",
                user_line("u1", "2026-06-01T09:00:00.000Z", None),
                // An assistant turn with two tool calls, one of them a spawn.
                "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":\"u1\",\
                 \"timestamp\":\"2026-06-01T09:00:10.000Z\",\"message\":{\"role\":\"assistant\",\
                 \"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Read\",\"input\":{}},\
                 {\"type\":\"tool_use\",\"id\":\"t2\",\"name\":\"Agent\",\"input\":{}}]}}\n"
            ),
        );
        f.sidecar(
            &main,
            "a1000000000000001",
            &user_line("s1", "2026-06-01T09:01:00.000Z", Some("a1000000000000001")),
        );

        let found = discover(&f.root, SystemTime::UNIX_EPOCH);
        let s = Summary::of(&found[0]);

        assert_eq!(s.files, 2, "main + sidecar");
        assert_eq!(s.lines, 3);
        assert_eq!(s.tool_calls, 2);
        assert_eq!(s.spawns, 1);
        assert_eq!(s.agents, 1, "the sidecar's agentId");
        assert_eq!(
            s.prompts, 2,
            "one in the main transcript, one in the sidecar"
        );
        assert_eq!(s.ambiguous_prompts, 0);
        assert_eq!(
            s.first_activity
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "2026-06-01T09:00:00.000Z"
        );
        assert_eq!(
            s.last_activity
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "2026-06-01T09:01:00.000Z",
            "the newest activity is the subagent's"
        );
    }
}
