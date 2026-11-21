//! Where OpenCode keeps its sessions, and how to tell what one is.
//!
//! Everything is rows in one SQLite database,
//! `~/.local/share/opencode/opencode.db` (`OPENCODE_DB` overrides it), and a
//! session is a row of `session_v2`. A spawned agent is a session of its own
//! whose `parent_id` names the session that spawned it, so a session and its
//! subagents form a tree and the root is the topmost row.
//!
//! The rest of zoetrope names a session's files by path, and nothing here is a
//! file of its own. So a session is located by a **locator**: the database's
//! path, `#`, and the session id (`.../opencode.db#ses_abc`). It goes through
//! every `PathBuf` the core keeps (identity, `rescan`, `Target::Path`, a
//! newer-session switch) unchanged, and it is the one place the shape of it is
//! known. A bare path to the database is accepted too, and means its newest
//! session.
//!
//! Every file of a session is read by polling its row's messages
//! ([`ReadMode::Rows`]), so there is no byte offset to keep and nothing to
//! truncate: [`stream_for`] hands back a stream that remembers how far it got.

#[cfg(feature = "native")]
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::Stream;
use crate::provider::{FileRole, Scope, SessionFile};

#[cfg(feature = "native")]
use super::db;
#[cfg(feature = "native")]
use super::wire::SessionRow;
#[cfg(feature = "native")]
use crate::provider::{Provider, ReadMode};
#[cfg(feature = "native")]
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------
// The locator, and the database's path
// ---------------------------------------------------------------------------

/// The path that names one session of the database at `db`.
#[cfg(feature = "native")]
pub fn locator(db: &Path, session: &str) -> PathBuf {
    let mut s: OsString = db.as_os_str().to_owned();
    s.push("#");
    s.push(session);
    PathBuf::from(s)
}

/// The database and session a locator names. `None` for any other path. The
/// session id is the part after the last `#` and holds no separator, so a `#`
/// in a directory name is not mistaken for one.
pub fn split(path: &Path) -> Option<(PathBuf, String)> {
    let s = path.to_str()?;
    let (db, id) = s.rsplit_once('#')?;
    if db.is_empty() || id.is_empty() || id.contains(['/', '\\']) {
        return None;
    }
    Some((PathBuf::from(db), id.to_string()))
}

/// The database from the environment: `$OPENCODE_DB`, else
/// `opencode/opencode.db` under `$XDG_DATA_HOME`, else under `~/.local/share`.
#[cfg(feature = "native")]
pub fn db_path() -> Option<PathBuf> {
    db_path_from(
        std::env::var_os("OPENCODE_DB"),
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
    )
}

/// [`db_path`] over explicit values, so the rule is testable without
/// touching the process environment. An empty variable is an unset one.
#[cfg(feature = "native")]
pub fn db_path_from(
    db: Option<OsString>,
    xdg_data: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    let set = |v: Option<OsString>| v.filter(|s| !s.is_empty()).map(PathBuf::from);
    if let Some(db) = set(db) {
        return Some(db);
    }
    let data = set(xdg_data).or_else(|| set(home).map(|h| h.join(".local").join("share")))?;
    Some(data.join("opencode").join("opencode.db"))
}

/// OpenCode records the working directory as it is; a trailing separator is
/// not part of the name.
pub fn project_key(cwd: &Path) -> String {
    key_of(&cwd.to_string_lossy())
}

fn key_of(dir: &str) -> String {
    match dir.trim_end_matches('/') {
        "" if dir.starts_with('/') => "/".to_string(),
        trimmed => trimmed.to_string(),
    }
}

/// The node id the model knows a session by: the root is `main`.
pub(super) fn node_id(session: &str, root: &str) -> String {
    if session == root {
        crate::state::session::MAIN_ID.to_string()
    } else {
        session.to_string()
    }
}

/// A parser for one of a session's files. The database and the session come
/// from the locator, the root from the file's own session, and the spawner
/// from its role.
pub fn stream_for(file: &SessionFile) -> Stream {
    let (db, session) = split(&file.path).unwrap_or_default();
    let parent = match &file.role {
        FileRole::Agent { parent } => Some(node_id(parent, &file.session)),
        _ => None,
    };
    Stream::new(db, session, file.session.clone(), parent)
}

