//! Providers: one module per transcript format, each turning its own
//! records into the shared [`Fact`](crate::fact::Fact) vocabulary. Everything
//! a format is *for*, what its records mean and how they join, lives here and
//! nowhere else; the model never learns a field name.
//!
//! # Vocabulary
//!
//! - **Core**: [`fact`](crate::fact), the model, the timeline, the UI. Consumes
//!   facts; knows no format.
//! - **Provider**: one per transcript format, `provider/<name>/`. Speaks the
//!   format and presents one uniform surface to everything else: a per-file
//!   [`Stream`] whose `push(line)` yields a [`Statement`], and a few primitives
//!   about its own files (below).
//! - **Feeder**: one per way of getting bytes. The live tailer (polls files),
//!   the replay assembler (reads files up front), `inspect` (reads and prints),
//!   the browser's append (bytes from JS). Bytes in, a provider's stream in the
//!   middle, statements out to the core. Feeders know where bytes come from and
//!   nothing about what they mean; they reach a provider only through
//!   [`open`], [`sweep`] and [`provider_of`].
//!
//! # The two boundaries
//!
//! Output, fixed in `src/fact.rs`: a provider states what its records contain,
//! the core decides what it means when nothing was recorded. Input, fixed here
//! (see `docs/DISCOVERY.md`), one level up:
//!
//! > A provider states what a file is. The core decides what a session is.
//!
//! A provider answers questions about single paths in its own layout
//! ([`SessionFile`]); [`assemble`] builds sessions out of the answers, and
//! [`open`] and [`sweep`] are written once over that. A provider never
//! assembles a session, resolves an id, diffs a rescan, or tails.
//!
//! # Adding a provider
//!
//! A provider is one directory, `provider/<name>/`, with three halves:
//!
//! - `wire.rs` — the serde model for the format's records. Defensive: unknown
//!   record types, missing fields and malformed lines parse to something
//!   skippable, never a panic.
//! - `discovery.rs` — where the format keeps a session on disk and how to tell
//!   what a file is. Pure path logic, directory scans, and at most a head read.
//! - `mod.rs` — the provider: a per-file stream whose `push(line)` yields a
//!   [`Statement`] (the record's own time plus the facts it stated).
//!
//! Then one arm in each `match` below: the compiler lists every site. Its
//! fixtures live in `assets/<name>/`, shaped like the real thing so `discovery`
//! finds them the way it finds a live session. Its conformance test is one
//! call, `harness::conform(name, fixture, streams)`, which checks three things:
//! the model reaches the same state however the files interleave (§1.1), the
//! folded model matches `assets/<name>/<fixture>.model.txt`, and the dated
//! timeline matches `assets/<name>/<fixture>.timeline.txt`. The two goldens are
//! generated with `UPDATE_GOLDEN=1` and are the human-readable record of what
//! the provider extracts. Nothing above `provider/` changes when one is added.
//!
//! A provider's own shape follows its format: the Claude stream carries one
//! inherited timestamp, the Codex stream learns whose file it is from the
//! first line. Both are expected, not deviations.
//!
//! This contract covers agents that write one append-only file per thread.
//! An agent that rewrites a document per turn would add a document push
//! beside `Stream::push`; one that keeps a database is a different kind of
//! feeder, not a wider `SessionFile`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::fact::Statement;

pub mod claude;
pub mod codex;
pub(crate) mod summary;

#[cfg(test)]
pub(crate) mod harness;

/// The transcript formats this build reads. An enum, not a trait: providers
/// arrive by pull request, and an exhaustive `match` makes the compiler list
/// every site a new one must touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    /// The name on the command line and under `assets/`.
    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }

    /// The reverse of [`name`](Self::name), for `--provider`.
    pub fn parse(name: &str) -> Option<Provider> {
        Provider::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(name))
    }
}

// ---------------------------------------------------------------------------
// The uniform surface: one stream per file
// ---------------------------------------------------------------------------

/// One per-file parser, whichever format wrote the file. `push` is the whole
/// reading contract a feeder needs.
#[derive(Debug, Clone)]
pub enum Stream {
    Claude(claude::Stream),
    Codex(codex::Stream),
}

impl Stream {
    /// Parse one line and state what it says. `None` for a blank or
    /// unparsable line, or one that states nothing.
    pub fn push(&mut self, line: &str) -> Option<Statement> {
        match self {
            Stream::Claude(s) => s.push(line),
            Stream::Codex(s) => s.push(line),
        }
    }
}

