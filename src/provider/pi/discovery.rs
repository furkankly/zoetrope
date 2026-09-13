//! Where omp and pi keep a session on disk, and how to tell what a file is.
//!
//! Both write `$<agent-dir>/sessions/<project-key>/<ISO-ts>_<uuid>.jsonl` for
//! a root session. omp additionally writes, beside the root, a directory of
//! the same stem holding one sibling `.jsonl` per spawned child (see
//! `super`): `<project-key>/<ISO-ts>_<uuid>/<Name>.jsonl`. That sibling
//! directory also holds `<n>.bash.log` (raw shell output) and `<Name>.md`
//! (a written report) — neither a transcript, both skipped by extension.
//!
//! `project_key` is the one real fork between the two: both replace every
//! path separator with `-`, but omp additionally shortens a `cwd` under
//! `$HOME` or the system temp dir, and pi always wraps the whole path in
//! `--`. Reverse-engineered from the installed binaries on this box —
//! `omp` 0.2.0 (`~/.bun/bin/omp`, functions `Avs`/`Mvs`/`Ult` in its bundle)
//! and `@earendil-works/pi-coding-agent` 0.84.4
//! (`jsonlSessionDirectoryName` in `dist/bundle/chunks/chunk-OMWWHBTG.js`,
//! plain unminified source) — and checked against every real project
//! directory under `~/.omp/agent/sessions` and `~/.pi/agent/sessions` on
//! this box. See the doc comments on [`omp_project_key`] and
//! [`pi_project_key`] for the worked examples.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::provider::{FileRole, Provider, ReadMode, Scope, SessionFile};

// ---------------------------------------------------------------------------
// Roots
// ---------------------------------------------------------------------------

/// omp/pi's session-storage root, reverse-engineered from both installed
/// binaries: `$PI_CODING_AGENT_SESSION_DIR` names the sessions directory
/// directly; failing that, `$PI_CODING_AGENT_DIR` names its parent
/// (`<agent-dir>`, `sessions` appended); failing that, `$HOME/.omp/agent` or
/// `$HOME/.pi/agent`. Both env vars are literally the same string in both
/// binaries — the framework underneath both apps only ever reads
/// `PI_CODING_AGENT_DIR`/`PI_CODING_AGENT_SESSION_DIR`, and the `omp` binary
/// aliases any `OMP_`-prefixed variable to its `PI_`-prefixed counterpart
/// for its own users (confirmed in `~/.bun/bin/omp`'s own strings), so an
/// override set either way is respected by both providers, like `$CODEX_HOME`
/// is respected by Codex alone.
fn sessions_dir(provider: Provider) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_SESSION_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR") {
        return Some(PathBuf::from(dir).join("sessions"));
    }
    let home = std::env::var_os("HOME")?;
    let dot = match provider {
        Provider::Omp => ".omp",
        Provider::Pi => ".pi",
        Provider::Claude | Provider::Codex => {
            unreachable!("pi::discovery only ever serves Provider::Omp/Pi")
        }
    };
    Some(PathBuf::from(home).join(dot).join("agent").join("sessions"))
}

// ---------------------------------------------------------------------------
// project_key
// ---------------------------------------------------------------------------

/// omp: `cwd` relative to `$HOME` when it is `$HOME` or a descendant
/// (prefixed `-`), else relative to the system temp dir when it is that or
/// a descendant (prefixed `-tmp`), else the whole path wrapped like pi's
/// (omp's own fallback for a project outside both). Worked examples, all
/// verified against real directories on this box:
///
/// | `cwd` | key |
/// |---|---|
/// | `/tmp` | `-tmp` |
/// | `/tmp/bq-harness-wt-jwd0e29d/wt` | `-tmp-bq-harness-wt-jwd0e29d-wt` |
/// | `/home/jorge/beequa-plugins` | `-beequa-plugins` |
/// | `/home/jorge` (home itself) | `-` |
pub fn omp_project_key(cwd: &Path) -> String {
    if let Some(home) = home_dir()
        && let Some(rel) = relative_descendant(cwd, &home)
    {
        return join_under("-", &rel);
    }
    if let Some(rel) = relative_descendant(cwd, &std::env::temp_dir()) {
        return join_under("-tmp", &rel);
    }
    wrap_absolute(cwd)
}

/// pi: the whole path, with its leading separator stripped and every
/// remaining separator turned into `-`, wrapped in `--`. No home/tmp
/// shortcut. Worked examples, all verified against real directories on this
/// box:
///
/// | `cwd` | key |
/// |---|---|
/// | `/tmp` | `--tmp--` |
/// | `/tmp/bq-harness-wt-bqjcpcn9/wt` | `--tmp-bq-harness-wt-bqjcpcn9-wt--` |
/// | `/home/jorge/beequa-pi-auth` | `--home-jorge-beequa-pi-auth--` |
pub fn pi_project_key(cwd: &Path) -> String {
    wrap_absolute(cwd)
}

