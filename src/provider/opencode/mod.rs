//! The OpenCode provider: session rows in, [`Fact`]s out.
//!
//! OpenCode (v2) keeps a session in a SQLite database, not a transcript, so
//! this is the one provider that is *polled* rather than read line by line
//! (see [`discovery`] for where things are, and `docs/DISCOVERY.md` for why
//! that is a second kind of feeder and not a wider [`SessionFile`]). The
//! contract on this side is the same: a [`Stream`] states what its records
//! contain and never concludes. What differs is the record. A transcript line
//! is written once; a message row is **rewritten** as it streams: an assistant
//! message grows part by part, and each tool part walks `running` → `completed`
//! under the same id. So a stream remembers what it has already stated about
//! each message and states only what is new, which is what keeps the facts
//! idempotent (ARCHITECTURE §1.1) when the same row is read again.
//!
//! What this module knows that the model must not:
//!
//! - **A session is a thread; a subagent is a session of its own.** The root
//!   is `main`, every other session its own id. The `subagent` tool is the one
//!   that spawns, and its part names the child (`state.metadata.sessionID`) once
//!   it exists. The *parent* states the child's birth, with the call that
//!   spawned it, as Codex's does; the child's own stream states its own
//!   activity. Calling `subagent` with a `sessionID` of its own **resumes** a
//!   child that exists, which is a tool call and no spawn.
//! - **`idle` is a turn boundary, not an agent's end.** Every session goes
//!   idle after each turn, and a subagent is resumable, so an `idle` row states
//!   nothing (compare Codex's `task_complete`). A finished subagent's end is
//!   the parent's `subagent` call returning, which the core already joins
//!   (§2.1), and it stays reversible when the child is resumed. Stating `Ended`
//!   here would pin a resumed agent `Done` while it works.
//! - **A tool's verdict is not always its status.** A `shell` call whose
//!   command exits non-zero (or dies by signal) is `completed`; the verdict is
//!   in `metadata`, as Codex's is in `exit_code`. A background shell's
//!   `completed` is a launch acknowledgement.
//! - **A part is stated once it is closed.** Text and reasoning grow while the
//!   model streams, so they are stated when a later part or the message's
//!   completion shows them finished, not on every poll. Tokens likewise wait
//!   for completion, because the first count the core sees under a `dedup` key
//!   is the one it keeps.
//! - **`synthetic`, `system`, `compaction` and the switch records** are
//!   context the runtime wrote around the thread, not the thread speaking.
//!   They state nothing.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use chrono::{DateTime, Utc};

#[cfg(feature = "native")]
mod db;
pub mod discovery;
pub mod wire;

use crate::fact::{AgentKind, Fact, FactKind, Outcome, Statement};
use crate::provider::summary::{short_path, truncate_summary};
use wire::{Message, Row, SessionRow, ToolState, ms};

/// One session's rows being read. Cloning one keeps its place.
#[derive(Debug, Clone, Default)]
pub struct Stream {
    /// Only [`poll`](Stream::poll) opens it, and that needs the native build.
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    db: PathBuf,
    /// This thread's session id.
    session: String,
    /// The root session's id: references to it are the model's `main`.
    root: String,
    /// The model's id for the thread that spawned this one. `None` for the root.
    parent: Option<String>,
    /// The model's id for this thread.
    owner: String,
    /// For relativising paths in summaries.
    cwd: Option<String>,
    /// Whether the agent has been stated from its session row.
    born: bool,
    title: Option<String>,
    /// The model last stated, so it is stated again only when it changes.
    model: Option<String>,
    /// The latest `time_updated` read. Rows at or after it are read again.
    cursor: i64,
    /// The rows read *at* the cursor, by fingerprint of their document, so a
    /// row read twice in the same millisecond is stated once and one rewritten
    /// within it is stated again.
    boundary: HashMap<String, u64>,
    /// What has been stated about each message, so a rewrite states the rest.
    seen: HashMap<String, Seen>,
}

/// What has been stated about one message.
#[derive(Debug, Clone, Default)]
struct Seen {
    started: bool,
    tokens: bool,
    parts: Vec<PartMem>,
}

#[derive(Debug, Clone, Default)]
struct PartMem {
    stage: Stage,
    /// The child a `subagent` part names has been stated.
    linked: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Stage {
    #[default]
    Pending,
    /// A text or reasoning part, stated.
    Said,
    /// A tool part whose start is stated.
    Started,
    /// A tool part whose end is stated.
    Ended,
}

impl Stream {
    pub fn new(db: PathBuf, session: String, root: String, parent: Option<String>) -> Self {
        let owner = discovery::node_id(&session, &root);
        Stream {
            db,
            session,
            root,
            parent,
            owner,
            ..Stream::default()
        }
    }