// ---------------------------------------------------------------------------
// Reading the database (native only)
// ---------------------------------------------------------------------------

/// How far up a `parent_id` chain is followed before the row reached is taken
/// as the root. Real trees are a few deep; the bound only has to outlast one
/// and stop a corrupt cycle.
#[cfg(feature = "native")]
const MAX_DEPTH: usize = 32;

#[cfg(feature = "native")]
fn at(ms: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

/// What one session row is, given the root of its tree.
#[cfg(feature = "native")]
fn file_of(db: &Path, row: &SessionRow, root: &SessionRow) -> SessionFile {
    SessionFile {
        provider: Provider::OpenCode,
        path: locator(db, &row.id),
        session: root.id.clone(),
        role: if row.id == root.id {
            FileRole::Root
        } else {
            FileRole::Agent {
                parent: row.parent_id.clone().unwrap_or_else(|| root.id.clone()),
            }
        },
        read: ReadMode::Rows,
        project_key: key_of(&root.directory),
        modified: at(row.time_updated),
    }
}

/// The topmost row above `row`. A parent that is gone (deleted, or never
/// there) makes the row that named it the root of what is left.
#[cfg(feature = "native")]
fn root_row(conn: &rusqlite::Connection, row: SessionRow) -> SessionRow {
    let mut row = row;
    for _ in 0..MAX_DEPTH {
        let Some(parent) = row.parent_id.as_deref().and_then(|p| db::session(conn, p)) else {
            break;
        };
        row = parent;
    }
    row
}

/// The id of the topmost row above `row` among `by_id`, followed in memory.
#[cfg(feature = "native")]
fn root_id<'a>(
    by_id: &std::collections::HashMap<&'a str, &'a SessionRow>,
    row: &'a SessionRow,
) -> &'a str {
    let mut cur = row;
    for _ in 0..MAX_DEPTH {
        match cur.parent_id.as_deref().and_then(|p| by_id.get(p)) {
            Some(parent) => cur = parent,
            None => break,
        }
    }
    cur.id.as_str()
}

/// Every session of `db` that `scope` admits, as files, trees together and the
/// tree touched most recently first, each root ahead of its agents.
#[cfg(feature = "native")]
pub fn list_in(db: &Path, scope: &Scope) -> Vec<SessionFile> {
    use std::collections::{BTreeMap, HashMap};

    let Some(conn) = db::open(db) else {
        return Vec::new();
    };
    let rows = db::sessions(&conn);
    let by_id: HashMap<&str, &SessionRow> = rows.iter().map(|r| (r.id.as_str(), r)).collect();

    // Group by root without a query per row: the listing is the whole history.
    let mut trees: BTreeMap<&str, Vec<&SessionRow>> = BTreeMap::new();
    for row in &rows {
        trees.entry(root_id(&by_id, row)).or_default().push(row);
    }

    let project = scope.project.as_deref().map(project_key);
    let mut admitted: Vec<(i64, &str, Vec<&SessionRow>)> = trees
        .into_iter()
        .filter_map(|(root_id, members)| {
            let root = by_id[root_id];
            let touched = members.iter().map(|r| r.time_updated).max()?;
            let ok = scope
                .id_prefix
                .as_deref()
                .is_none_or(|p| root.id.starts_with(p))
                && project
                    .as_deref()
                    .is_none_or(|k| key_of(&root.directory) == k)
                && scope.since.is_none_or(|t| at(touched) >= t);
            ok.then_some((touched, root_id, members))
        })
        .collect();
    admitted.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));

    let mut out = Vec::new();
    for (_, root_id, mut members) in admitted {
        let root = by_id[root_id];
        members.sort_by_key(|r| (r.id != root.id, r.time_created, r.id.clone()));
        out.extend(members.into_iter().map(|r| file_of(db, r, root)));
    }
    out
}

#[cfg(feature = "native")]
fn list(scope: &Scope) -> Vec<SessionFile> {
    db_path().map_or_else(Vec::new, |db| list_in(&db, scope))
}

