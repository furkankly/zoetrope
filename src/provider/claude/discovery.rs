//! Where Claude Code keeps a session on disk, and how one session's files are
//! found: `~/.claude/projects/<sanitized-cwd>/<uuid>.jsonl`, plus the
//! `<uuid>/subagents/` tree of per-agent transcripts, `meta.json` sidecars and
//! per-workflow `journal.jsonl` ledgers. Pure path logic plus directory scans;
//! nothing here reads a line.

// ---------------------------------------------------------------------------
// Directory discovery / sanitization
// ---------------------------------------------------------------------------

/// Sanitize an absolute cwd into the project-directory name Claude Code uses:
/// every character that is not `[a-zA-Z0-9]` becomes a single `-`, one-to-one
/// (a leading slash becomes a leading dash; `/Users/me/.config` →
/// `-Users-me--config`). This is Claude Code's documented rule — "non-
/// alphanumeric characters replaced by `-`" — so `.`, `_`, and spaces all map
/// to dashes, not just path separators.
///
/// Operates on the string form of the path so it is platform-agnostic and never
/// touches the filesystem. The canonical input is an absolute Unix path.
pub fn sanitize_cwd(cwd: &std::path::Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The `~/.claude/projects` root, if a home directory can be resolved.
fn claude_projects_root() -> Option<std::path::PathBuf> {
    #[allow(deprecated)]
    let home = std::env::home_dir()
        .filter(|h| !h.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))?;
    Some(home.join(".claude").join("projects"))
}

/// Absolute path to the `~/.claude/projects/<sanitized-cwd>` directory for a
/// given cwd.
pub fn project_dir(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    Some(claude_projects_root()?.join(sanitize_cwd(cwd)))
}