    /// Read what changed since the last call and state it, in the order the
    /// session made it. Empty when nothing changed, or when the database cannot
    /// be read this tick (it is retried on the next).
    #[cfg(feature = "native")]
    pub fn poll(&mut self) -> Vec<Statement> {
        let Some(conn) = db::open(&self.db) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(row) = db::session(&conn, &self.session) {
            out.extend(self.session_row(&row));
        }
        for row in db::messages_since(&conn, &self.session, self.cursor) {
            out.extend(self.push(&row));
        }
        out
    }

    /// Without the database layer there is nothing to read.
    #[cfg(not(feature = "native"))]
    pub fn poll(&mut self) -> Vec<Statement> {
        Vec::new()
    }

    fn is_root(&self) -> bool {
        self.session == self.root
    }

    /// State the agent from its session row, once, and the title whenever it
    /// changes (the session names itself after its first exchange).
    pub fn session_row(&mut self, row: &SessionRow) -> Vec<Statement> {
        let mut out = Vec::new();
        if !self.born {
            self.born = true;
            self.cwd = Some(row.directory.clone());
            let at = ms(Some(row.time_created));
            let mut facts = vec![Fact {
                agent: Some(self.owner.clone()),
                ts: at,
                kind: if self.is_root() {
                    FactKind::Agent {
                        kind: AgentKind::Main,
                        parent: None,
                        agent_type: Some("opencode".into()),
                        description: None,
                        spawned_by: None,
                        interactive: true,
                    }
                } else {
                    FactKind::Agent {
                        kind: AgentKind::Subagent,
                        parent: self.parent.clone(),
                        // The parent's call states the agent it asked for when
                        // the row does not say.
                        agent_type: row.agent.clone(),
                        description: None,
                        spawned_by: None,
                        interactive: false,
                    }
                },
            }];
            if self.is_root() {
                let session = |label: &str, value: &str| Fact {
                    agent: None,
                    ts: None,
                    kind: FactKind::Session {
                        label: label.into(),
                        value: value.into(),
                    },
                };
                facts.push(session("version", &row.version));
                facts.push(session("cwd", &row.directory));
            }
            out.push(Statement { at, facts });
        }
        if self.is_root()
            && let Some(title) = row.title.as_deref().filter(|t| !t.trim().is_empty())
            && self.title.as_deref() != Some(title)
        {
            self.title = Some(title.to_string());
            out.push(Statement::from(Fact {
                agent: None,
                ts: None,
                kind: FactKind::Title(title.to_string()),
            }));
        }
        out
    }

    /// State what one message row says that has not been stated. A row read
    /// again unchanged states nothing.
    pub fn push(&mut self, row: &Row) -> Vec<Statement> {
        let print = fingerprint(&row.data);
        if row.time_updated == self.cursor && self.boundary.get(&row.id) == Some(&print) {
            return Vec::new();
        }
        if row.time_updated > self.cursor {
            self.cursor = row.time_updated;
            self.boundary.clear();
        }
        if row.time_updated == self.cursor {
            self.boundary.insert(row.id.clone(), print);
        }
        match row.kind.as_str() {
            "user" => self.user(row),
            "assistant" => self.assistant(row),
            // `idle` ends a turn, not an agent; the rest is the runtime
            // writing around the thread. See the module docs.
            _ => Vec::new(),
        }
    }

    /// A `user` message is written once. On the root thread it is what a
    /// person typed; on a subagent it is the spawner's instruction, which is
    /// not a person's prompt, so it is only activity.
    fn user(&mut self, row: &Row) -> Vec<Statement> {
        let seen = self.seen.entry(row.id.clone()).or_default();
        if std::mem::replace(&mut seen.started, true) {
            return Vec::new();
        }
        let Some(msg) = Message::parse(&row.data) else {
            return Vec::new();
        };
        let at = ms(msg.time.created.or(Some(row.time_updated)));
        let text = msg.text.unwrap_or_default();
        let kind = if self.session == self.root && !text.trim().is_empty() {
            FactKind::Prompt(text.trim_end().to_string())
        } else {
            FactKind::Activity
        };
        vec![Statement {
            at,
            facts: vec![Fact {
                agent: Some(self.owner.clone()),
                ts: at,
                kind,
            }],
        }]
    }

