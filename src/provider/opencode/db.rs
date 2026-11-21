//! Reading OpenCode's database. Native only: SQLite cannot be built for
//! wasm32, and a browser has no disk to open.
//!
//! The file is not ours and not quiet. OpenCode's service writes it while we
//! read, in WAL mode, and the same file holds the user's credentials
//! (`account`, `credential`, `control_account`). Two rules follow, and both are
//! kept here and nowhere else:
//!
//! - **Read-only, and short.** Every connection is opened `READ_ONLY` with
//!   `query_only`, never creates or checkpoints anything, waits a few hundred
//!   milliseconds on a lock rather than the UI's patience, and lives for one
//!   poll. A failure to read is "nothing new this tick", the way an unreadable
//!   file is for a tailer.
//! - **Only the tables a session is made of.** `session_v2` and
//!   `session_message`, and only the columns this module names. Nothing here
//!   reads another table, so nothing here can leak what lives in it.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};

use super::wire::{Row, SessionRow};

/// How long a read waits on a writer before giving up until the next tick.
const BUSY: Duration = Duration::from_millis(250);

/// Open the database for reading, or `None` if it cannot be (absent, locked
/// past [`BUSY`], not a database).
pub fn open(path: &Path) -> Option<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.busy_timeout(BUSY).ok()?;
    conn.pragma_update(None, "query_only", true).ok()?;
    Some(conn)
}

const SESSION_COLUMNS: &str =
    "id, parent_id, directory, title, version, agent, time_created, time_updated";

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: r.get(0)?,
        parent_id: r.get(1)?,
        directory: r.get(2)?,
        title: r.get(3)?,
        version: r.get(4)?,
        agent: r.get(5)?,
        time_created: r.get(6)?,
        time_updated: r.get(7)?,
    })
}

/// One session.
pub fn session(conn: &Connection, id: &str) -> Option<SessionRow> {
    conn.query_row(
        &format!("SELECT {SESSION_COLUMNS} FROM session_v2 WHERE id = ?1"),
        params![id],
        session_row,
    )
    .ok()
}

/// Every session, newest write first. Metadata only: the listing of a long
/// history must stay cheap, so it carries no message and no document.
pub fn sessions(conn: &Connection) -> Vec<SessionRow> {
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT {SESSION_COLUMNS} FROM session_v2 ORDER BY time_updated DESC, id"
    )) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], session_row) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

/// A session and every session beneath it, by `parent_id`, oldest first. The
/// tree of one root is a recursive query, so a rescan every tick costs a
/// lookup rather than a listing of the history.
pub fn descendants(conn: &Connection, root: &str) -> Vec<SessionRow> {
    let Ok(mut stmt) = conn.prepare(&format!(
        "WITH RECURSIVE tree(id) AS ( \
             SELECT ?1 UNION SELECT s.id FROM session_v2 s JOIN tree t ON s.parent_id = t.id \
         ) \
         SELECT {SESSION_COLUMNS} FROM session_v2 \
         WHERE id IN (SELECT id FROM tree) ORDER BY time_created, id"
    )) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![root], session_row) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

/// A session's messages written or rewritten at or after `since`
/// (milliseconds), in the order they were made. The bound is inclusive on
/// purpose: a row rewritten within the millisecond of the last one read has
/// the same `time_updated`, and the caller tells it apart by content.
pub fn messages_since(conn: &Connection, session: &str, since: i64) -> Vec<Row> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, type, time_updated, data FROM session_message \
         WHERE session_id = ?1 AND time_updated >= ?2 ORDER BY seq",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![session, since], |r| {
        Ok(Row {
            id: r.get(0)?,
            kind: r.get(1)?,
            time_updated: r.get(2)?,
            data: r.get(3)?,
        })
    }) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

/// Whether a file starts with SQLite's magic, so a bare path to a database
/// can be told from any other file without opening it.
pub fn is_sqlite_file(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && &head == b"SQLite format 3\0"
}
