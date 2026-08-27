//! The session rail: every live session at a glance, and which one is focused.
//!
//! Deliberately **portable** — no filesystem types beyond `PathBuf`, no
//! discovery. The native side sweeps the disk ([`crate::sessions`]) and hands
//! finished rows across as a [`UiEvent`](crate::tailer::UiEvent), exactly the
//! way transcript batches arrive. The browser frontend simply never populates
//! it, and the rail renders as absent rather than as a special case.
//!
//! A row is built from a skeleton [`Summary`](crate::sessions::Summary), not
//! from a folded model: an unfocused session costs records, never bodies. The
//! canvas draws every live session; the rail is the index over them — including
//! the idle ones the canvas leaves out — and picks which one is focused.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

/// A session is "running" if its newest activity is within this window.
///
/// The same threshold the graph uses for interactive liveness
/// (`session::INTERACTIVE_IDLE_SECS`), so a rail row and the agent card for the
/// same session can never disagree about whether it is alive.
pub const RAIL_IDLE_SECS: i64 = 120;

/// Whether a session is currently producing output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailStatus {
    /// Wrote something within [`RAIL_IDLE_SECS`].
    Running,
    /// Quiet, but within the discovery window.
    Idle,
}

/// One session's row: everything the rail draws, and the path focusing it
/// watches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailRow {
    pub session_id: String,
    /// Display name for the session's project (see [`project_label`]).
    pub project: String,
    /// The main transcript — what a focus switch asks the tailer to watch.
    pub main_path: PathBuf,
    /// Agents in the session, including the main agent.
    pub agents: usize,
    pub tool_calls: usize,
    pub failures: usize,
    /// Prompts the index could vouch for. A lower bound on legacy transcripts
    /// (see `index::flags::AMBIGUOUS_PROMPT`); the rail shows a count, not a
    /// claim about the era spine, so a lower bound is honest enough here.
    pub prompts: usize,
    /// Newest timestamp across the session's files.
    pub last_activity: Option<DateTime<Utc>>,
    /// The newest bytes came from a subagent, not the main transcript — the
    /// main agent is blocked on a subagent, which is worth showing.
    pub sidecar_active: bool,
}

impl RailRow {
    /// Whether this session is running as of `now`.
    pub fn status(&self, now: DateTime<Utc>) -> RailStatus {
        match self.last_activity {
            Some(ts) if (now - ts).num_seconds() <= RAIL_IDLE_SECS => RailStatus::Running,
            _ => RailStatus::Idle,
        }
    }

    /// How long ago this session last wrote, as a compact label (`3s`, `12m`,
    /// `4h`, `2d`). `—` when undated.
    pub fn age(&self, now: DateTime<Utc>) -> String {
        let Some(ts) = self.last_activity else {
            return "—".to_string();
        };
        let secs = (now - ts).num_seconds().max(0);
        match secs {
            s if s < 60 => format!("{s}s"),
            s if s < 3600 => format!("{}m", s / 60),
            s if s < 86_400 => format!("{}h", s / 3600),
            s => format!("{}d", s / 86_400),
        }
    }
}

/// The rail's own state: the discovered rows and whether the pane is drawn.
#[derive(Debug, Clone, Default)]
pub struct SessionRail {
    /// Discovered sessions, newest activity first.
    pub rows: Vec<RailRow>,
    /// User override for rail visibility: `Some(true)`/`Some(false)` after an
    /// explicit toggle, `None` while it follows the workspace (see
    /// [`should_show`](Self::should_show)).
    pub forced_visible: Option<bool>,
    /// True once a sweep has landed — distinguishes "no sessions" from "not
    /// looked yet", which the empty state needs to say honestly.
    pub swept: bool,
}

impl SessionRail {
    /// Replace the rows from a fresh sweep.
    ///
    /// The rail holds NO focus of its own: the focused row is whichever row
    /// matches the session the app is actually watching
    /// (`App::current_session_id`, which `UiEvent::SessionReset` updates when a
    /// switch lands). A parallel focus field here could point at a session the
    /// tailer is not watching — a marker that lies. Deriving it means the
    /// marker moves when the switch actually takes effect, and a watched
    /// session that has aged out of the discovery window simply shows no
    /// marker, which is the truth.
    pub fn adopt(&mut self, rows: Vec<RailRow>) {
        self.rows = rows;
        self.swept = true;
    }

