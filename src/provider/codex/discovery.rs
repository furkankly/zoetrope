//! Where Codex keeps its sessions on disk, and how to tell what a rollout is.
//!
//! Every thread, root or spawned, is one file:
//! `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<thread-id>.jsonl`.
//! Nothing in the path says which session a file belongs to; that is the
//! file's first line, its own `session_meta`, which carries the thread id, the
//! root's id, and for a child the parent's. A child rollout also carries a
//! second `session_meta` further down: its parent's, replayed. Only the first
//! identifies the file.
//!
//! Children of one session land in the root's date directory or a later one,
//! never earlier, which is what bounds the search for a session's files.

use std::path::{Path, PathBuf};

use super::wire::{Payload, SessionMeta, parse_line};

/// The Codex home: `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".codex"))
}

/// The sessions root under a Codex home.
pub fn sessions_dir(home: &Path) -> PathBuf {
    home.join("sessions")
}

/// Whether a path is shaped like a rollout file. Shape only; the first line
/// decides what it is.
pub fn is_rollout_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-"))
}

/// The thread id a rollout file name ends in, if it is shaped like one.
/// Cheaper than reading the file; the file's meta is the truth.
pub fn thread_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    // rollout-2026-08-26T18-30-09-<uuid>: the uuid is the last 36 chars.
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    (id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
        .then(|| id.to_string())
}

/// Read a rollout's own `session_meta`: the first line, when it is one.
pub fn read_meta(path: &Path) -> Option<SessionMeta> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first).ok()?;
    match parse_line(&first)?.payload {
        Payload::SessionMeta(m) => Some(*m),
        _ => None,
    }
}

/// Every rollout file under `sessions`, in path order. Walks the
/// `YYYY/MM/DD` tree and nothing else.
pub fn all_rollouts(sessions: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut days = Vec::new();
    walk_days(sessions, 0, &mut days);
    days.sort();
    for day in days {
        let Ok(entries) = std::fs::read_dir(&day) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_rollout_file(p))
            .collect();
        files.sort();
        out.extend(files);
    }
    out
}

/// The day directories at depth three (`YYYY/MM/DD`) under `root`.
fn walk_days(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if depth == 2 {
            out.push(path);
        } else {
            walk_days(&path, depth + 1, out);
        }
    }
}

/// The day directory a rollout sits in.
pub fn day_of(rollout: &Path) -> Option<&Path> {
    rollout.parent()
}

/// Rollout files in `root`'s day and every later day under the same
/// sessions tree: where that session's children can be.
pub fn rollouts_from(root: &Path) -> Vec<PathBuf> {
    let Some(day) = day_of(root) else {
        return Vec::new();
    };
    // sessions/YYYY/MM/DD → sessions
    let Some(sessions) = day.ancestors().nth(3) else {
        return Vec::new();
    };
    all_rollouts(sessions)
        .into_iter()
        .filter(|p| day_of(p).is_some_and(|d| d >= day))
        .collect()
}