/// Which provider wrote this text, from its first record. Content, never a
/// path or an extension: a Codex line carries a `payload`, a Claude line a
/// `type` at the top level and nothing else this looks at.
pub fn provider_of(head: &str) -> Option<Provider> {
    let line = head.lines().find(|l| !l.trim().is_empty())?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let obj = v.as_object()?;
    let kind = obj.get("type").and_then(|t| t.as_str());
    if kind == Some("session_meta") || obj.contains_key("payload") {
        return Some(Provider::Codex);
    }
    kind.map(|_| Provider::Claude)
}

// ---------------------------------------------------------------------------
// What a provider states about a file
// ---------------------------------------------------------------------------

/// What a provider states about one path: the input-side analogue of a fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFile {
    pub provider: Provider,
    pub path: PathBuf,
    /// The session this file belongs to. Every format seen names the root
    /// from any of its files, so no parent chain has to be walked.
    pub session: String,
    pub role: FileRole,
    pub read: ReadMode,
    /// Compared with [`Provider::project_key`], never with a path.
    pub project_key: String,
    pub modified: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRole {
    /// The session's own transcript.
    Root,
    /// A spawned agent's transcript. `parent` is the thread that spawned it,
    /// which the fact stream states again with more detail.
    Agent { parent: String },
    /// A session-level file that is nobody's transcript: a meta sidecar, a
    /// workflow ledger.
    Sidecar,
}

/// How a file is read: line by line as it grows, or whole, once it parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    Tail,
    Whole,
}

/// What the core assembles from files. Never produced by a provider.
#[derive(Debug, Clone)]
pub struct Session {
    pub provider: Provider,
    pub id: String,
    pub project_key: String,
    pub root: SessionFile,
    /// Agents and sidecars, root excluded.
    pub files: Vec<SessionFile>,
    pub last_modified: SystemTime,
    /// Paths near this session that classified as some other session's. A
    /// file's identity never changes once written, so a rescan skips them
    /// instead of reading their heads again on every tick. A path that did
    /// not classify at all (a file caught mid-creation) is not remembered,
    /// so it is retried.
    rejected: std::collections::HashSet<PathBuf>,
}

impl Session {
    /// Every file of the session, root first.
    pub fn every_file(&self) -> impl Iterator<Item = &SessionFile> {
        std::iter::once(&self.root).chain(self.files.iter())
    }

    /// Files that belong to this session and were not known before. They are
    /// added to `files` and returned. The live tailer's per-tick scan.
    pub fn rescan(&mut self) -> Vec<SessionFile> {
        let p = self.provider;
        // The root keeps being written while the session runs; a provider
        // that bounds the search by the root's last write needs the current
        // one, not the one seen at open.
        self.root.modified = modified(&self.root.path);
        let mut found = Vec::new();
        for path in p.related_paths(&self.root) {
            if path == self.root.path
                || self.rejected.contains(&path)
                || self.files.iter().any(|f| f.path == path)
            {
                continue;
            }
            match p.session_file(&path) {
                Some(f) if f.session == self.id && f.role != FileRole::Root => found.push(f),
                Some(_) => {
                    self.rejected.insert(path);
                }
                None => {}
            }
        }
        self.files.extend(found.iter().cloned());
        found
    }
}

/// Which sessions a [`sweep`] is for.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Only sessions of this project (a working directory).
    pub project: Option<PathBuf>,
    /// Only files modified since. A provider that can prune by it does.
    pub since: Option<SystemTime>,
    /// Only sessions whose id starts with this. A provider whose file names
    /// carry the id prunes by it without reading anything; the core still
    /// checks the id on what comes back.
    pub id_prefix: Option<String>,
}

impl Scope {
    pub const ALL: Scope = Scope {
        project: None,
        since: None,
        id_prefix: None,
    };

    pub fn project(cwd: &Path) -> Scope {
        Scope {
            project: Some(cwd.to_path_buf()),
            since: None,
            id_prefix: None,
        }
    }

    pub fn id(prefix: &str) -> Scope {
        Scope {
            project: None,
            since: None,
            id_prefix: Some(prefix.to_string()),
        }
    }

    pub fn since(mut self, t: SystemTime) -> Scope {
        self.since = Some(t);
        self
    }

    fn admits(&self, s: &Session) -> bool {
        self.project
            .as_deref()
            .is_none_or(|cwd| s.project_key == s.provider.project_key(cwd))
            && self.since.is_none_or(|t| s.last_modified >= t)
            && self
                .id_prefix
                .as_deref()
                .is_none_or(|p| s.id.starts_with(p))
    }
}

