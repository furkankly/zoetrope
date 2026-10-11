//! Where OMP and Pi keep sessions, and how to tell what a file is.
//!
//! `<agent dir>/sessions/<project>/<timestamp>_<uuid>.jsonl` is a root. Its
//! spawned agents live under the sibling directory of the same stem, one
//! `<Name>.jsonl` per agent, nested again as `<Name>/<Name>.<Child>.jsonl`
//! for agents that spawn. The session id is the root's uuid; a file below a
//! `<timestamp>_<uuid>/` directory belongs to that session. `__*.jsonl`
//! (advisor transcripts) are nobody's thread.
//!
//! Roots, first that exists wins per entry, all kept: `$PI_CODING_AGENT_DIR`,
//! `$OMP_CODING_AGENT_DIR`, `~/.omp/agent`, `~/.pi/agent`, each plus
//! `sessions`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::wire;
use crate::provider::{FileRole, Provider, ReadMode, Scope, SessionFile};

/// Agent dirs this machine may keep sessions under.
pub fn agent_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for var in ["PI_CODING_AGENT_DIR", "OMP_CODING_AGENT_DIR"] {
        if let Some(p) = std::env::var_os(var).filter(|p| !p.is_empty()) {
            out.push(PathBuf::from(p));
        }
    }
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let home = PathBuf::from(home);
        out.push(home.join(".omp").join("agent"));
        out.push(home.join(".pi").join("agent"));
    }
    let mut seen = Vec::new();
    out.retain(|p| {
        let dup = seen.contains(p);
        seen.push(p.clone());
        !dup
    });
    out
}

/// `<ts>_<uuid>` → the uuid, when a stem has that shape.
pub fn root_uuid(stem: &str) -> Option<&str> {
    let (ts, id) = stem.split_once('_')?;
    let ok_ts = ts.len() >= 10 && ts.as_bytes()[0].is_ascii_digit() && ts.contains('T');
    let ok_id = id.len() >= 8 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    (ok_ts && ok_id).then_some(id)
}

fn stem(path: &Path) -> Option<&str> {
    path.file_stem()?.to_str()
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

/// The node id the model uses for the thread a session file records: `main`
/// for a root, else the file stem.
pub fn node_of_path(path: &str) -> String {
    match stem(Path::new(path)) {
        Some(s) if root_uuid(s).is_none() => s.to_string(),
        _ => crate::state::session::MAIN_ID.to_string(),
    }
}

/// What a path is by its name and place alone.
pub fn classify_path(path: &Path, modified: SystemTime) -> Option<SessionFile> {
    if !is_jsonl(path) {
        return None;
    }
    let name = stem(path)?;
    if name.starts_with("__") {
        return None;
    }
    let project = |dir: &Path| {
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string()
    };
    // The nearest ancestor directory named like a root decides the session.
    for dir in path.ancestors().skip(1) {
        if let Some(id) = dir.file_name().and_then(|n| n.to_str()).and_then(root_uuid) {
            let parent = match name.rsplit_once('.') {
                Some((p, _)) => p.to_string(),
                None => crate::state::session::MAIN_ID.to_string(),
            };
            return Some(SessionFile {
                provider: Provider::Omp,
                path: path.to_path_buf(),
                session: id.to_string(),
                role: FileRole::Agent { parent },
                read: ReadMode::Tail,
                project_key: dir.parent().map(project).unwrap_or_default(),
                modified,
            });
        }
    }
    let id = root_uuid(name)?;
    Some(SessionFile {
        provider: Provider::Omp,
        path: path.to_path_buf(),
        session: id.to_string(),
        role: FileRole::Root,
        read: ReadMode::Tail,
        project_key: path.parent().map(project).unwrap_or_default(),
        modified,
    })
}

/// A file whose name says nothing: its header says it is a root, with the
/// header's id as the session. The title line may come first.
pub fn classify_head(path: &Path, head: &str, modified: SystemTime) -> Option<SessionFile> {
    if let Some(f) = classify_path(path, modified) {
        return Some(f);
    }
    if !is_jsonl(path) || stem(path)?.starts_with("__") {
        return None;
    }
    let mut lines = head.lines().filter(|l| !l.trim().is_empty());
    let first = lines.next()?;
    if !wire::looks_like_omp(first) {
        return None;
    }
    let header = std::iter::once(first).chain(lines.take(3)).find_map(|l| {
        let r = wire::parse_line(l)?;
        (r.kind() == "session").then(|| {
            (
                r.str("id").map(str::to_owned),
                r.str("cwd").map(str::to_owned),
            )
        })
    })?;
    Some(SessionFile {
        provider: Provider::Omp,
        path: path.to_path_buf(),
        session: header.0?,
        role: FileRole::Root,
        read: ReadMode::Tail,
        project_key: header
            .1
            .map(|c| project_key(Path::new(&c)))
            .unwrap_or_default(),
        modified,
    })
}

/// [`classify_path`] with the filesystem: modified time, and the head for a
/// path whose name says nothing.
pub fn session_file(path: &Path) -> Option<SessionFile> {
    let modified = crate::provider::modified(path);
    if let Some(f) = classify_path(path, modified) {
        return Some(f);
    }
    classify_head(path, &read_head(path, 8192)?, modified)
}

fn read_head(path: &Path, max: u64) -> Option<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(max)
        .read_to_end(&mut buf)
        .ok()?;
    // A cut-off last line is harmless: it is only skipped by the parser.
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Root transcripts under every sessions dir: `<sessions>/<project>/*.jsonl`
/// of root shape. Children are found from their root.
pub fn all_paths(scope: &Scope) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in agent_dirs() {
        let Ok(projects) = std::fs::read_dir(dir.join("sessions")) else {
            continue;
        };
        for project in projects.flatten() {
            let pp = project.path();
            if !pp.is_dir() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(&pp) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                let Some(id) = is_jsonl(&p).then(|| stem(&p).and_then(root_uuid)).flatten() else {
                    continue;
                };
                if scope
                    .id_prefix
                    .as_deref()
                    .is_some_and(|x| !id.starts_with(x))
                {
                    continue;
                }
                if scope
                    .since
                    .is_some_and(|t| crate::provider::modified(&p) < t)
                {
                    continue;
                }
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Every `.jsonl` below a directory, any depth.
fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out, depth + 1);
        } else if is_jsonl(&p) {
            out.push(p);
        }
    }
}

