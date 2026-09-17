//! The row feeder's reader: Bob's SQLite database as lines. Native only.
//!
//! Strictly read-only. The database is opened with SQLite's read-only flag
//! and nothing here writes; Bob runs it in WAL mode, so a running Bob and
//! this reader do not block each other.
//!
//! What the database does that a reader must survive: it **rewrites a
//! session's message rows** when the session ends or is reopened, deleting
//! and re-inserting them, which reuses rowids, restamps every `created_at`
//! with the rewrite time, and can reorder rows. So nothing here keys on a
//! rowid or a `created_at`: a message is seen once, by its `id`, and a task
//! row is re-read as its `status`, `updated_at` or `costs` change. Rows land
//! whole, one per turn, once the turn's tool result is in; the model's
//! streaming never touches the database. That is an observation (a live
//! probe, and no row in the history that a later write changed), not a
//! guarantee: the change detector below rests on it, and a turn revised
//! in place after landing would go unseen.
//!
//! Every poll reads inside one transaction, so a write landing between the
//! task and message queries cannot show a turn before its task. The tree is
//! walked to any depth; `sessions` dates a root by its direct children only.
//!
//! A `subagent` child keeps no rows of its own: what it did is the
//! transcript its spawn's result carries. The reader presents that
//! transcript as the child's own turns, one line each, so they sit at their
//! own times rather than at the moment the result landed.
//!
//! A [`RowSource`] is one session's cursor over its rows: `poll` returns the
//! lines for what changed since the last call, in the shape `wire.rs` reads,
//! root task first. The first call returns the whole session.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags, Row, params};
use serde_json::{Value, json};

use super::discovery::from_root;
use super::wire::TaskRow;
use crate::provider::{ReadMode, Scope, SessionFile};

/// How long a query waits on a write in progress before giving up for this
/// tick. Bob's writes are short; the next tick retries anyway.
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// Open the database read-only.
pub fn open(db: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

const TASK_COLUMNS: &str = "id, parent_id, project_id, title, status, first_message, directory, \
     version, git_sha, git_branch, env, costs, created_at, updated_at, task_type, last_error";

/// Every stored session `scope` admits, newest first, as the root files the
/// core assembles sessions from. An absent or unreadable database is no
/// sessions.
pub fn sessions(db: &Path, scope: &Scope) -> Vec<SessionFile> {
    let Ok(conn) = open(db) else {
        return Vec::new();
    };
    let mut sql = format!(
        "SELECT {TASK_COLUMNS}, \
         (SELECT MAX(c.updated_at) FROM tasks c WHERE c.parent_id = t.id) AS last \
         FROM tasks t WHERE t.parent_id IS NULL"
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(prefix) = &scope.id_prefix {
        sql.push_str(" AND substr(t.id, 1, ?) = ?");
        args.push(Box::new(prefix.len() as i64));
        args.push(Box::new(prefix.clone()));
    }
    if let Some(since) = scope.since {
        sql.push_str(" AND t.updated_at >= ?");
        args.push(Box::new(to_millis(since)));
    }
    sql.push_str(" ORDER BY t.updated_at DESC");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
        let row = task_row(r)?;
        let last: Option<i64> = r.get("last")?;
        Ok((row, last))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(|(value, last)| {
            let row: TaskRow = serde_json::from_value(value).ok()?;
            let modified = row
                .updated_at
                .max(last)
                .map_or(SystemTime::UNIX_EPOCH, from_millis);
            Some(from_root(db.to_path_buf(), &row, ReadMode::Rows, modified))
        })
        .collect()
}

/// One session's cursor over its rows.
pub struct RowSource {
    db: PathBuf,
    task: String,
    conn: Option<Connection>,
    /// Messages already emitted, by id.
    seen_messages: HashSet<String>,
    /// Task rows already emitted, with the `(status, updated_at, costs)`
    /// they were last emitted at.
    tasks: HashMap<String, (String, i64, String)>,
    /// `(count, max created_at, max rowid)` of the session's messages at
    /// the last read: the cheap change detector that spares the full fetch.
    messages_mark: Option<(i64, i64, i64)>,
}

impl std::fmt::Debug for RowSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowSource")
            .field("db", &self.db)
            .field("task", &self.task)
            .field("seen_messages", &self.seen_messages.len())
            .field("tasks", &self.tasks.len())
            .finish()
    }
}