/// This box's `$HOME`, resolved the same way both binaries do (`os.homedir()`
/// under the hood, i.e. `$HOME` on Unix).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `path`'s components after `base`, when `path` is `base` itself (empty
/// result) or a proper descendant of it. `None` for an unrelated path —
/// mirrors the source's own check (`path.relative(base, path)` not starting
/// with `..` and not absolute), without pulling in `..`-walking semantics
/// this provider's paths never need: `cwd` is always already absolute and
/// normalized here, like every other provider in this crate assumes.
fn relative_descendant(path: &Path, base: &Path) -> Option<PathBuf> {
    if path == base {
        return Some(PathBuf::new());
    }
    path.strip_prefix(base).ok().map(PathBuf::from)
}

/// omp's `Mvs`: join a relative path under a fixed prefix, turning every
/// separator in `rel` into `-`, never doubling the dash where `prefix`
/// already ends in one, and returning `prefix` unchanged for `rel == ""`
/// (`cwd` exactly at the shortcut root).
fn join_under(prefix: &str, rel: &Path) -> String {
    let dashed = path_to_dashed(rel);
    if dashed.is_empty() {
        prefix.to_string()
    } else if prefix.ends_with('-') {
        format!("{prefix}{dashed}")
    } else {
        format!("{prefix}-{dashed}")
    }
}

/// pi's (and omp's own out-of-home-and-tmp fallback) `Ult`: the whole path
/// with one leading separator stripped and every remaining separator turned
/// into `-`, wrapped in `--`.
fn wrap_absolute(path: &Path) -> String {
    let mut components = path.components();
    let stripped: PathBuf = if matches!(components.next(), Some(std::path::Component::RootDir)) {
        components.as_path().to_path_buf()
    } else {
        path.to_path_buf()
    };
    format!("--{}--", path_to_dashed(&stripped))
}

/// A relative path's components joined by `-` — the shared last step of
/// every encoding above.
fn path_to_dashed(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("-")
}

// ---------------------------------------------------------------------------
// What a path is
// ---------------------------------------------------------------------------

/// Whether `name` is shaped like a session's `<ISO-ts>_<uuid>` stem (e.g.
/// `2026-09-06T20-10-29-994Z_01a07858-...`) — the sibling-directory name a
/// spawned child's file sits under. Shape only, so [`classify_path`] never
/// touches the filesystem and doubles as `session_file_from` for a
/// filesystem-less caller (the browser build).
fn looks_like_session_stem(name: &str) -> bool {
    let Some((ts, uuid)) = name.split_once('_') else {
        return false;
    };
    ts.contains('T') && ts.ends_with('Z') && !uuid.is_empty()
}

/// What a path is, by where it sits in the project layout:
///
/// - `<key>/<ts>_<uuid>.jsonl` is the root;
/// - `<key>/<ts>_<uuid>/<name>.jsonl` is a spawned child (omp only — pi
///   never writes the sibling directory, so this arm never matches a real
///   pi path, but nothing here needs to assume that to stay correct).
pub fn session_file(provider: Provider, path: &Path) -> Option<SessionFile> {
    classify_path(provider, path, crate::provider::modified(path))
}

/// [`session_file`] without the filesystem beyond `modified` (which the
/// browser caller has no use for and passes the epoch for, like Claude's
/// `classify_path`).
pub fn classify_path(provider: Provider, path: &Path, modified: SystemTime) -> Option<SessionFile> {
    if path.extension().is_none_or(|e| e != "jsonl") {
        return None;
    }
    let parent = path.parent()?;
    let parent_name = parent.file_name()?.to_str()?;
    if looks_like_session_stem(parent_name) {
        let key = parent.parent()?.file_name()?.to_str()?.to_string();
        return Some(SessionFile {
            provider,
            path: path.to_path_buf(),
            session: parent_name.to_string(),
            role: FileRole::Agent {
                parent: parent_name.to_string(),
            },
            read: ReadMode::Tail,
            project_key: key,
            modified,
        });
    }
    let session = path.file_stem()?.to_str()?.to_string();
    let key = parent_name.to_string();
    Some(SessionFile {
        provider,
        path: path.to_path_buf(),
        session,
        role: FileRole::Root,
        read: ReadMode::Tail,
        project_key: key,
        modified,
    })
}

