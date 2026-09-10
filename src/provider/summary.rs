//! One-line summaries of tool inputs, shared by every provider: what makes a
//! good one-liner is a property of the panel that shows it, not of any
//! format. Which field of which tool to summarize is each provider's own
//! lexicon.

/// Upper bound on a stored tool summary. Generous on purpose: the detail panel
/// truncates to its (often wide) width at render time, so this only caps
/// pathological inputs. The node cards don't render summaries, so it is NOT a
/// card-width constraint — capping tighter here just starved the panel.
const SUMMARY_MAX: usize = 200;

/// Collapse whitespace and truncate a summary to [`SUMMARY_MAX`].
pub(crate) fn truncate_summary(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = SUMMARY_MAX;
    if flat.chars().count() > MAX {
        let truncated: String = flat.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    } else {
        flat
    }
}

/// A file path, made readable for the panel: relative to `cwd` when it lives
/// under the project root, and truncated keeping the BASENAME (not the root) if
/// it's still long — `…/state/timeline.rs`, never `/Users/.../src/sta…`.
pub(crate) fn short_path(path: &str, cwd: Option<&str>) -> String {
    let rel = cwd
        .and_then(|c| path.strip_prefix(c).map(|r| (c, r)))
        // Only a match at a path-component boundary counts: without this a
        // SIBLING dir sharing the cwd as a string prefix is mangled
        // (cwd `…/zoetrope` + path `…/zoetrope-web/src/app.rs` → `-web/src/app.rs`).
        .filter(|(c, r)| r.starts_with('/') || c.ends_with('/'))
        .map(|(_, r)| r.trim_start_matches('/'))
        .filter(|r| !r.is_empty())
        .unwrap_or(path);
    const MAX: usize = SUMMARY_MAX;
    let n = rel.chars().count();
    if n <= MAX {
        return rel.to_string();
    }
    // Keep the tail (basename + nearest dirs) with a leading ellipsis.
    let tail: String = rel.chars().skip(n - (MAX - 1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_path_strips_cwd_only_at_a_component_boundary() {
        assert_eq!(
            short_path(
                "/Users/me/projects/zoetrope/src/a.rs",
                Some("/Users/me/projects/zoetrope")
            ),
            "src/a.rs"
        );
        // A SIBLING dir sharing the cwd as a string prefix must NOT be mangled
        // into a fake relative path ("-web/src/a.rs").
        assert_eq!(
            short_path(
                "/Users/me/projects/zoetrope-web/src/a.rs",
                Some("/Users/me/projects/zoetrope")
            ),
            "/Users/me/projects/zoetrope-web/src/a.rs"
        );
        assert_eq!(short_path("/project/x.rs", Some("/proj")), "/project/x.rs");
        // A trailing-slash cwd still relativizes.
        assert_eq!(short_path("/proj/x.rs", Some("/proj/")), "x.rs");
        // cwd == path falls back to the absolute path (not an empty string).
        assert_eq!(short_path("/proj", Some("/proj")), "/proj");
    }
}