/// What the user pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A file: a session's root, or any file of one.
    Path(PathBuf),
    /// A session id, or a unique prefix of one.
    Id(String),
    /// The newest session of the project at this working directory.
    Here(PathBuf),
}

/// Why [`open`] found nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// Not a transcript any provider recognises.
    Unrecognized(PathBuf),
    /// The file is a session's, but its session has no root file to be found.
    Orphan(PathBuf),
    NotFound(Target),
    /// The id prefix matched more than one session.
    Ambiguous(Vec<String>),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Unrecognized(p) => {
                write!(f, "not a transcript any provider reads: {}", p.display())
            }
            OpenError::Orphan(p) => write!(f, "no session found for {}", p.display()),
            OpenError::NotFound(Target::Path(p)) => write!(f, "not found: {}", p.display()),
            OpenError::NotFound(Target::Id(id)) => write!(f, "no session with id {id}"),
            OpenError::NotFound(Target::Here(cwd)) => {
                write!(f, "no session for {}", cwd.display())
            }
            OpenError::Ambiguous(ids) => write!(f, "ambiguous id, matches: {}", ids.join(", ")),
        }
    }
}

impl std::error::Error for OpenError {}

// ---------------------------------------------------------------------------
// The primitives, dispatched
// ---------------------------------------------------------------------------

impl Provider {
    /// Every path that could be a session file, across this provider's roots.
    /// A provider prunes by `scope` where its layout lets it.
    pub fn all_paths(self, scope: &Scope) -> Vec<PathBuf> {
        match self {
            Provider::Claude => claude::discovery::all_paths(scope),
            Provider::Codex => codex::discovery::all_paths(scope),
        }
    }

    /// What is this file? Which session, which role, which project.
    pub fn session_file(self, path: &Path) -> Option<SessionFile> {
        match self {
            Provider::Claude => claude::discovery::session_file(path),
            Provider::Codex => codex::discovery::session_file(path),
        }
    }

    /// Where the rest of this file's session can be, root included when
    /// `file` is not it. May over-include; [`session_file`](Self::session_file)
    /// sorts the answers out.
    pub fn related_paths(self, file: &SessionFile) -> Vec<PathBuf> {
        match self {
            Provider::Claude => claude::discovery::related_paths(file),
            Provider::Codex => codex::discovery::related_paths(file),
        }
    }

    /// How this provider names the project at a working directory.
    pub fn project_key(self, cwd: &Path) -> String {
        match self {
            Provider::Claude => claude::discovery::project_key(cwd),
            Provider::Codex => codex::discovery::project_key(cwd),
        }
    }

    /// [`session_file`](Self::session_file) without a filesystem: the path a
    /// file came with and its first bytes. A Claude file is classified by its
    /// path, a Codex file by its first line. The browser's way in.
    pub fn classify(self, path: &Path, head: &str) -> Option<SessionFile> {
        match self {
            Provider::Claude => claude::discovery::classify_path(path, SystemTime::UNIX_EPOCH),
            Provider::Codex => codex::discovery::classify_head(path, head, SystemTime::UNIX_EPOCH),
        }
    }

    /// A parser for one of this provider's tailed files.
    pub fn stream_for(self, file: &SessionFile) -> Stream {
        match self {
            Provider::Claude => Stream::Claude(claude::discovery::stream_for(file)),
            Provider::Codex => Stream::Codex(codex::Stream::new()),
        }
    }