/// Every path that could be a root session file under `provider`'s sessions
/// root. Only roots, like Claude: a spawned child is found from its root via
/// [`related_paths`].
pub fn all_paths(provider: Provider, scope: &Scope) -> Vec<PathBuf> {
    let Some(root) = sessions_dir(provider) else {
        return Vec::new();
    };
    let dirs: Vec<PathBuf> = match &scope.project {
        // A project scope narrows to the one directory its key names —
        // `project_key` is exactly that directory's own name.
        Some(cwd) => {
            let key = match provider {
                Provider::Omp => omp_project_key(cwd),
                Provider::Pi => pi_project_key(cwd),
                Provider::Claude | Provider::Codex => {
                    unreachable!("pi::discovery only ever serves Provider::Omp/Pi")
                }
            };
            vec![root.join(key)]
        }
        None => std::fs::read_dir(&root)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
            .unwrap_or_default(),
    };
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            if let Some(since) = scope.since
                && crate::provider::modified(&path) < since
            {
                continue;
            }
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

/// Where the rest of a file's session is: the root beside the `<ts>_<uuid>`
/// directory, and every sibling `.jsonl` in it (a pi file, or an omp root
/// with no spawned children, has no such directory and so no siblings — an
/// empty result past the root itself is correct, not a miss).
pub fn related_paths(file: &SessionFile) -> Vec<PathBuf> {
    let session_dir = match &file.role {
        FileRole::Root => file.path.with_extension(""),
        FileRole::Agent { .. } => match file.path.parent() {
            Some(p) => p.to_path_buf(),
            None => return Vec::new(),
        },
        FileRole::Sidecar => return Vec::new(),
    };
    let root = session_dir.with_extension("jsonl");
    let mut out = vec![root];
    if let Ok(rd) = std::fs::read_dir(&session_dir) {
        let mut siblings: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        siblings.sort();
        out.extend(siblings);
    }
    out.retain(|p| p.is_file());
    out
}

/// A parser for one of this session's tailed files: the root (interactive,
/// labelled by which of the two providers wrote it) or a spawned child
/// (labelled by its own file stem — the only name either side of the spawn
/// agrees on; see `super`).
pub fn stream_for(file: &SessionFile) -> super::Stream {
    match &file.role {
        FileRole::Root => super::Stream::new_root(file.provider.name()),
        FileRole::Agent { .. } => {
            let name = file
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("agent")
                .to_string();
            super::Stream::new_child(name)
        }
        // Neither provider ever produces a Sidecar file; `related_paths`
        // only ever returns root/child `.jsonl` paths.
        FileRole::Sidecar => super::Stream::new_root(file.provider.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omp_project_key_matches_every_measured_directory() {
        // SAFETY (test-only, single-threaded within this process): pinning
        // HOME/TMPDIR is required to make the home/tmp branches
        // deterministic in CI regardless of the runner's own environment.
        unsafe {
            std::env::set_var("HOME", "/home/jorge");
            std::env::set_var("TMPDIR", "/tmp");
        }
        assert_eq!(omp_project_key(Path::new("/tmp")), "-tmp");
        assert_eq!(
            omp_project_key(Path::new("/tmp/bq-harness-wt-jwd0e29d/wt")),
            "-tmp-bq-harness-wt-jwd0e29d-wt"
        );
        assert_eq!(
            omp_project_key(Path::new("/home/jorge/beequa-plugins")),
            "-beequa-plugins"
        );
        assert_eq!(omp_project_key(Path::new("/home/jorge")), "-");
        // A scratchpad path containing a literal `-home-jorge` component is
        // not home-relativised: it is outside both $HOME and $TMPDIR once
        // nested under `/tmp/claude-1000/`, so only the plain slash->dash
        // fallback applies, doubling the dash where the component itself
        // starts with one.
        assert_eq!(
            omp_project_key(Path::new(
                "/tmp/claude-1000/-home-jorge/0b7cb65b-5cf3-4d90-b7e4-a680b6291f8c/scratchpad"
            )),
            "-tmp-claude-1000--home-jorge-0b7cb65b-5cf3-4d90-b7e4-a680b6291f8c-scratchpad"
        );
    }

    #[test]
    fn pi_project_key_matches_every_measured_directory() {
        assert_eq!(pi_project_key(Path::new("/tmp")), "--tmp--");
        assert_eq!(
            pi_project_key(Path::new("/tmp/bq-harness-wt-bqjcpcn9/wt")),
            "--tmp-bq-harness-wt-bqjcpcn9-wt--"
        );
        assert_eq!(
            pi_project_key(Path::new("/home/jorge/beequa-pi-auth")),
            "--home-jorge-beequa-pi-auth--"
        );
    }

    #[test]
    fn classify_path_tells_a_root_from_a_spawned_child() {
        // Root file stem carries the session id.
        let root = Path::new("/x/-tmp/2026-09-06T20-10-29-994Z_01a07858.jsonl");
        let child = Path::new("/x/-tmp/2026-09-06T20-10-29-994Z_01a07858/TickReviewer.jsonl");

        let r = classify_path(Provider::Omp, root, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(r.role, FileRole::Root);
        assert_eq!(r.session, "2026-09-06T20-10-29-994Z_01a07858");
        assert_eq!(r.project_key, "-tmp");

        let c = classify_path(Provider::Omp, child, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(
            c.role,
            FileRole::Agent {
                parent: "2026-09-06T20-10-29-994Z_01a07858".to_string()
            }
        );
        assert_eq!(c.session, "2026-09-06T20-10-29-994Z_01a07858");
        assert_eq!(c.project_key, "-tmp");
    }
}