    /// An `assistant` message is rewritten as it streams: state the parts that
    /// have closed, the tools that have started or ended, and the message's
    /// tokens once it completes, each at its own time.
    fn assistant(&mut self, row: &Row) -> Vec<Statement> {
        let Some(msg) = Message::parse(&row.data) else {
            return Vec::new();
        };
        let owner = self.owner.clone();
        let cwd = self.cwd.clone();
        let seen = self.seen.entry(row.id.clone()).or_default();
        let complete = msg.is_complete();
        let by = |ts: Option<DateTime<Utc>>, kind: FactKind| Fact {
            agent: Some(owner.clone()),
            ts,
            kind,
        };
        let says = |ts: Option<DateTime<Utc>>, facts: Vec<Fact>| Statement { at: ts, facts };

        // Where the message is on the clock: it starts when the first token
        // arrives and moves to each tool's own times as they pass.
        let began = ms(msg.time.first_token.or(msg.time.created));
        let mut now = began;
        let mut out = Vec::new();

        // Every message names its model; state it when it is not the one
        // already stated, so a session on one model says it once.
        if let Some(model) = msg.model.as_ref().and_then(|m| m.id.as_deref())
            && self.model.as_deref() != Some(model)
        {
            self.model = Some(model.to_string());
            let at = ms(msg.time.created).or(began);
            out.push(says(at, vec![by(at, FactKind::Model(model.to_string()))]));
        }

        if seen.parts.len() < msg.content.len() {
            seen.parts.resize(msg.content.len(), PartMem::default());
        }
        for (i, part) in msg.content.iter().enumerate() {
            let mem = &mut seen.parts[i];
            match part.kind.as_str() {
                "text" | "reasoning" => {
                    // Still being written until something follows it.
                    let closed = complete || i + 1 < msg.content.len();
                    if closed && mem.stage == Stage::Pending {
                        mem.stage = Stage::Said;
                        let text = part.text.as_deref().unwrap_or_default().trim_end();
                        if !text.trim().is_empty() {
                            out.push(says(
                                now,
                                vec![by(now, FactKind::Reasoning(text.to_string()))],
                            ));
                        }
                    }
                }
                "tool" => {
                    let (Some(call), Some(state)) = (part.id.as_deref(), part.state.as_ref())
                    else {
                        continue;
                    };
                    let name = part.tool_name();
                    let started = ms(part.time.ran.or(part.time.created)).or(now);
                    // `subagent` with a session of its own is a resume.
                    let spawns = name == "subagent" && state.input_str("sessionID").is_none();

                    if mem.stage == Stage::Pending {
                        mem.stage = Stage::Started;
                        let mut facts = vec![by(
                            started,
                            FactKind::ToolStart {
                                call: call.to_string(),
                                name: name.to_string(),
                                summary: summarize(name, state, cwd.as_deref()),
                            },
                        )];
                        if spawns {
                            facts.push(by(
                                started,
                                FactKind::Spawn {
                                    call: call.to_string(),
                                },
                            ));
                        }
                        out.push(says(started, facts));
                        now = started;
                    }

                    // The child exists once the part names it, which may be
                    // after the call began.
                    if spawns
                        && !mem.linked
                        && let Some(child) = state.meta_str("sessionID")
                    {
                        mem.linked = true;
                        out.push(says(
                            started,
                            vec![
                                Fact {
                                    agent: Some(child.to_string()),
                                    ts: started,
                                    kind: FactKind::Agent {
                                        kind: AgentKind::Subagent,
                                        parent: Some(owner.clone()),
                                        agent_type: None,
                                        description: state
                                            .input_str("description")
                                            .map(truncate_summary),
                                        spawned_by: Some(call.to_string()),
                                        interactive: false,
                                    },
                                },
                                Fact {
                                    agent: Some(child.to_string()),
                                    ts: started,
                                    kind: FactKind::Label {
                                        agent_type: state.input_str("agent").map(str::to_string),
                                        description: None,
                                    },
                                },
                            ],
                        ));
                    }

                    if mem.stage == Stage::Started && state.finished() {
                        mem.stage = Stage::Ended;
                        let ended = ms(part.time.completed).or(now);
                        let failed = state.errored() || (name == "shell" && state.shell_failed());
                        let outcome = if failed { Outcome::Err } else { Outcome::Ok };
                        out.push(says(
                            ended,
                            vec![by(
                                ended,
                                FactKind::ToolEnd {
                                    call: call.to_string(),
                                    outcome,
                                },
                            )],
                        ));
                        now = ended;
                    }
                }
                _ => {}
            }
        }

        // Final once the message completes; the core keeps the first count it
        // sees under a key, so a partial one must never be stated.
        if complete
            && !seen.tokens
            && let Some(tokens) = &msg.tokens
        {
            seen.tokens = true;
            let n = tokens.generated();
            if n > 0 {
                let at = ms(msg.time.completed).or(now);
                out.push(says(
                    at,
                    vec![by(
                        at,
                        FactKind::Tokens {
                            output: n,
                            dedup: Some(row.id.clone()),
                        },
                    )],
                ));
            }
        }

        // A message that has begun is its owner working, even before it says
        // anything the model keeps.
        if !std::mem::replace(&mut seen.started, true) && out.is_empty() {
            out.push(says(began, vec![by(began, FactKind::Activity)]));
        }
        out
    }
}

/// A stable fingerprint of a document, to tell a rewrite from a re-read.
fn fingerprint(data: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// Tool summaries: the OpenCode tool vocabulary and what makes a good one-liner
// ---------------------------------------------------------------------------

/// One line for a tool call, from its input. The built-in tools are named;
/// anything else (an MCP or plugin tool) gets the first of the usual fields
/// it carries.
fn summarize(name: &str, state: &ToolState, cwd: Option<&str>) -> Option<String> {
    let text = |key: &str| state.input_str(key).map(truncate_summary);
    let path = |key: &str| state.input_str(key).map(|p| short_path(p, cwd));
    let named = match name {
        "shell" => text("command"),
        "read" | "edit" | "write" => path("path"),
        "patch" => state
            .input_str("patchText")
            .and_then(|p| patch_files(p, cwd)),
        "grep" | "glob" => text("pattern"),
        "webfetch" => text("url"),
        "websearch" => text("query"),
        "subagent" => text("description").or_else(|| text("agent")),
        "skill" => text("id"),
        "execute" => state
            .input_str("code")
            .and_then(|c| c.lines().find(|l| !l.trim().is_empty()))
            .map(truncate_summary),
        _ => None,
    };
    named.or_else(|| {
        [
            "command",
            "path",
            "pattern",
            "url",
            "query",
            "description",
            "id",
        ]
        .into_iter()
        .find_map(|k| if k == "path" { path(k) } else { text(k) })
    })
}

/// The files a patch touches, from its `*** Add/Update/Delete File:` lines:
/// the summary of a patch is where it lands, never the diff.
fn patch_files(patch: &str, cwd: Option<&str>) -> Option<String> {
    let files: Vec<String> = patch
        .lines()
        .filter_map(|l| {
            l.strip_prefix("*** Add File: ")
                .or_else(|| l.strip_prefix("*** Update File: "))
                .or_else(|| l.strip_prefix("*** Delete File: "))
        })
        .map(|p| short_path(p.trim(), cwd))
        .collect();
    (!files.is_empty()).then(|| truncate_summary(&files.join(", ")))
}

/// Fixture databases for the tests, here and in the feeders: the SQL under
/// `assets/opencode/` loaded into a temporary SQLite file, which is what
/// OpenCode's own is. Test-only.
#[cfg(all(test, feature = "native"))]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};

    /// A database holding `assets/opencode/<name>.sql`, or `None` where the
    /// checkout has no fixtures (the published crate ships only `src/`). Each
    /// call is its own file, removed by [`unload`].
    pub(crate) fn load(name: &str) -> Option<PathBuf> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = crate::provider::harness::fixture_dir("opencode")?;
        let sql = std::fs::read_to_string(dir.join(format!("{name}.sql"))).unwrap();
        let db = std::env::temp_dir().join(format!(
            "zoetrope_opencode_{name}_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&db);
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(&sql)
            .unwrap();
        Some(db)
    }

    pub(crate) fn unload(db: &Path) {
        let _ = std::fs::remove_file(db);
    }

    /// Run `sql` against the database as OpenCode would write to it.
    pub(crate) fn write(db: &Path, sql: &str) {
        rusqlite::Connection::open(db)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }
}

#[cfg(all(test, feature = "native"))]
mod database_tests {
    use super::testing::{load, unload, write};
    use super::*;
    use crate::provider::{FileRole, Provider, ReadMode, Scope, Target, open};

    const ROOT: &str = "ses_demo_root";

    fn session_of(db: &std::path::Path, id: &str) -> crate::provider::Session {
        open(&Target::Path(discovery::locator(db, id)), None).unwrap()
    }

    /// The demo database, discovered the way a live one is, run through the
    /// whole conformance check: order invariance, the folded model, the dated
    /// timeline.
    #[test]
    fn captures_conform() {
        let Some(dir) = crate::provider::harness::fixture_dir("opencode") else {
            return;
        };
        let mut fixtures: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension()? == "sql").then(|| p.file_stem()?.to_str().map(str::to_string))?
            })
            .collect();
        fixtures.sort();
        assert!(!fixtures.is_empty(), "no fixtures under {}", dir.display());
        for name in fixtures {
            let db = load(&name).unwrap();
            let path = db.clone();
            let streams = move || -> Vec<Vec<Statement>> {
                let session = session_of(&path, ROOT);
                session
                    .every_file()
                    .map(|f| session.provider.stream_for(f).poll())
                    .collect()
            };
            crate::provider::harness::conform("opencode", &name, streams);
            unload(&db);
        }
    }

    #[test]
    fn discovery_lists_the_tree_root_first() {
        let Some(db) = load("demo") else { return };
        let files = discovery::list_in(&db, &Scope::ALL);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].role, FileRole::Root);
        assert_eq!(files[0].session, ROOT);
        assert!(files.iter().all(|f| f.read == ReadMode::Rows));
        assert!(files.iter().all(|f| f.session == ROOT));
        assert!(files.iter().all(|f| f.project_key == "/home/dev/shop"));
        let agents: Vec<&FileRole> = files[1..].iter().map(|f| &f.role).collect();
        assert!(agents.iter().all(|r| **r
            == FileRole::Agent {
                parent: ROOT.into()
            }));

        // The scope prunes by project, id prefix and time.
        assert_eq!(
            discovery::list_in(&db, &Scope::project("/home/dev/shop/".as_ref())).len(),
            3
        );
        assert!(discovery::list_in(&db, &Scope::project("/elsewhere".as_ref())).is_empty());
        assert_eq!(discovery::list_in(&db, &Scope::id("ses_demo_r")).len(), 3);
        assert!(discovery::list_in(&db, &Scope::id("ses_other")).is_empty());
        let later = std::time::SystemTime::now();
        assert!(discovery::list_in(&db, &Scope::ALL.since(later)).is_empty());
        unload(&db);
    }

    /// Any file of a session opens the session, by locator or by the bare
    /// database, and a locator that names nothing is not found.
    #[test]
    fn a_session_opens_from_any_of_its_files() {
        let Some(db) = load("demo") else { return };
        let via_root = session_of(&db, ROOT);
        assert_eq!(via_root.provider, Provider::OpenCode);
        assert_eq!(via_root.id, ROOT);
        assert_eq!(via_root.files.len(), 2);

        let via_child = session_of(&db, "ses_demo_review");
        assert_eq!(via_child.id, ROOT);
        assert_eq!(via_child.root.path, via_root.root.path);

        let via_db = open(&Target::Path(db.clone()), None).unwrap();
        assert_eq!(via_db.id, ROOT, "a bare database is its newest session");

        let missing = discovery::locator(&db, "ses_nope");
        assert!(open(&Target::Path(missing), None).is_err());
        // Not a locator and not a file: nothing claims it.
        assert!(open(&Target::Path("/nowhere/at/all".into()), None).is_err());
        unload(&db);
    }

    #[test]
    fn a_stream_states_a_session_once_and_then_only_what_changed() {
        let Some(db) = load("demo") else { return };
        let session = session_of(&db, ROOT);
        let mut stream = session.provider.stream_for(&session.root);

        let first = stream.poll();
        let said = kinds(&first);
        assert_eq!(
            said.iter().filter(|k| **k == "Agent").count(),
            3,
            "root + two spawned"
        );
        assert_eq!(said.iter().filter(|k| **k == "ToolStart").count(), 6);
        assert!(stream.poll().is_empty(), "nothing changed, nothing stated");

        // OpenCode finishes the in-flight call, rewriting its row in place,
        // and the session carries on in a new one.
        write(
            &db,
            r#"UPDATE session_message SET time_updated = time_updated + 20000, data = json_set(
                   json_set(data, '$.time.completed', 1793527500000),
                   '$.content[0].state.status', 'completed')
               WHERE id = 'msg_r06';
               INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
               VALUES ('msg_r07', 'ses_demo_root', 'user', 7, 1793527600000, 1793527600000,
                       '{"time":{"created":1793527600000},"text":"Thanks, ship it."}');"#,
        );
        let next = stream.poll();
        assert_eq!(
            kinds(&next),
            ["ToolEnd", "Prompt"],
            "the rewrite ends the call; no second start"
        );
        assert!(stream.poll().is_empty());
        unload(&db);
    }

    /// A subagent that appears while the session runs is found by the
    /// per-tick rescan, with its parent.
    #[test]
    fn a_new_subagent_is_found_by_rescan() {
        let Some(db) = load("demo") else { return };
        let mut session = session_of(&db, ROOT);
        assert!(
            session.rescan().is_empty(),
            "nothing new on a settled session"
        );
        write(
            &db,
            r#"INSERT INTO session_v2 (id, project_id, parent_id, slug, directory, version, agent, time_created, time_updated)
               VALUES ('ses_demo_late', 'prj_demo', 'ses_demo_explore', 'late', '/home/dev/shop', '2.3.2', 'general', 1793527700000, 1793527700000);"#,
        );
        let found = session.rescan();
        assert_eq!(found.len(), 1, "a grandchild is part of the tree");
        assert_eq!(found[0].session, ROOT);
        assert_eq!(
            found[0].role,
            FileRole::Agent {
                parent: "ses_demo_explore".into()
            }
        );
        // Its stream names the spawner by the model's id for it.
        let said = session.provider.stream_for(&found[0]).poll();
        assert!(matches!(
            &said[0].facts[0].kind,
            FactKind::Agent { parent, .. } if parent.as_deref() == Some("ses_demo_explore")
        ));
        unload(&db);
    }

    /// The database is the user's, live, and holds their credentials: what we
    /// open it with must not be able to change it, whatever the SQL.
    #[test]
    fn the_connection_cannot_write() {
        let Some(db) = load("demo") else { return };
        let conn = db::open(&db).unwrap();
        for sql in [
            "DELETE FROM session_message",
            "UPDATE session_v2 SET title = 'x'",
            "CREATE TABLE intruder (x)",
        ] {
            assert!(conn.execute_batch(sql).is_err(), "`{sql}` must be refused");
        }
        assert_eq!(db::sessions(&conn).len(), 3, "and nothing was lost");
        unload(&db);
    }

    #[test]
    fn a_database_that_is_gone_states_nothing() {
        let gone = std::env::temp_dir().join("zoetrope_opencode_never_existed.db");
        let _ = std::fs::remove_file(&gone);
        assert!(stream_at(&gone).poll().is_empty());
        assert!(discovery::session_file(&discovery::locator(&gone, "ses_x")).is_none());
        assert!(discovery::list_in(&gone, &Scope::ALL).is_empty());
    }

    fn stream_at(db: &std::path::Path) -> Stream {
        Stream::new(db.to_path_buf(), "ses_x".into(), "ses_x".into(), None)
    }

    fn kinds(statements: &[Statement]) -> Vec<&'static str> {
        statements
            .iter()
            .flat_map(|s| s.facts.iter().map(|f| f.kind.name()))
            .filter(|k| {
                !matches!(
                    *k,
                    "Session"
                        | "Title"
                        | "Activity"
                        | "Model"
                        | "Tokens"
                        | "Reasoning"
                        | "Label"
                        | "Spawn"
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(session: &str, root: &str, parent: Option<&str>) -> Stream {
        Stream::new(
            PathBuf::new(),
            session.into(),
            root.into(),
            parent.map(str::to_string),
        )
    }

    fn row(id: &str, kind: &str, updated: i64, data: &str) -> Row {
        Row {
            id: id.into(),
            kind: kind.into(),
            time_updated: updated,
            data: data.into(),
        }
    }

    fn kinds(statements: &[Statement]) -> Vec<&'static str> {
        statements
            .iter()
            .flat_map(|s| s.facts.iter().map(|f| f.kind.name()))
            .collect()
    }

    fn session(parent: Option<&str>, agent: Option<&str>) -> SessionRow {
        SessionRow {
            id: "x".into(),
            parent_id: parent.map(str::to_string),
            directory: "/p".into(),
            title: Some("Fix it".into()),
            version: "2.3.2".into(),
            agent: agent.map(str::to_string),
            time_created: 1_000,
            time_updated: 2_000,
        }
    }

    #[test]
    fn the_root_is_main_with_its_rows_and_a_title() {
        let mut s = stream("ses_r", "ses_r", None);
        let said = s.session_row(&session(None, Some("build")));
        assert_eq!(said[0].facts[0].agent.as_deref(), Some("main"));
        assert!(matches!(
            &said[0].facts[0].kind,
            FactKind::Agent { kind: AgentKind::Main, interactive: true, agent_type, .. }
                if agent_type.as_deref() == Some("opencode")
        ));
        let labels: Vec<&str> = said[0]
            .facts
            .iter()
            .filter_map(|f| match &f.kind {
                FactKind::Session { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["version", "cwd"]);
        assert!(matches!(&said[1].facts[0].kind, FactKind::Title(t) if t == "Fix it"));
        // Stated once; the title again only when it changes.
        assert!(s.session_row(&session(None, Some("build"))).is_empty());
        let mut renamed = session(None, Some("build"));
        renamed.title = Some("Fix it properly".into());
        assert!(matches!(
            &s.session_row(&renamed)[0].facts[0].kind,
            FactKind::Title(t) if t == "Fix it properly"
        ));
    }

    #[test]
    fn a_child_session_is_its_own_agent_under_its_spawner() {
        let mut s = stream("ses_c", "ses_r", Some("main"));
        let said = s.session_row(&session(Some("ses_r"), Some("explore")));
        assert_eq!(said.len(), 1, "a child names no session rows or title");
        assert_eq!(said[0].facts[0].agent.as_deref(), Some("ses_c"));
        assert!(matches!(
            &said[0].facts[0].kind,
            FactKind::Agent { kind: AgentKind::Subagent, parent, agent_type, interactive: false, .. }
                if parent.as_deref() == Some("main") && agent_type.as_deref() == Some("explore")
        ));
    }

    #[test]
    fn a_tool_is_started_once_and_ended_when_the_row_says_so() {
        let mut s = stream("ses_r", "ses_r", None);
        let running = r#"{"time":{"created":1000,"firstToken":1100},"content":[
            {"type":"tool","id":"call_1","name":"shell",
             "state":{"status":"running","input":{"command":"cargo test"}},
             "time":{"created":1200,"ran":1210}}]}"#;
        let first = s.push(&row("m1", "assistant", 1210, running));
        assert_eq!(kinds(&first), ["ToolStart"]);
        assert!(matches!(
            &first[0].facts[0].kind,
            FactKind::ToolStart { call, name, summary }
                if call == "call_1" && name == "shell" && summary.as_deref() == Some("cargo test")
        ));
        // The same row again: nothing.
        assert!(s.push(&row("m1", "assistant", 1210, running)).is_empty());

        // Rewritten: the call finished, nonzero exit, message complete.
        let done = r#"{"time":{"created":1000,"firstToken":1100,"completed":3000},
            "tokens":{"output":40,"reasoning":2},"content":[
            {"type":"tool","id":"call_1","name":"shell",
             "state":{"status":"completed","input":{"command":"cargo test"},"metadata":{"exit":101}},
             "time":{"created":1200,"ran":1210,"completed":2900}}]}"#;
        let then = s.push(&row("m1", "assistant", 3000, done));
        assert_eq!(kinds(&then), ["ToolEnd", "Tokens"], "no second start");
        assert!(matches!(
            &then[0].facts[0].kind,
            FactKind::ToolEnd { call, outcome: Outcome::Err } if call == "call_1"
        ));
        assert!(
            matches!(&then[1].facts[0].kind, FactKind::Tokens { output: 42, dedup: Some(k) } if k == "m1")
        );
        assert_eq!(then[0].at, ms(Some(2900)), "the end sits at its own time");
        assert!(s.push(&row("m1", "assistant", 3000, done)).is_empty());
    }

    #[test]
    fn a_rewrite_in_the_same_millisecond_is_not_lost() {
        let mut s = stream("ses_r", "ses_r", None);
        let before = r#"{"time":{"created":1000},"content":[
            {"type":"tool","id":"c","name":"read","state":{"status":"running","input":{"path":"a"}},"time":{"created":1000}}]}"#;
        let after = r#"{"time":{"created":1000},"content":[
            {"type":"tool","id":"c","name":"read","state":{"status":"completed","input":{"path":"a"}},"time":{"created":1000,"completed":1000}}]}"#;
        assert_eq!(
            kinds(&s.push(&row("m", "assistant", 1000, before))),
            ["ToolStart"]
        );
        assert_eq!(
            kinds(&s.push(&row("m", "assistant", 1000, after))),
            ["ToolEnd"]
        );
    }

    #[test]
    fn text_is_stated_when_it_closes_and_tokens_when_the_message_does() {
        let mut s = stream("ses_r", "ses_r", None);
        let streaming = r#"{"time":{"created":1000,"firstToken":1100},"tokens":{"output":3},
            "content":[{"type":"text","text":"Looking"}]}"#;
        // Open: the model is still writing it. Only the message's activity.
        assert_eq!(
            kinds(&s.push(&row("m", "assistant", 1100, streaming))),
            ["Activity"]
        );
        let closed = r#"{"time":{"created":1000,"firstToken":1100,"completed":2000},"tokens":{"output":50},
            "content":[{"type":"reasoning","text":"plan"},{"type":"text","text":"Looking at it."}]}"#;
        let said = s.push(&row("m", "assistant", 2000, closed));
        assert_eq!(kinds(&said), ["Reasoning", "Reasoning", "Tokens"]);
        assert!(
            matches!(&said[2].facts[0].kind, FactKind::Tokens { output: 50, .. }),
            "the count at completion, never the partial one"
        );
    }

    #[test]
    fn a_subagent_call_spawns_a_child_and_a_resume_does_not() {
        let mut s = stream("ses_r", "ses_r", None);
        let spawn = r#"{"time":{"created":1000},"content":[
            {"type":"tool","id":"call_s","name":"subagent",
             "state":{"status":"running","input":{"agent":"explore","description":"Map the theme code","prompt":"x"},
                      "metadata":{"sessionID":"ses_c"}},
             "time":{"created":1100,"ran":1100}}]}"#;
        let said = s.push(&row("m1", "assistant", 1100, spawn));
        assert_eq!(kinds(&said), ["ToolStart", "Spawn", "Agent", "Label"]);
        let child = &said[1].facts;
        assert_eq!(child[0].agent.as_deref(), Some("ses_c"));
        assert!(matches!(
            &child[0].kind,
            FactKind::Agent { kind: AgentKind::Subagent, parent, description, spawned_by, agent_type, .. }
                if parent.as_deref() == Some("main")
                    && description.as_deref() == Some("Map the theme code")
                    && spawned_by.as_deref() == Some("call_s")
                    && agent_type.is_none()
        ));
        assert!(matches!(
            &child[1].kind,
            FactKind::Label { agent_type, .. } if agent_type.as_deref() == Some("explore")
        ));

        // Asking the same child to carry on is a tool call, not a spawn.
        let resume = r#"{"time":{"created":2000},"content":[
            {"type":"tool","id":"call_r","name":"subagent",
             "state":{"status":"running","input":{"sessionID":"ses_c","description":"more","prompt":"y"},
                      "metadata":{"sessionID":"ses_c"}},
             "time":{"created":2100,"ran":2100}}]}"#;
        assert_eq!(
            kinds(&s.push(&row("m2", "assistant", 2100, resume))),
            ["ToolStart"]
        );
    }

    #[test]
    fn a_prompt_is_the_roots_and_a_subagents_first_message_is_only_activity() {
        let said = |s: &mut Stream| {
            s.push(&row(
                "u",
                "user",
                1000,
                r#"{"time":{"created":1000},"text":"Do the thing\n"}"#,
            ))
        };
        let mut root = stream("ses_r", "ses_r", None);
        let root_said = said(&mut root);
        assert!(matches!(&root_said[0].facts[0].kind, FactKind::Prompt(t) if t == "Do the thing"));
        assert!(said(&mut root).is_empty(), "stated once");
        let mut child = stream("ses_c", "ses_r", Some("main"));
        assert_eq!(kinds(&said(&mut child)), ["Activity"]);
    }

    #[test]
    fn turn_boundaries_and_runtime_context_state_nothing() {
        let mut s = stream("ses_c", "ses_r", Some("main"));
        for (i, kind) in [
            "idle",
            "synthetic",
            "system",
            "compaction",
            "model-switched",
            "wat",
        ]
        .iter()
        .enumerate()
        {
            let at = 1000 + i as i64;
            assert!(
                s.push(&row(
                    &format!("m{i}"),
                    kind,
                    at,
                    r#"{"time":{"created":1000},"outcome":"succeeded","text":"x"}"#
                ))
                .is_empty(),
                "{kind} states nothing"
            );
        }
        assert!(
            s.push(&row("bad", "assistant", 5000, "not json"))
                .is_empty()
        );
    }

    #[test]
    fn summaries_name_what_a_tool_touched() {
        let state = |json: &str| serde_json::from_str::<ToolState>(json).unwrap();
        let cwd = Some("/p");
        assert_eq!(
            summarize("read", &state(r#"{"input":{"path":"/p/src/a.rs"}}"#), cwd).as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(
            summarize(
                "patch",
                &state(
                    r#"{"input":{"patchText":"*** Begin Patch\n*** Update File: /p/a.js\n@@\n-a\n+b\n*** Add File: /p/b.js\n+x\n*** End Patch"}}"#
                ),
                cwd
            )
            .as_deref(),
            Some("a.js, b.js")
        );
        assert_eq!(
            summarize(
                "grep",
                &state(r#"{"input":{"pattern":"fn main","path":"/p"}}"#),
                cwd
            )
            .as_deref(),
            Some("fn main")
        );
        assert_eq!(
            summarize(
                "execute",
                &state(r#"{"input":{"code":"\n// go\nrun()"}}"#),
                cwd
            )
            .as_deref(),
            Some("// go")
        );
        // A tool the provider does not know: the first usual field.
        assert_eq!(
            summarize(
                "router_verify",
                &state(r#"{"input":{"command":"just check"}}"#),
                cwd
            )
            .as_deref(),
            Some("just check")
        );
        assert_eq!(
            summarize("question", &state(r#"{"input":{"questions":[]}}"#), cwd),
            None
        );
    }
}