/// The rollouts that belong to the session whose root is `root`: the root
/// itself, then every file whose own meta names that root as its session.
pub fn session_rollouts(root: &Path) -> Vec<(PathBuf, SessionMeta)> {
    let Some(root_meta) = read_meta(root) else {
        return Vec::new();
    };
    let Some(root_id) = root_meta.id.clone() else {
        return Vec::new();
    };
    let mut out = vec![(root.to_path_buf(), root_meta)];
    for path in rollouts_from(root) {
        if path == root {
            continue;
        }
        let Some(meta) = read_meta(&path) else {
            continue;
        };
        if !meta.is_root() && meta.session_id.as_deref() == Some(root_id.as_str()) {
            out.push((path, meta));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The provider primitives (see `provider/mod.rs` and docs/DISCOVERY.md)
// ---------------------------------------------------------------------------

use std::time::SystemTime;

use crate::provider::{FileRole, Provider, ReadMode, Scope, SessionFile};

/// Every rollout under `~/.codex/sessions`, root and child alike. A scope
/// with `since` prunes whole day directories before it; a project scope
/// cannot prune, since the project is inside the file.
pub fn all_paths(scope: &Scope) -> Vec<PathBuf> {
    let Some(home) = codex_home() else {
        return Vec::new();
    };
    let mut out = all_rollouts(&sessions_dir(&home));
    if let Some(since) = scope.since {
        let day_floor = day_dir_floor(since);
        out.retain(|p| day_of(p).and_then(day_key).is_none_or(|d| d >= day_floor));
    }
    out
}

/// `(year, month, day)` of a `YYYY/MM/DD` day directory.
fn day_key(day: &Path) -> Option<(u32, u32, u32)> {
    let mut parts = day
        .iter()
        .rev()
        .take(3)
        .map(|s| s.to_str()?.parse::<u32>().ok());
    let d = parts.next()??;
    let m = parts.next()??;
    let y = parts.next()??;
    Some((y, m, d))
}

/// The day directory a time falls in. Codex names the directories by the
/// local date (a rollout written at 16:25 local sits under that local day,
/// and its file name carries the same local time), so the floor is local too.
fn day_dir_floor(t: SystemTime) -> (u32, u32, u32) {
    use chrono::Datelike;
    let dt: chrono::DateTime<chrono::Local> = t.into();
    (dt.year() as u32, dt.month(), dt.day())
}

/// What a rollout is, by its own first line.
pub fn session_file(path: &Path) -> Option<SessionFile> {
    if !is_rollout_file(path) {
        return None;
    }
    let meta = read_meta(path)?;
    from_meta(path, &meta, crate::provider::modified(path))
}

/// [`session_file`] given the first line already parsed: the browser's way,
/// where the page reads the head and hands it over.
pub fn classify_head(path: &Path, head: &str, modified: SystemTime) -> Option<SessionFile> {
    let first = head.lines().find(|l| !l.trim().is_empty())?;
    match parse_line(first)?.payload {
        Payload::SessionMeta(meta) => from_meta(path, &meta, modified),
        _ => None,
    }
}

fn from_meta(path: &Path, meta: &SessionMeta, modified: SystemTime) -> Option<SessionFile> {
    let id = meta.id.clone()?;
    let (session, role) = if meta.is_root() {
        (id, FileRole::Root)
    } else {
        let session = meta
            .session_id
            .clone()
            .or_else(|| meta.parent().map(str::to_owned))?;
        let parent = meta
            .parent()
            .map(str::to_owned)
            .unwrap_or_else(|| session.clone());
        (session, FileRole::Agent { parent })
    };
    Some(SessionFile {
        provider: Provider::Codex,
        path: path.to_path_buf(),
        session,
        role,
        read: ReadMode::Tail,
        project_key: meta.cwd.clone().unwrap_or_default(),
        modified,
    })
}

/// Where the rest of a file's session is. From a root: rollouts in its day
/// and every later one, where children land. From a child: every rollout in
/// the tree, since the root may be in an earlier day.
pub fn related_paths(file: &SessionFile) -> Vec<PathBuf> {
    let paths = match file.role {
        FileRole::Root => rollouts_from(&file.path),
        _ => file
            .path
            .parent()
            .and_then(|day| day.ancestors().nth(3))
            .map(all_rollouts)
            .unwrap_or_default(),
    };
    paths.into_iter().filter(|p| *p != file.path).collect()
}

/// Codex records the working directory as it is.
pub fn project_key(cwd: &Path) -> String {
    cwd.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_id_is_the_tail_of_the_stem() {
        let p = Path::new(
            "/x/sessions/2026/08/26/rollout-2026-08-26T18-30-09-01a03eb1-6b82-78e3-87e0-2578e88e80cf.jsonl",
        );
        assert!(is_rollout_file(p));
        assert_eq!(
            thread_id_from_path(p).as_deref(),
            Some("01a03eb1-6b82-78e3-87e0-2578e88e80cf")
        );
        assert!(!is_rollout_file(Path::new("/x/notes.jsonl")));
        assert_eq!(
            thread_id_from_path(Path::new("/x/rollout-short.jsonl")),
            None
        );
    }
}