    /// Index of the row for `current`, if the rail lists it.
    pub fn focused_index(&self, current: &str) -> Option<usize> {
        self.rows.iter().position(|r| r.session_id == current)
    }

    /// The row for `current`, if the rail lists it.
    pub fn focused_row(&self, current: &str) -> Option<&RailRow> {
        self.rows.get(self.focused_index(current)?)
    }

    /// The transcript `delta` rows away from `current`, wrapping.
    ///
    /// `None` when there is nowhere else to go (fewer than two rows), so a
    /// keypress cannot cost a redundant session switch — a switch tears down
    /// and rebuilds the whole model. A `current` the rail does not list starts
    /// from the top rather than refusing, so an out-of-window session is never
    /// a dead end.
    pub fn step_focus(&self, current: &str, delta: isize) -> Option<PathBuf> {
        if self.rows.len() < 2 {
            return None;
        }
        let len = self.rows.len() as isize;
        let next = match self.focused_index(current) {
            Some(i) => (i as isize + delta).rem_euclid(len) as usize,
            // Not listed: step in from whichever end the user was heading for.
            None if delta < 0 => self.rows.len() - 1,
            None => 0,
        };
        self.watch_target(current, next)
    }

    /// The transcript for row `index` (a rail click), or `None` when that row
    /// is already the watched session.
    pub fn focus_index(&self, current: &str, index: usize) -> Option<PathBuf> {
        self.watch_target(current, index)
    }

    /// The main transcript of row `index`, unless it is already being watched.
    fn watch_target(&self, current: &str, index: usize) -> Option<PathBuf> {
        let row = self.rows.get(index)?;
        (row.session_id != current).then(|| row.main_path.clone())
    }

    /// How many rows are running as of `now`.
    pub fn running(&self, now: DateTime<Utc>) -> usize {
        self.rows
            .iter()
            .filter(|r| r.status(now) == RailStatus::Running)
            .count()
    }

    /// Whether to draw the rail.
    ///
    /// Off by default when there is nothing to choose between: a lone session
    /// is what `zoe` has always shown, and spending 30 columns to say so would
    /// be a regression for the common case. It appears on its own as soon as a
    /// second session exists, and an explicit toggle overrides either way.
    pub fn should_show(&self) -> bool {
        self.forced_visible.unwrap_or(self.rows.len() >= 2)
    }

    /// Toggle rail visibility, taking over from the automatic rule.
    pub fn toggle_visible(&mut self) {
        self.forced_visible = Some(!self.should_show());
    }
}