impl RowSource {
    pub fn new(db: &Path, task: &str) -> Self {
        RowSource {
            db: db.to_path_buf(),
            task: task.to_string(),
            conn: None,
            seen_messages: HashSet::new(),
            tasks: HashMap::new(),
            messages_mark: None,
        }
    }

    /// The session this reads.
    pub fn task(&self) -> &str {
        &self.task
    }

    /// The lines for what changed since the last call: task rows first
    /// (root, then children by creation), then unseen messages by their
    /// turn time. Empty when nothing changed, or the database cannot be
    /// read this tick (it is retried on the next).
    pub fn poll(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        match self.read(&mut out) {
            Ok(()) => out,
            Err(_) => {
                // Reopen next tick: a locked or vanished database.
                self.conn = None;
                out
            }
        }
    }

    fn read(&mut self, out: &mut Vec<String>) -> rusqlite::Result<()> {
        if self.conn.is_none() {
            self.conn = Some(open(&self.db)?);
        }
        let conn = self.conn.as_ref().expect("opened above");
        // One snapshot for both tables.
        let tx = conn.unchecked_transaction()?;

        // --- the task and every task under it ---
        let mut stmt = tx.prepare(&format!(
            "WITH RECURSIVE tree(id) AS (\
                 SELECT ?1 UNION SELECT t.id FROM tasks t JOIN tree ON t.parent_id = tree.id\
             ) \
             SELECT {TASK_COLUMNS} FROM tasks WHERE id IN (SELECT id FROM tree) \
             ORDER BY (parent_id IS NOT NULL), created_at, rowid"
        ))?;
        let tasks: Vec<Value> = stmt
            .query_map(params![self.task], task_row)?
            .collect::<Result<_, _>>()?;
        for row in tasks {
            let id = row["id"].as_str().unwrap_or_default().to_string();
            let mark = (
                row["status"].as_str().unwrap_or_default().to_string(),
                row["updated_at"].as_i64().unwrap_or_default(),
                row["costs"].to_string(),
            );
            match self.tasks.get(&id) {
                None => {
                    // A row first seen is two observations, not one: what
                    // it was created as, and what it has come to since. Both
                    // always, so a row that finished within the millisecond
                    // of its creation still states its ending.
                    out.push(line("tasks", Some("created"), &row));
                    out.push(line("tasks", Some("updated"), &row));
                    self.tasks.insert(id, mark);
                }
                Some(last) if *last != mark => {
                    out.push(line("tasks", Some("updated"), &row));
                    self.tasks.insert(id, mark);
                }
                Some(_) => {}
            }
        }

        // --- the messages, only when the detector moves ---
        const TREE: &str = "WITH RECURSIVE tree(id) AS (\
             SELECT ?1 UNION SELECT t.id FROM tasks t JOIN tree ON t.parent_id = tree.id)";
        let mark: (i64, i64, i64) = tx.query_row(
            &format!(
                "{TREE} SELECT COUNT(*), COALESCE(MAX(created_at), 0), COALESCE(MAX(rowid), 0) \
                 FROM messages WHERE task_id IN (SELECT id FROM tree)"
            ),
            params![self.task],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if self.messages_mark == Some(mark) {
            return Ok(());
        }
        let mut stmt = tx.prepare(&format!(
            "{TREE} SELECT id, task_id, role, created_at, data FROM messages \
             WHERE task_id IN (SELECT id FROM tree) \
             ORDER BY COALESCE(json_extract(data, '$._meta.timestamp'), created_at), rowid"
        ))?;
        let messages: Vec<Value> = stmt
            .query_map(params![self.task], message_row)?
            .collect::<Result<_, _>>()?;
        for mut row in messages {
            let id = row["id"].as_str().unwrap_or_default().to_string();
            if !self.seen_messages.insert(id) {
                continue;
            }
            // A spawn's result names its child and carries its transcript.
            // The transcript goes out as the child's own turns, each at its
            // own time and marked with the call it came `via`, so a replay
            // shows the child working when it worked; the result keeps its
            // verdict and the child's name. A result that names no child
            // keeps its transcript, for the stream's fallback join.
            lift(&mut row["data"], out);
            out.push(line("messages", None, &row));
        }
        self.messages_mark = Some(mark);
        Ok(())
    }
}

/// Lift the transcript a spawn result carries out of it: one line per turn,
/// as the named child's own, marked `via` the call. Recursive, since a
/// child's own spawn result carries its grandchild's transcript the same
/// way. A result that names no child is left as it is.
fn lift(data: &mut Value, out: &mut Vec<String>) {
    let child = data["_meta"]["subagentId"].as_str().map(str::to_string);
    let call = data["toolUsage"]["signature"]["id"]
        .as_str()
        .map(str::to_string);
    let (Some(child), Some(call)) = (child, call) else {
        return;
    };
    let Some(Value::Array(turns)) = data.as_object_mut().and_then(|d| d.remove("messages")) else {
        return;
    };
    for (i, mut turn) in turns.into_iter().enumerate() {
        lift(&mut turn, out);
        let id = turn["id"]
            .as_str()
            .map_or_else(|| format!("{call}#{i}"), str::to_string);
        out.push(
            json!({"table": "messages", "row": {
                "id": id, "task_id": child, "via": call,
                "role": turn["role"], "created_at": Value::Null, "data": turn,
            }})
            .to_string(),
        );
    }
}

/// One line of the wire format.
fn line(table: &str, seen: Option<&str>, row: &Value) -> String {
    let mut v = json!({ "table": table, "row": row });
    if let Some(seen) = seen {
        v["seen"] = Value::String(seen.into());
    }
    v.to_string()
}

/// A `tasks` row as JSON, its blob columns parsed.
fn task_row(r: &Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>("id")?,
        "parent_id": r.get::<_, Option<String>>("parent_id")?,
        "project_id": r.get::<_, Option<String>>("project_id")?,
        "title": r.get::<_, Option<String>>("title")?,
        "status": r.get::<_, Option<String>>("status")?,
        "first_message": r.get::<_, Option<String>>("first_message")?,
        "directory": r.get::<_, Option<String>>("directory")?,
        "version": r.get::<_, Option<String>>("version")?,
        "git_sha": r.get::<_, Option<String>>("git_sha")?,
        "git_branch": r.get::<_, Option<String>>("git_branch")?,
        "env": blob(r.get::<_, Option<String>>("env")?),
        "costs": blob(r.get::<_, Option<String>>("costs")?),
        "last_error": blob(r.get::<_, Option<String>>("last_error")?),
        "created_at": r.get::<_, Option<i64>>("created_at")?,
        "updated_at": r.get::<_, Option<i64>>("updated_at")?,
        "task_type": r.get::<_, Option<String>>("task_type")?,
    }))
}