    /// What one of this provider's whole-read sidecars states, once its text
    /// parses. `None` while it does not (a mid-write read) or if the provider
    /// has no such files.
    pub fn sidecar(self, file: &SessionFile, text: &str) -> Option<Statement> {
        match self {
            Provider::Claude => claude::discovery::sidecar(file, text),
            Provider::Codex => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The core: assemble, sweep, open
// ---------------------------------------------------------------------------

/// Group files into sessions. A group without a root file is not a session
/// and is dropped. Newest first.
pub fn assemble(files: Vec<SessionFile>) -> Vec<Session> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<(Provider, String), Vec<SessionFile>> = BTreeMap::new();
    for f in files {
        groups
            .entry((f.provider, f.session.clone()))
            .or_default()
            .push(f);
    }
    let mut out = Vec::new();
    for ((provider, id), mut group) in groups {
        let Some(root_at) = group.iter().position(|f| f.role == FileRole::Root) else {
            continue;
        };
        let root = group.remove(root_at);
        group.retain(|f| f.role != FileRole::Root);
        let last_modified = group
            .iter()
            .map(|f| f.modified)
            .chain(std::iter::once(root.modified))
            .max()
            .unwrap_or(root.modified);
        out.push(Session {
            provider,
            id,
            project_key: root.project_key.clone(),
            root,
            files: group,
            last_modified,
            rejected: std::collections::HashSet::new(),
        });
    }
    out.sort_by(|a, b| {
        b.last_modified
            .cmp(&a.last_modified)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// Every session under every provider's roots that `scope` admits, newest
/// first. `only` restricts to one provider.
pub fn sweep(scope: &Scope, only: Option<Provider>) -> Vec<Session> {
    let mut files = Vec::new();
    for p in Provider::ALL {
        if only.is_some_and(|o| o != p) {
            continue;
        }
        for path in p.all_paths(scope) {
            if let Some(f) = p.session_file(&path) {
                files.push(f);
            }
        }
    }
    assemble(files)
        .into_iter()
        .filter(|s| scope.admits(s))
        .collect()
}

/// One session, with every file of it known at this moment. The provider is
/// read off the content for a path, and found by looking for an id or the
/// newest session at a directory otherwise.
pub fn open(target: &Target, only: Option<Provider>) -> Result<Session, OpenError> {
    let file = match target {
        Target::Path(path) => {
            if !path.is_file() {
                return Err(OpenError::NotFound(target.clone()));
            }
            // Content first; failing that (an empty, just-created file), the
            // first provider whose layout claims the path.
            let p = only
                .or_else(|| read_head(path).and_then(|head| provider_of(&head)))
                .or_else(|| {
                    Provider::ALL
                        .into_iter()
                        .find(|p| p.session_file(path).is_some())
                })
                .ok_or_else(|| OpenError::Unrecognized(path.clone()))?;
            // Any file of a session opens the session: the rest of it, root
            // included, is wherever the provider says files of one session are.
            p.session_file(path)
                .ok_or_else(|| OpenError::Unrecognized(path.clone()))?
        }
        // A sweep lists what `all_paths` returned, which for Claude is roots
        // only: the hit is expanded to its files below like any other.
        Target::Id(prefix) => {
            let mut hits: Vec<Session> = sweep(&Scope::id(prefix), only);
            match hits.len() {
                0 => return Err(OpenError::NotFound(target.clone())),
                1 => hits.remove(0).root,
                _ => {
                    return Err(OpenError::Ambiguous(
                        hits.into_iter().map(|s| s.id).collect(),
                    ));
                }
            }
        }
        Target::Here(cwd) => {
            sweep(&Scope::project(cwd), only)
                .into_iter()
                .next()
                .ok_or_else(|| OpenError::NotFound(target.clone()))?
                .root
        }
    };
    expand(file)
}

/// Every file of the session `file` belongs to, as of now: the file itself,
/// plus whatever the provider says is near it and classifies to the same
/// session. The one place a session is assembled for `open`.
fn expand(file: SessionFile) -> Result<Session, OpenError> {
    let p = file.provider;
    let mut files = vec![file.clone()];
    for path in p.related_paths(&file) {
        if path == file.path {
            continue;
        }
        if let Some(f) = p.session_file(&path)
            && f.session == file.session
        {
            files.push(f);
        }
    }
    assemble(files)
        .into_iter()
        .find(|s| s.id == file.session)
        .ok_or(OpenError::Orphan(file.path.clone()))
}

/// The first non-blank line of a file, for [`provider_of`].
fn read_head(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    while reader.read_line(&mut line).ok()? > 0 {
        if !line.trim().is_empty() {
            return Some(line);
        }
        line.clear();
    }
    None
}

/// A file's modification time, or the epoch if it cannot be read.
pub(crate) fn modified(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_is_read_off_the_first_record() {
        assert_eq!(
            provider_of(
                r#"{"timestamp":"t","ordinal":0,"type":"session_meta","payload":{"id":"a"}}"#
            ),
            Some(Provider::Codex)
        );
        assert_eq!(
            provider_of("\n{\"type\":\"response_item\",\"payload\":{\"type\":\"message\"}}\n"),
            Some(Provider::Codex)
        );
        assert_eq!(
            provider_of(r#"{"type":"mode","mode":"normal","sessionId":"demo"}"#),
            Some(Provider::Claude)
        );
        assert_eq!(
            provider_of(
                r#"{"type":"user","uuid":"u","agentId":"a1","message":{"role":"user","content":"x"}}"#
            ),
            Some(Provider::Claude)
        );
        assert_eq!(provider_of(r#"{"agentType":"guide"}"#), None);
        assert_eq!(provider_of("not json"), None);
        assert_eq!(provider_of(""), None);
    }

    #[test]
    fn scope_admits_by_id_prefix_project_and_time() {
        let f = |session: &str| SessionFile {
            provider: Provider::Codex,
            path: PathBuf::from(format!("/{session}")),
            session: session.into(),
            role: FileRole::Root,
            read: ReadMode::Tail,
            project_key: "/p".into(),
            modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100),
        };
        let s = &assemble(vec![f("01a03eb1-aaaa")])[0];
        assert!(Scope::ALL.admits(s));
        assert!(Scope::id("01a03").admits(s));
        assert!(!Scope::id("01a04").admits(s));
        assert!(Scope::project(Path::new("/p")).admits(s));
        assert!(!Scope::project(Path::new("/q")).admits(s));
        assert!(Scope::ALL.since(SystemTime::UNIX_EPOCH).admits(s));
        assert!(
            !Scope::ALL
                .since(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(200))
                .admits(s)
        );
    }

    #[test]
    fn assemble_groups_by_session_and_drops_orphans() {
        let f = |provider, session: &str, role, t: u64| SessionFile {
            provider,
            path: PathBuf::from(format!("/{session}/{t}")),
            session: session.into(),
            role,
            read: ReadMode::Tail,
            project_key: "k".into(),
            modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(t),
        };
        let sessions = assemble(vec![
            f(
                Provider::Claude,
                "a",
                FileRole::Agent { parent: "a".into() },
                5,
            ),
            f(Provider::Claude, "a", FileRole::Root, 1),
            f(Provider::Codex, "b", FileRole::Root, 3),
            f(
                Provider::Codex,
                "orphan",
                FileRole::Agent { parent: "x".into() },
                9,
            ),
        ]);
        assert_eq!(sessions.len(), 2);
        // Newest first: `a` was touched at 5 by its agent file.
        assert_eq!(sessions[0].id, "a");
        assert_eq!(sessions[0].files.len(), 1);
        assert_eq!(sessions[1].id, "b");
        assert!(sessions[1].files.is_empty());
    }

    /// A live rescan reads the head of an unrelated rollout once, not on
    /// every tick: after the first pass it is remembered as not ours.
    #[test]
    fn rescan_classifies_a_stranger_once() {
        let Some(codex_dir) = harness::fixture_dir("codex") else {
            return;
        };
        // Two sessions share the August day directory: opening one leaves
        // the other's rollouts as strangers.
        let day = codex_dir.join("cli-0.149.1/2026/08/26");
        let mut rollouts: Vec<PathBuf> = std::fs::read_dir(&day)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        rollouts.sort();
        let mut s = open(&Target::Path(rollouts[0].clone()), None).unwrap();
        let known = s.files.len();
        assert!(s.rescan().is_empty(), "nothing new on a settled session");
        assert_eq!(s.files.len(), known);
        // Every path near the root is now either adopted or remembered.
        let near = s.provider.related_paths(&s.root).len();
        assert_eq!(s.files.len() + s.rejected.len(), near);
    }

    /// The shipped fixtures, opened by path the way `zoe <file>` does: the
    /// provider comes from the content, every file of the session is found,
    /// and a child file opens the same session as its root.
    #[test]
    fn open_finds_every_file_of_a_fixture_session() {
        let Some(claude_dir) = harness::fixture_dir("claude") else {
            return;
        };
        let s = open(&Target::Path(claude_dir.join("demo.jsonl")), None).unwrap();
        assert_eq!(s.provider, Provider::Claude);
        assert_eq!(s.id, "demo");
        let count = |s: &Session, role: fn(&FileRole) -> bool| {
            s.files.iter().filter(|f| role(&f.role)).count()
        };
        assert_eq!(count(&s, |r| matches!(r, FileRole::Agent { .. })), 6);
        assert_eq!(
            count(&s, |r| *r == FileRole::Sidecar),
            7,
            "six metas + one journal"
        );

        let Some(codex_dir) = harness::fixture_dir("codex") else {
            return;
        };
        let day = codex_dir.join("cli-0.153.4/2026/09/07");
        let mut rollouts: Vec<PathBuf> = std::fs::read_dir(&day)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        rollouts.sort();
        let root = open(&Target::Path(rollouts[0].clone()), None).unwrap();
        assert_eq!(root.provider, Provider::Codex);
        assert_eq!(root.files.len(), 4);
        assert!(
            root.files
                .iter()
                .all(|f| matches!(f.role, FileRole::Agent { .. }))
        );
        // A child opens its session.
        let via_child = open(&Target::Path(rollouts[1].clone()), None).unwrap();
        assert_eq!(via_child.id, root.id);
        assert_eq!(via_child.root.path, root.root.path);
    }
}