/// Fallback project name, derived from a `~/.claude/projects` directory name.
///
/// **Only for sessions whose transcript records no `cwd`.** Those directory
/// names are a cwd with every non-alphanumeric byte replaced by `-`
/// (`transcript::sanitize_cwd`), which is not invertible: `parametric_pump`
/// sanitizes to `parametric-pump`, whose trailing segment is the useless
/// `pump`. Guessing where the project name starts is not possible from the
/// string alone, so the native row builder reads the real path out of the
/// transcript's `cwd` envelope field instead (one parsed line per session) and
/// only falls back here when there is none.
pub fn project_label(dir_name: &str) -> String {
    let trimmed = dir_name.trim_matches('-');
    match trimmed.rsplit('-').find(|s| !s.is_empty()) {
        Some(last) => last.to_string(),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, project: &str, ago_secs: i64, now: DateTime<Utc>) -> RailRow {
        RailRow {
            session_id: id.to_string(),
            project: project.to_string(),
            main_path: PathBuf::from(format!("/p/{id}.jsonl")),
            agents: 1,
            tool_calls: 0,
            failures: 0,
            prompts: 0,
            last_activity: Some(now - chrono::Duration::seconds(ago_secs)),
            sidecar_active: false,
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-08-26T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn status_and_age_follow_the_graphs_idle_threshold() {
        let n = now();
        assert_eq!(row("a", "p", 5, n).status(n), RailStatus::Running);
        assert_eq!(
            row("a", "p", RAIL_IDLE_SECS, n).status(n),
            RailStatus::Running,
            "the boundary is inclusive, as it is for agent cards"
        );
        assert_eq!(
            row("a", "p", RAIL_IDLE_SECS + 1, n).status(n),
            RailStatus::Idle
        );

        assert_eq!(row("a", "p", 5, n).age(n), "5s");
        assert_eq!(row("a", "p", 300, n).age(n), "5m");
        assert_eq!(row("a", "p", 7200, n).age(n), "2h");
        assert_eq!(row("a", "p", 172_800, n).age(n), "2d");
    }

    #[test]
    fn the_marker_follows_the_watched_session_across_a_reorder() {
        let n = now();
        let mut rail = SessionRail::default();
        rail.adopt(vec![row("a", "alpha", 5, n), row("b", "beta", 60, n)]);
        assert_eq!(rail.focused_index("a"), Some(0));

        // A sweep reorders by recency; the marker tracks the SESSION, so it
        // moves with the row rather than staying at position 0.
        rail.adopt(vec![row("b", "beta", 1, n), row("a", "alpha", 90, n)]);
        assert_eq!(rail.focused_index("a"), Some(1));
        assert_eq!(rail.focused_row("a").unwrap().project, "alpha");
    }

    #[test]
    fn a_watched_session_outside_the_window_shows_no_marker() {
        let n = now();
        let mut rail = SessionRail::default();
        rail.adopt(vec![row("a", "alpha", 5, n)]);
        // The app is watching a replay file that discovery never listed. The
        // rail must not point at some other session as though it were it.
        assert_eq!(rail.focused_index("recording"), None);
        assert!(rail.focused_row("recording").is_none());
        // ...and stepping from there is still possible, not a dead end.
        rail.adopt(vec![row("a", "alpha", 5, n), row("b", "beta", 9, n)]);
        assert_eq!(
            rail.step_focus("recording", 1),
            Some(PathBuf::from("/p/a.jsonl"))
        );
        assert_eq!(
            rail.step_focus("recording", -1),
            Some(PathBuf::from("/p/b.jsonl"))
        );
    }

    #[test]
    fn stepping_focus_wraps_and_never_reports_a_redundant_switch() {
        let n = now();
        let mut rail = SessionRail::default();
        rail.adopt(vec![row("a", "alpha", 1, n)]);
        assert!(
            rail.step_focus("a", 1).is_none(),
            "one row: a keypress must not cost a model rebuild"
        );

        rail.adopt(vec![
            row("a", "alpha", 1, n),
            row("b", "beta", 2, n),
            row("c", "gamma", 3, n),
        ]);
        assert_eq!(
            rail.step_focus("a", -1),
            Some(PathBuf::from("/p/c.jsonl")),
            "wraps backwards"
        );
        assert_eq!(
            rail.step_focus("c", 1),
            Some(PathBuf::from("/p/a.jsonl")),
            "wraps forwards"
        );
        assert!(
            rail.focus_index("a", 0).is_none(),
            "focusing the watched row is not a switch"
        );
    }

    #[test]
    fn the_rail_appears_only_once_there_is_a_choice_to_make() {
        let n = now();
        let mut rail = SessionRail::default();
        assert!(!rail.should_show(), "nothing swept yet");
        rail.adopt(vec![row("a", "alpha", 1, n)]);
        assert!(!rail.should_show(), "one session is what zoe always showed");
        rail.adopt(vec![row("a", "alpha", 1, n), row("b", "beta", 2, n)]);
        assert!(rail.should_show());

        // An explicit toggle overrides the rule in both directions, and keeps
        // overriding it as the workspace changes.
        rail.toggle_visible();
        assert!(!rail.should_show());
        rail.adopt(vec![row("a", "alpha", 1, n), row("b", "beta", 2, n)]);
        assert!(!rail.should_show(), "the override outlives a sweep");
        rail.toggle_visible();
        rail.adopt(vec![row("a", "alpha", 1, n)]);
        assert!(rail.should_show(), "forced on for a lone session");
    }

    #[test]
    fn the_fallback_label_is_lossy_by_construction() {
        assert_eq!(project_label("-home-elwardi-repo-zoetrope"), "zoetrope");
        // Documented loss: the sanitizer flattened `parametric_pump`, and no
        // rule recovers it from the string. This is why `cwd` is preferred.
        assert_eq!(project_label("-home-u-repo-parametric-pump"), "pump");
        assert_eq!(project_label("-"), "—");
        assert_eq!(project_label(""), "—");
    }
}