/// A `messages` row as JSON, its turn parsed.
fn message_row(r: &Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>("id")?,
        "task_id": r.get::<_, String>("task_id")?,
        "role": r.get::<_, Option<String>>("role")?,
        "created_at": r.get::<_, Option<i64>>("created_at")?,
        "data": blob(r.get::<_, Option<String>>("data")?),
    }))
}

/// A JSON blob column, parsed; kept as the string when it does not parse.
fn blob(text: Option<String>) -> Value {
    match text {
        None => Value::Null,
        Some(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
    }
}

fn to_millis(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn from_millis(ms: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A database shaped like Bob's, with one session, one child and a few
    /// turns. The schema is the subset this module reads.
    pub(crate) fn fixture_db(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("zoetrope_bob_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bob.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, \
             title TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'active', \
             first_message TEXT, directory TEXT NOT NULL, version TEXT, git_sha TEXT, \
             git_branch TEXT, env TEXT, costs TEXT, created_at INTEGER NOT NULL, \
             updated_at INTEGER NOT NULL, task_type TEXT NOT NULL DEFAULT 'normal', last_error TEXT); \
             CREATE TABLE messages (id TEXT PRIMARY KEY, task_id TEXT NOT NULL, role TEXT NOT NULL, \
             data TEXT NOT NULL, created_at INTEGER NOT NULL); \
             INSERT INTO tasks VALUES ('root1', 'file:/p', NULL, 'do it', 'active', 'do it', '', NULL, NULL, NULL, \
               '{\"workspace\":\"/p\",\"modeId\":\"agent\"}', '{\"output\":100,\"cost\":0.5}', 1000, 5000, 'normal', 'null'); \
             INSERT INTO tasks VALUES ('child1', 'file:/p', 'root1', 'count', 'completed', NULL, '', NULL, NULL, NULL, \
               NULL, '{\"output\":40,\"cost\":0.1}', 2000, 3000, 'subagent', NULL); \
             INSERT INTO tasks VALUES ('other', 'file:/q', NULL, 'else', 'active', 'else', '', NULL, NULL, NULL, \
               '{\"workspace\":\"/q\"}', NULL, 500, 600, 'normal', NULL); \
             INSERT INTO messages VALUES ('u1', 'root1', 'user', '{\"role\":\"user\",\"content\":\"do it\",\"_meta\":{\"timestamp\":1001}}', 9000); \
             INSERT INTO messages VALUES ('a1', 'root1', 'assistant', '{\"role\":\"assistant\",\"content\":\"\",\"_meta\":{\"timestamp\":1500,\"spend\":{\"output\":9}},\"toolCalls\":[{\"id\":\"c1\",\"name\":\"spawn_subagent\",\"arguments\":{\"name\":\"general\",\"description\":\"count\"}}]}', 9000); \
             INSERT INTO messages VALUES ('t1', 'root1', 'tool', '{\"role\":\"tool\",\"content\":\"<task_result>2</task_result>\",\"toolUsage\":{\"signature\":{\"id\":\"c1\",\"name\":\"spawn_subagent\",\"isError\":false}},\"_meta\":{\"timestamp\":3001,\"subagentId\":\"child1\",\"agentType\":\"general\"},\"messages\":[{\"role\":\"user\",\"content\":\"count\",\"_meta\":{\"timestamp\":2001},\"id\":\"e1\"},{\"role\":\"tool\",\"content\":\"<task_result>x</task_result>\",\"toolUsage\":{\"signature\":{\"id\":\"c2\",\"name\":\"spawn_subagent\"}},\"_meta\":{\"timestamp\":2500,\"subagentId\":\"grandchild\"},\"messages\":[{\"role\":\"assistant\",\"content\":\"deep\",\"_meta\":{\"timestamp\":2400},\"id\":\"g1\"}],\"id\":\"e3\"},{\"role\":\"assistant\",\"content\":\"2\",\"_meta\":{\"timestamp\":2900},\"id\":\"e2\"}]}', 9000);",
        )
        .unwrap();
        path
    }

    #[test]
    fn sessions_lists_roots_newest_first_and_prunes_by_scope() {
        let db = fixture_db("sessions");
        let all = sessions(&db, &Scope::ALL);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].session, "root1");
        assert_eq!(all[0].read, ReadMode::Rows);
        assert_eq!(all[0].path, db);
        assert_eq!(all[0].project_key, "/p");
        assert_eq!(all[0].modified, from_millis(5000));
        assert_eq!(all[1].session, "other");
        assert_eq!(sessions(&db, &Scope::id("oth")).len(), 1);
        assert!(sessions(&db, &Scope::id("zzz")).is_empty());
        assert_eq!(sessions(&db, &Scope::ALL.since(from_millis(1000))).len(), 1);
        assert!(sessions(Path::new("/nonexistent/bob.db"), &Scope::ALL).is_empty());
    }

    #[test]
    fn poll_emits_the_session_once_then_only_changes() {
        let db = fixture_db("poll");
        let mut src = RowSource::new(&db, "root1");
        let first = src.poll();
        let tables: Vec<String> = first
            .iter()
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                format!(
                    "{}:{}:{}",
                    v["table"].as_str().unwrap(),
                    v["seen"].as_str().unwrap_or("-"),
                    v["row"]["id"].as_str().unwrap()
                )
            })
            .collect();
        // Root first, both observations of every row, then the turns by
        // their own time, a spawn result's transcript as the child's turns
        // just before it, a grandchild's likewise before the child's own
        // spawn result.
        assert_eq!(
            tables,
            [
                "tasks:created:root1",
                "tasks:updated:root1",
                "tasks:created:child1",
                "tasks:updated:child1",
                "messages:-:u1",
                "messages:-:a1",
                "messages:-:e1",
                "messages:-:g1",
                "messages:-:e3",
                "messages:-:e2",
                "messages:-:t1",
            ]
        );
        let deep: Value = serde_json::from_str(&first[7]).unwrap();
        assert_eq!(deep["row"]["task_id"], "grandchild");
        assert_eq!(deep["row"]["via"], "c2");
        // The blobs travel parsed.
        let root: Value = serde_json::from_str(&first[0]).unwrap();
        assert_eq!(root["row"]["env"]["workspace"], "/p");
        let turn: Value = serde_json::from_str(&first[6]).unwrap();
        assert_eq!(turn["row"]["task_id"], "child1");
        assert_eq!(turn["row"]["via"], "c1");
        assert_eq!(turn["row"]["data"]["_meta"]["timestamp"], 2001);
        let result: Value = serde_json::from_str(&first[10]).unwrap();
        assert!(result["row"]["data"]["messages"].is_null(), "un-nested");
        assert_eq!(result["row"]["data"]["_meta"]["subagentId"], "child1");
        assert!(src.poll().is_empty(), "nothing changed");

        // A rewrite: same message ids, new rowids and created_at. Nothing is
        // re-emitted. A new turn is.
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "DELETE FROM messages; \
             INSERT INTO messages VALUES ('u1', 'root1', 'user', '{\"role\":\"user\",\"content\":\"do it\",\"_meta\":{\"timestamp\":1001}}', 9900); \
             INSERT INTO messages VALUES ('a1', 'root1', 'assistant', '{\"role\":\"assistant\",\"content\":\"\",\"_meta\":{\"timestamp\":1500}}', 9900); \
             INSERT INTO messages VALUES ('t1', 'root1', 'tool', '{\"role\":\"tool\",\"content\":\"x\",\"_meta\":{\"timestamp\":3001}}', 9900); \
             INSERT INTO messages VALUES ('a2', 'root1', 'assistant', '{\"role\":\"assistant\",\"content\":\"done\",\"stop\":true,\"_meta\":{\"timestamp\":4000}}', 9900); \
             UPDATE tasks SET status = 'active', updated_at = 9900, costs = '{\"output\":120,\"cost\":0.6}' WHERE id = 'root1';",
        )
        .unwrap();
        let second = src.poll();
        assert_eq!(second.len(), 2);
        assert!(second[0].contains(r#""seen":"updated""#) && second[0].contains("root1"));
        assert!(second[1].contains(r#""id":"a2""#));
        assert!(src.poll().is_empty());
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }
}