/// Whether a filename stem is a canonical lowercase UUID (8-4-4-4-12 hex).
///
/// Transcript files are exactly `<uuid>.jsonl`; this filter rejects sidecars
/// like `skill-injections.jsonl` and metadata like `sessions-index.json`.
fn is_uuid(stem: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut parts = stem.split('-');
    for &len in &GROUPS {
        match parts.next() {
            Some(p) if p.len() == len && p.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

/// Whether a path is a `<uuid>.jsonl` transcript file (UUID stem + `.jsonl`).
pub fn is_session_file(path: &std::path::Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return false;
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(is_uuid)
}

/// Find the newest `<uuid>.jsonl` transcript directly inside `project_dir`
/// (ignoring non-transcript files like `skill-injections.jsonl`,
/// `sessions-index.json`, and subdirectories).
pub fn latest_session_file(project_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(project_dir).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !is_session_file(&path) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        // Newest wins; equal mtimes break ties on the (lexicographically greater)
        // path so the choice is deterministic, not `read_dir` order.
        let better = match &best {
            None => true,
            Some((bt, bp)) => mtime > *bt || (mtime == *bt && path > *bp),
        };
        if better {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

// ---------------------------------------------------------------------------
// Subagent directory scanning
// ---------------------------------------------------------------------------

/// A discovered subagent file pair inside a `subagents/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentFile {
    /// The 17-hex `agentId` parsed from the `agent-<id>.jsonl` filename.
    pub agent_id: String,
    /// Absolute path to the `agent-<id>.jsonl` transcript.
    pub transcript: std::path::PathBuf,
    /// Absolute path to the `agent-<id>.meta.json` sidecar (may not exist yet).
    pub meta: std::path::PathBuf,
    /// `Some(wf_id)` when this lives under `subagents/workflows/<wf-id>/`.
    pub workflow: Option<String>,
}

/// The `subagents/` directory for a session transcript path.
///
/// `<dir>/<uuid>.jsonl` → `<dir>/<uuid>/subagents`.
pub fn subagents_dir(session_file: &std::path::Path) -> Option<std::path::PathBuf> {
    let parent = session_file.parent()?;
    let stem = session_file.file_stem()?.to_str()?;
    Some(parent.join(stem).join("subagents"))
}

/// Extract the `agentId` from an `agent-<id>.jsonl` filename, if it matches.
fn agent_id_from_filename(path: &std::path::Path) -> Option<String> {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    stem.strip_prefix("agent-")
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Scan a single `subagents/` (or `subagents/workflows/<wf-id>/`) directory for
/// `agent-*.jsonl` files, pairing each with its `.meta.json` sidecar.
///
/// `workflow` tags the discovered files; missing directories yield an empty
/// vec (subagent dirs are created lazily — that is expected, never an error).
pub fn scan_subagent_files(dir: &std::path::Path, workflow: Option<&str>) -> Vec<SubagentFile> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Some(agent_id) = agent_id_from_filename(&path) else {
            continue;
        };
        let meta = dir.join(format!("agent-{agent_id}.meta.json"));
        out.push(SubagentFile {
            agent_id,
            transcript: path,
            meta,
            workflow: workflow.map(str::to_owned),
        });
    }
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    out
}

/// List workflow ids found under `subagents/workflows/` (each is a directory
/// containing a `journal.jsonl` plus its own `agent-*.jsonl` files).
pub fn scan_workflow_ids(subagents_dir: &std::path::Path) -> Vec<String> {
    let workflows = subagents_dir.join("workflows");
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(&workflows) else {
        return out;
    };
    for entry in read.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            && let Some(name) = entry.file_name().to_str()
        {
            out.push(name.to_owned());
        }
    }
    out.sort();
    out
}

/// The `journal.jsonl` ledger path for a workflow inside a `subagents/` dir.
pub fn workflow_journal(subagents_dir: &std::path::Path, wf_id: &str) -> std::path::PathBuf {
    subagents_dir
        .join("workflows")
        .join(wf_id)
        .join("journal.jsonl")
}

/// The `subagents/workflows/<wf-id>` directory that holds a workflow's agents.
pub fn workflow_dir(subagents_dir: &std::path::Path, wf_id: &str) -> std::path::PathBuf {
    subagents_dir.join("workflows").join(wf_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The provider primitives (see `provider/mod.rs` and docs/DISCOVERY.md)
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::provider::{FileRole, Provider, ReadMode, Scope, SessionFile};

/// Every `<uuid>.jsonl` under `~/.claude/projects/*/`, or under the one
/// project directory when the scope names a working directory. Only roots:
/// a session's other files are found from its root.
pub fn all_paths(scope: &Scope) -> Vec<PathBuf> {
    let dirs: Vec<PathBuf> = match &scope.project {
        Some(cwd) => project_dir(cwd).into_iter().collect(),
        None => claude_projects_root()
            .and_then(|root| std::fs::read_dir(root).ok())
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !is_session_file(&path) {
                continue;
            }
            if let Some(since) = scope.since
                && crate::provider::modified(&path) < since
            {
                continue;
            }
            // The session id is the file stem: prune without reading.
            if let Some(prefix) = &scope.id_prefix
                && !path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with(prefix.as_str()))
            {
                continue;
            }
            out.push(path);
        }
    }
    out.sort();
    out
}

/// What a path is, by where it sits in the project layout:
///
/// - `<key>/<uuid>.jsonl` is the root;
/// - `<key>/<uuid>/subagents/agent-<id>.jsonl` and
///   `<key>/<uuid>/subagents/workflows/<wf>/agent-<id>.jsonl` are agents;
/// - their `.meta.json` sidecars and a workflow's `journal.jsonl` are sidecars,
///   the meta read whole (one JSON document, no trailing newline), the
///   journal tailed.
pub fn session_file(path: &Path) -> Option<SessionFile> {
    classify_path(path, crate::provider::modified(path))
}

/// [`session_file`] without the filesystem: pure path logic, so the browser
/// can classify a dropped file by the path it came with. Scans keep to
/// `<uuid>.jsonl` (see `all_paths`); a path handed over explicitly may be a
/// fixture with any name, and the content already said it is Claude's.
pub fn classify_path(path: &Path, modified: SystemTime) -> Option<SessionFile> {
    let file = |session: &str, role, read, key: &str| SessionFile {
        provider: Provider::Claude,
        path: path.to_path_buf(),
        session: session.to_string(),
        role,
        read,
        project_key: key.to_string(),
        modified,
    };
    let dir_name = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_owned);
    // A transcript directly in a project directory is a root.
    let under_subagents = path
        .ancestors()
        .skip(1)
        .any(|d| d.file_name().is_some_and(|n| n == "subagents"));
    if path.extension().is_some_and(|e| e == "jsonl") && !under_subagents {
        let session = path.file_stem()?.to_str()?;
        let key = path.parent().and_then(dir_name).unwrap_or_default();
        return Some(file(session, FileRole::Root, ReadMode::Tail, &key));
    }
    // Walk up to a `subagents` directory whose parent is the session's
    // `<uuid>` directory, inside the project directory.
    let mut cursor = path.parent();
    let mut subagents: Option<&Path> = None;
    while let Some(dir) = cursor {
        if dir.file_name().is_some_and(|n| n == "subagents") {
            subagents = Some(dir);
            break;
        }
        cursor = dir.parent();
    }
    let subagents = subagents?;
    let session_dir = subagents.parent()?;
    let session = session_dir.file_name()?.to_str()?;
    let key = session_dir.parent().and_then(dir_name).unwrap_or_default();
    let name = path.file_name()?.to_str()?;
    if name == "journal.jsonl" {
        return Some(file(session, FileRole::Sidecar, ReadMode::Tail, &key));
    }
    if let Some(agent) = name.strip_prefix("agent-") {
        if agent.ends_with(".meta.json") {
            return Some(file(session, FileRole::Sidecar, ReadMode::Whole, &key));
        }
        if agent.ends_with(".jsonl") {
            return Some(file(
                session,
                FileRole::Agent {
                    parent: session.to_string(),
                },
                ReadMode::Tail,
                &key,
            ));
        }
    }
    None
}

/// The root transcript beside a `<uuid>` session directory, if there is one.
/// Lets a fixture whose session id is not a uuid (`demo`) classify its files.
fn session_file_of_dir(session_dir: &Path) -> Option<PathBuf> {
    let stem = session_dir.file_name()?.to_str()?;
    let root = session_dir.parent()?.join(format!("{stem}.jsonl"));
    root.is_file().then_some(root)
}

/// Where the rest of a file's session is: the root beside the `<uuid>`
/// directory, and everything under its `subagents/`: agent transcripts and
/// metas, workflow journals and their agents.
pub fn related_paths(file: &SessionFile) -> Vec<PathBuf> {
    let root = match file.role {
        FileRole::Root => file.path.clone(),
        _ => match root_of(&file.path) {
            Some(r) => r,
            None => return Vec::new(),
        },
    };
    let Some(subs) = subagents_dir(&root) else {
        return Vec::new();
    };
    let mut out = vec![root];
    for f in scan_subagent_files(&subs, None) {
        out.push(f.transcript);
        out.push(f.meta);
    }
    for wf in scan_workflow_ids(&subs) {
        out.push(workflow_journal(&subs, &wf));
        for f in scan_subagent_files(&workflow_dir(&subs, &wf), Some(&wf)) {
            out.push(f.transcript);
            out.push(f.meta);
        }
    }
    out.retain(|p| p.is_file());
    out
}

/// Claude names a project by its sanitized working directory.
pub fn project_key(cwd: &Path) -> String {
    sanitize_cwd(cwd)
}

/// The stream for one of the session's tailed files: which [`Source`](super::Source) it is
/// comes off the path.
pub fn stream_for(file: &SessionFile) -> super::Stream {
    use super::{Source, Stream};
    let source = match &file.role {
        FileRole::Root => Source::Main,
        FileRole::Agent { .. } => {
            Source::Sub(agent_id_from_filename(&file.path).unwrap_or_else(|| file.session.clone()))
        }
        FileRole::Sidecar => Source::Ledger(workflow_of(&file.path).unwrap_or_default()),
    };
    Stream::new(source)
}

/// What a `meta.json` states, once it parses.
pub fn sidecar(file: &SessionFile, text: &str) -> Option<crate::fact::Statement> {
    let name = file.path.file_name()?.to_str()?;
    let agent = name
        .strip_prefix("agent-")?
        .strip_suffix(".meta.json")?
        .to_string();
    let meta = super::wire::parse_meta(text)?;
    Some(super::Stream::meta(
        &agent,
        workflow_of(&file.path).as_deref(),
        &meta,
    ))
}

/// The workflow id a path sits under (`subagents/workflows/<wf>/...`), if any.
fn workflow_of(path: &Path) -> Option<String> {
    let mut cursor = path.parent();
    while let Some(dir) = cursor {
        let parent = dir.parent()?;
        if parent.file_name().is_some_and(|n| n == "workflows")
            && parent
                .parent()
                .is_some_and(|s| s.file_name().is_some_and(|n| n == "subagents"))
        {
            return dir.file_name()?.to_str().map(str::to_owned);
        }
        cursor = Some(parent);
    }
    None
}

/// The root transcript of the session a non-root file belongs to:
/// `<key>/<uuid>.jsonl` for anything under `<key>/<uuid>/subagents/`.
fn root_of(path: &Path) -> Option<PathBuf> {
    let mut cursor = path.parent();
    while let Some(dir) = cursor {
        if dir.file_name().is_some_and(|n| n == "subagents") {
            return session_file_of_dir(dir.parent()?);
        }
        cursor = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // --- Sanitization ------------------------------------------------------

    #[test]
    fn sanitize_cwd_rule() {
        assert_eq!(
            sanitize_cwd(Path::new("/Users/furkan/personal/projects/flyradar")),
            "-Users-furkan-personal-projects-flyradar"
        );
        // Leading slash → leading dash; root stays a single dash.
        assert_eq!(sanitize_cwd(Path::new("/")), "-");
        assert_eq!(sanitize_cwd(Path::new("/a")), "-a");
        // Every non-alphanumeric char maps to a dash, one-to-one: a dotfile dir
        // yields a double dash (slash + dot), and `_`/spaces become dashes too.
        assert_eq!(
            sanitize_cwd(Path::new("/Users/me/.config/foo")),
            "-Users-me--config-foo"
        );
        assert_eq!(
            sanitize_cwd(Path::new("/Users/me/my_project v2")),
            "-Users-me-my-project-v2"
        );
    }

    // --- UUID filename filter ---------------------------------------------

    #[test]
    fn uuid_filename_filter() {
        assert!(is_uuid("0e599cbe-23c4-460b-b097-cbd1d6bc0e3d"));
        assert!(is_uuid("55badaf6-c5d2-4b85-af5b-f41f42b3a8a7"));
        // Wrong group lengths / shapes.
        assert!(!is_uuid("0e599cbe-23c4-460b-b097-cbd1d6bc0e3"));
        assert!(!is_uuid("0e599cbe23c4460bb097cbd1d6bc0e3d"));
        assert!(!is_uuid("skill-injections"));
        assert!(!is_uuid("sessions-index"));
        // Non-hex characters rejected.
        assert!(!is_uuid("zzzzzzzz-23c4-460b-b097-cbd1d6bc0e3d"));
        // Trailing group rejected.
        assert!(!is_uuid("0e599cbe-23c4-460b-b097-cbd1d6bc0e3d-extra"));
    }

    #[test]
    fn is_session_file_only_uuid_jsonl() {
        assert!(is_session_file(Path::new(
            "/p/0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.jsonl"
        )));
        // Rejected: known non-transcript sidecars and wrong extensions.
        assert!(!is_session_file(Path::new("/p/skill-injections.jsonl")));
        assert!(!is_session_file(Path::new("/p/sessions-index.json")));
        assert!(!is_session_file(Path::new("/p/journal.jsonl")));
        assert!(!is_session_file(Path::new(
            "/p/0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.json"
        )));
    }

    // --- Subagent path helpers --------------------------------------------

    #[test]
    fn subagents_dir_derivation() {
        let session = Path::new("/root/-proj/0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.jsonl");
        let dir = subagents_dir(session).expect("derivable");
        assert_eq!(
            dir,
            Path::new("/root/-proj/0e599cbe-23c4-460b-b097-cbd1d6bc0e3d/subagents")
        );
    }

    #[test]
    fn agent_id_from_filename_parsing() {
        assert_eq!(
            agent_id_from_filename(Path::new("/x/agent-a5301c73ab04591b2.jsonl")).as_deref(),
            Some("a5301c73ab04591b2")
        );
        // meta.json is not a transcript file.
        assert!(
            agent_id_from_filename(Path::new("/x/agent-a5301c73ab04591b2.meta.json")).is_none()
        );
        // No agent- prefix.
        assert!(agent_id_from_filename(Path::new("/x/journal.jsonl")).is_none());
        // Empty id rejected.
        assert!(agent_id_from_filename(Path::new("/x/agent-.jsonl")).is_none());
    }

    #[test]
    fn workflow_path_helpers() {
        let sub = Path::new("/s/subagents");
        assert_eq!(
            workflow_journal(sub, "wf_6e734a65-3c6"),
            Path::new("/s/subagents/workflows/wf_6e734a65-3c6/journal.jsonl")
        );
        assert_eq!(
            workflow_dir(sub, "wf_6e734a65-3c6"),
            Path::new("/s/subagents/workflows/wf_6e734a65-3c6")
        );
    }

    #[test]
    fn scan_missing_dir_is_empty_not_error() {
        // Lazily-created dirs: scanning a nonexistent path is a no-op.
        let missing = Path::new("/definitely/not/a/real/zoetrope/subagents/xyz");
        assert!(scan_subagent_files(missing, None).is_empty());
        assert!(scan_workflow_ids(missing).is_empty());
    }

    #[test]
    fn scan_subagent_files_pairs_transcript_and_meta() {
        let tmp = std::env::temp_dir().join(format!(
            "zoetrope-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        std::fs::write(tmp.join("agent-a5301c73ab04591b2.jsonl"), b"{}\n").unwrap();
        std::fs::write(tmp.join("agent-a5301c73ab04591b2.meta.json"), b"{}").unwrap();
        std::fs::write(tmp.join("agent-a9dd56e1137830d9d.jsonl"), b"{}\n").unwrap();
        // Noise that must be ignored.
        std::fs::write(tmp.join("journal.jsonl"), b"{}\n").unwrap();
        std::fs::write(tmp.join("readme.txt"), b"x").unwrap();

        let found = scan_subagent_files(&tmp, Some("wf_x"));
        assert_eq!(found.len(), 2, "two agent transcripts, noise ignored");
        // Sorted by agent_id.
        assert_eq!(found[0].agent_id, "a5301c73ab04591b2");
        assert_eq!(found[1].agent_id, "a9dd56e1137830d9d");
        assert_eq!(found[0].workflow.as_deref(), Some("wf_x"));
        assert_eq!(found[0].meta, tmp.join("agent-a5301c73ab04591b2.meta.json"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn latest_session_file_picks_newest_uuid_jsonl() {
        let tmp = std::env::temp_dir().join(format!(
            "zoetrope-latest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let older = tmp.join("11111111-1111-1111-1111-111111111111.jsonl");
        let newer = tmp.join("22222222-2222-2222-2222-222222222222.jsonl");
        // Non-transcript files must be ignored even if they are the newest.
        std::fs::write(tmp.join("skill-injections.jsonl"), b"{}\n").unwrap();
        std::fs::write(tmp.join("sessions-index.json"), b"{}\n").unwrap();
        std::fs::write(&older, b"{}\n").unwrap();
        // Ensure a real mtime gap across coarse-granularity filesystems, then
        // write `newer` strictly after `older`.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&newer, b"{}\n").unwrap();

        let latest = latest_session_file(&tmp).expect("finds one");
        assert_eq!(latest, newer, "newest uuid .jsonl wins; sidecars ignored");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