/// The rest of a file's session: the root next to the directory, and every
/// transcript below it.
pub fn related_paths(file: &SessionFile) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let session_dir = match file.role {
        FileRole::Root => file.path.with_extension(""),
        _ => {
            let Some(dir) = file.path.ancestors().skip(1).find(|d| {
                d.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(root_uuid)
                    .is_some()
            }) else {
                return out;
            };
            out.push(dir.with_extension("jsonl"));
            dir.to_path_buf()
        }
    };
    walk(&session_dir, &mut out, 0);
    out.retain(|p| *p != file.path);
    out.sort();
    out
}

/// OMP names a project directory after its working directory: under the
/// home directory, `-<relative path>` with `/` → `-`
/// (`/Users/me/dev/x` → `-dev-x`); elsewhere `--<absolute path>--`
/// (`/private/tmp` → `--private-tmp--`).
pub fn project_key(cwd: &Path) -> String {
    let abs = cwd.to_string_lossy();
    let rel = std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .and_then(|h| abs.strip_prefix(h.trim_end_matches('/')).map(str::to_owned))
        .filter(|r| r.starts_with('/'));
    match rel {
        Some(r) => format!("-{}", r.trim_start_matches('/').replace('/', "-")),
        None => format!("--{}--", abs.trim_start_matches('/').replace('/', "-")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    const ROOT: &str =
        "/h/.omp/agent/sessions/-tmp/2026-10-11T01-42-10-041Z_01a1289f-e579-722c-bfe1-cded8884534d";

    #[test]
    fn classifies_root_child_and_nested_child() {
        let r = classify_path(Path::new(&format!("{ROOT}.jsonl")), UNIX_EPOCH).unwrap();
        assert_eq!(r.role, FileRole::Root);
        assert_eq!(r.session, "01a1289f-e579-722c-bfe1-cded8884534d");
        assert_eq!(r.project_key, "-tmp");

        let c = classify_path(Path::new(&format!("{ROOT}/Scout.jsonl")), UNIX_EPOCH).unwrap();
        assert_eq!(c.session, r.session);
        assert_eq!(
            c.role,
            FileRole::Agent {
                parent: "main".into()
            }
        );

        let n = classify_path(
            Path::new(&format!("{ROOT}/Scout/Scout.Inner.jsonl")),
            UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(
            n.role,
            FileRole::Agent {
                parent: "Scout".into()
            }
        );
        assert_eq!(n.session, r.session);

        assert!(classify_path(Path::new(&format!("{ROOT}/__advisor.jsonl")), UNIX_EPOCH).is_none());
        assert!(classify_path(Path::new(&format!("{ROOT}/x.log")), UNIX_EPOCH).is_none());
    }

    #[test]
    fn nameless_file_is_a_root_by_header_even_after_a_title() {
        let head = "{\"type\":\"title\",\"v\":1,\"title\":\"x\",\"source\":\"auto\"}\n{\"type\":\"session\",\"version\":3,\"id\":\"abc12345\",\"cwd\":\"/x\"}\n";
        let f = classify_head(Path::new("/z/some.jsonl"), head, UNIX_EPOCH).unwrap();
        assert_eq!((f.session.as_str(), f.role), ("abc12345", FileRole::Root));
        assert!(
            classify_head(
                Path::new("/z/some.jsonl"),
                "{\"type\":\"user\"}",
                UNIX_EPOCH
            )
            .is_none()
        );
    }

    #[test]
    fn parent_session_path_maps_to_a_node() {
        assert_eq!(node_of_path(&format!("{ROOT}.jsonl")), "main");
        assert_eq!(node_of_path(&format!("{ROOT}/Scout.jsonl")), "Scout");
    }

    #[test]
    fn project_key_matches_omp_directory_names() {
        // Outside the home directory: `--<path>--`.
        assert_eq!(project_key(Path::new("/private/tmp")), "--private-tmp--");
    }
}
