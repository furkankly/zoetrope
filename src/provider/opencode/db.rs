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
//! - **Only the tables a session is made of.** `session_v2`, `session_message`
//!   and `session_part`, and only the columns this module names. Nothing here
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

/// A session's row and its changed messages from **one snapshot**. The writer
/// is live, and OpenCode moves a message's inline parts into `session_part` and
/// clears its content as separate statements. Read the message in one query and
/// its parts in the next with a write between, and the old inline parts meet
/// their own new row copies: `[A, B]` becomes `[A, B, A, B]`. A read
/// transaction is one view of the database for every query inside it.
///
/// `None` is a read that failed, as for [`messages_since`].
pub fn read(conn: &Connection, session_id: &str, since: i64) -> Option<(SessionRow, Vec<Row>)> {
    // A deferred transaction on a read-only connection: it takes no write lock,
    // and starts its snapshot at the first query.
    let tx = conn.unchecked_transaction().ok()?;
    let row = session(&tx, session_id)?;
    let messages = messages_since(&tx, session_id, since)?;
    // Nothing was written; ending it is only letting the snapshot go.
    tx.commit().ok()?;
    Some((row, messages))
}

/// Whether this database has `session_part` at all. A database written only
/// by an older OpenCode may not, and a session read from one has no part rows.
fn has_part_table(conn: &Connection) -> bool {
    conn.prepare("SELECT 1 FROM session_part LIMIT 0").is_ok()
}

/// A session's messages written or rewritten at or after `since`
/// (milliseconds), in the order they were made, each with its part rows. A
/// message counts as rewritten when it *or any of its parts* was: with part
/// rows OpenCode updates a part and leaves the message alone, so the message's
/// own `time_updated` alone would miss a tool finishing. The bound is
/// inclusive on purpose: a row rewritten within the millisecond of the last
/// one read has the same time, and the caller tells it apart by content.
///
/// `None` is a read that failed (busy, gone, not a database), which is not
/// the same thing as nothing having changed.
pub fn messages_since(conn: &Connection, session: &str, since: i64) -> Option<Vec<Row>> {
    let with_parts = has_part_table(conn);
    let latest = if with_parts {
        "MAX(m.time_updated, COALESCE((SELECT MAX(p.time_updated) FROM session_part p \
         WHERE p.message_id = m.id), 0))"
    } else {
        "m.time_updated"
    };
    let mut stmt = conn
        .prepare(&format!(
            "SELECT m.id, m.type, {latest} AS updated, m.data FROM session_message m \
             WHERE m.session_id = ?1 AND updated >= ?2 ORDER BY m.seq"
        ))
        .ok()?;
    let mut rows: Vec<Row> = stmt
        .query_map(params![session, since], |r| {
            Ok(Row {
                id: r.get(0)?,
                kind: r.get(1)?,
                time_updated: r.get(2)?,
                data: r.get(3)?,
                parts: Vec::new(),
            })
        })
        .ok()?
        .collect::<Result<_, _>>()
        .ok()?;
    if with_parts {
        let mut parts = conn
            .prepare("SELECT data FROM session_part WHERE message_id = ?1 ORDER BY ordinal")
            .ok()?;
        for row in rows.iter_mut().filter(|r| r.kind == "assistant") {
            row.parts = parts
                .query_map(params![row.id], |r| r.get::<_, String>(0))
                .ok()?
                .collect::<Result<_, _>>()
                .ok()?;
        }
    }
    Some(rows)
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