/// Every session of the user's database that `scope` admits.
#[cfg(feature = "native")]
pub fn sessions(scope: &Scope) -> Vec<SessionFile> {
    list(scope)
}

/// Without the database layer there is nothing to list: the portable core
/// reads no disk, and a browser has no database to open.
#[cfg(not(feature = "native"))]
pub fn sessions(_scope: &Scope) -> Vec<SessionFile> {
    Vec::new()
}

/// A locator for every session [`sessions`] lists.
pub fn all_paths(scope: &Scope) -> Vec<PathBuf> {
    sessions(scope).into_iter().map(|f| f.path).collect()
}

/// What a locator is: which session, which role, which project. A bare path
/// to the database is its newest session.
#[cfg(feature = "native")]
pub fn session_file(path: &Path) -> Option<SessionFile> {
    if let Some((db, id)) = split(path) {
        let conn = db::open(&db)?;
        let row = db::session(&conn, &id)?;
        let root = root_row(&conn, row.clone());
        return Some(file_of(&db, &row, &root));
    }
    if path.is_file() && db::is_sqlite_file(path) {
        return list_in(path, &Scope::ALL)
            .into_iter()
            .find(|f| f.role == FileRole::Root);
    }
    None
}

#[cfg(not(feature = "native"))]
pub fn session_file(_path: &Path) -> Option<SessionFile> {
    None
}

/// Where the rest of a session is: its whole tree, root included, from any
/// one of its files. Exact, not an over-include: a tree is a query.
#[cfg(feature = "native")]
pub fn related_paths(file: &SessionFile) -> Vec<PathBuf> {
    let Some((db, _)) = split(&file.path) else {
        return Vec::new();
    };
    let Some(conn) = db::open(&db) else {
        return Vec::new();
    };
    db::descendants(&conn, &file.session)
        .into_iter()
        .map(|r| locator(&db, &r.id))
        .filter(|p| *p != file.path)
        .collect()
}

#[cfg(not(feature = "native"))]
pub fn related_paths(_file: &SessionFile) -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;

    #[test]
    fn a_locator_round_trips_and_other_paths_are_not_one() {
        let db = Path::new("/home/u/.local/share/opencode/opencode.db");
        let loc = locator(db, "ses_abc");
        assert_eq!(
            loc,
            Path::new("/home/u/.local/share/opencode/opencode.db#ses_abc")
        );
        assert_eq!(split(&loc), Some((db.to_path_buf(), "ses_abc".to_string())));
        assert_eq!(split(db), None);
        assert_eq!(split(Path::new("/x/opencode.db#")), None);
        assert_eq!(split(Path::new("#ses_abc")), None);
        // A `#` in a directory is not the separator.
        assert_eq!(split(Path::new("/x/a#b/opencode.db")), None);
        assert_eq!(
            split(Path::new("/x/a#b/opencode.db#ses_1")),
            Some((PathBuf::from("/x/a#b/opencode.db"), "ses_1".to_string()))
        );
    }

    #[test]
    fn the_database_is_found_from_the_environment_in_order() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            db_path_from(os("/tmp/x.db"), os("/xdg"), os("/home/u")),
            Some(PathBuf::from("/tmp/x.db"))
        );
        assert_eq!(
            db_path_from(None, os("/xdg"), os("/home/u")),
            Some(PathBuf::from("/xdg/opencode/opencode.db"))
        );
        assert_eq!(
            db_path_from(None, None, os("/home/u")),
            Some(PathBuf::from("/home/u/.local/share/opencode/opencode.db"))
        );
        // Empty is unset.
        assert_eq!(
            db_path_from(os(""), os(""), os("/home/u")),
            Some(PathBuf::from("/home/u/.local/share/opencode/opencode.db"))
        );
        assert_eq!(db_path_from(None, None, None), None);
    }

    #[test]
    fn a_project_is_its_directory_without_a_trailing_separator() {
        assert_eq!(project_key(Path::new("/home/u/shop/")), "/home/u/shop");
        assert_eq!(project_key(Path::new("/home/u/shop")), "/home/u/shop");
        assert_eq!(project_key(Path::new("/")), "/");
    }
}
