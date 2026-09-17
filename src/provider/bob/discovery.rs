//! Where Bob keeps its sessions, and how to tell what a file is.
//!
//! Bob has no file per session: everything is rows in one SQLite database,
//! `~/.bob/db/bob.db`. So the layout questions the other providers answer
//! about paths are answered here about two things:
//!
//! - **The database.** A stored session is one [`SessionFile`] read as
//!   [`ReadMode::Rows`]: its `path` is the database, its `session` the task
//!   id, and the native row feeder (`db.rs`) reads it. Listing them is a
//!   query, not a directory walk, so it is the feeder's to do; `all_paths`
//!   has nothing to walk.
//! - **An export.** `zoe export` writes a session's rows as JSON lines, one
//!   file per session, root task first. That file is a tailed root like any
//!   other provider's, classified by its head, kept wherever the user put it.
//!   It is how a Bob session reaches the browser.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::wire::{TaskRow, is_root_head};
use crate::provider::{FileRole, Provider, ReadMode, Scope, SessionFile};

/// The Bob home: `~/.bob`. Only a feeder reads what is there.
#[cfg(feature = "native")]
pub fn bob_home() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".bob"))
}

/// The database under a Bob home.
#[cfg(feature = "native")]
pub fn db_path(home: &Path) -> PathBuf {
    home.join("db").join("bob.db")
}

/// Nothing to walk: exports are kept wherever the user wrote them, and
/// stored sessions are rows, listed by the row feeder.
pub fn all_paths(_scope: &Scope) -> Vec<PathBuf> {
    Vec::new()
}

/// What a file is, by its first line: an export whose head is a root task
/// row is that session's root. The database itself names no session and
/// classifies as nothing.
pub fn session_file(path: &Path) -> Option<SessionFile> {
    if !path.extension().is_some_and(|e| e == "jsonl") {
        return None;
    }
    let head = crate::provider::read_head(path)?;
    classify_head(path, &head, crate::provider::modified(path))
}

/// [`session_file`] given the first line already read: the browser's way.
pub fn classify_head(path: &Path, head: &str, modified: SystemTime) -> Option<SessionFile> {
    let first = head.lines().find(|l| !l.trim().is_empty())?;
    let row = is_root_head(first)?;
    Some(from_root(
        path.to_path_buf(),
        &row,
        ReadMode::Tail,
        modified,
    ))
}

/// A session's root file from its root task row: the export at `path`, or
/// the database when `read` is [`ReadMode::Rows`].
pub fn from_root(
    path: PathBuf,
    row: &TaskRow,
    read: ReadMode,
    modified: SystemTime,
) -> SessionFile {
    SessionFile {
        provider: Provider::Bob,
        path,
        session: row.id.clone(),
        role: FileRole::Root,
        read,
        project_key: row.workspace().unwrap_or_default(),
        modified,
    }
}

/// An export is one file, and a stored session is one set of rows: nothing
/// is near either.
pub fn related_paths(_file: &SessionFile) -> Vec<PathBuf> {
    Vec::new()
}

/// Bob records the workspace as it is.
pub fn project_key(cwd: &Path) -> String {
    cwd.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_export_is_classified_by_its_head() {
        let head = r#"{"table":"tasks","seen":"created","row":{"id":"abc","parent_id":null,"project_id":"file:/p","env":{"workspace":"/p"}}}"#;
        let f = classify_head(Path::new("/x/abc.jsonl"), head, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(f.provider, Provider::Bob);
        assert_eq!(f.session, "abc");
        assert_eq!(f.role, FileRole::Root);
        assert_eq!(f.read, ReadMode::Tail);
        assert_eq!(f.project_key, "/p");
        // A child's row, or a message, heads no export.
        assert!(
            classify_head(
                Path::new("/x/c.jsonl"),
                r#"{"table":"tasks","row":{"id":"c","parent_id":"abc"}}"#,
                SystemTime::UNIX_EPOCH
            )
            .is_none()
        );
        assert!(related_paths(&f).is_empty());
        assert!(all_paths(&Scope::ALL).is_empty());
    }
}
