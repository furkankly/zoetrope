//! Application state.
//!
//! [`App`] owns the rataflow `Flow` (the rendered graph), the pure
//! [`SessionModel`] (domain truth), and all UI state (pause, mode, scroll, the
//! current session id, the follow camera, and the chip tray).
//! [`App::handle_ui_event`] folds tailer events into the model and re-syncs the
//! graph; events for stale sessions are dropped via [`App::is_current`].

pub mod graph;
pub mod info;
pub mod rail;
pub mod session;
pub mod timeline;

use self::graph::AgentFlow;
pub use self::info::SessionInfo;
use self::session::SessionModel;
use self::timeline::Timeline;
use crate::tailer::UiEvent;
use crate::ui::chips::ChipTray;

/// Whether the app is watching a live session or replaying a finished one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Live mode: tail the latest session for a cwd.
    Live,
    /// Replay mode: timestamp-paced playback of a finished transcript.
    Replay,
}

/// Camera modes: who drives the viewport.
///
/// `o`/`f` name destinations, not toggles — any manual pan/zoom switches to
/// [`Manual`](Camera::Manual) from any mode, and the camera keys are the only
/// way back out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Camera {
    /// Auto-frame everything: re-fits on structural change, pulling back as
    /// the graph grows (default).
    Overview,
    /// Hold readable zoom and track the most recently active agent.
    Follow,
    /// The user has the camera; the app never moves it.
    Manual,
}

/// The zoom floor centering engages at (Follow tracking and click-to-center)
/// — cards must be readable.
pub const FOLLOW_ZOOM: f64 = 1.0;

/// Breathing room (terminal cells, per side) kept around a centered card when
/// clamping zoom so the card fits the canvas — see [`App::center_node`].
const NODE_FIT_PAD: f64 = 2.0;

/// Emergent transport state — derived, never stored as a mode. "Live" is not a
/// property of the file (completion is unknowable); it's "following the edge
/// while it still grows."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Following the edge and appends are arriving right now.
    Live,
    /// Playing forward behind the edge — a replay, or a live session catching up.
    Playing,
    /// Playback halted (`space`) — in a replay or at a live edge.
    Paused,
    /// Parked in the past (scrubbed back off the edge).
    History,
    /// At the edge with no fresh activity — a finished/quiet session.
    Idle,
}

/// How recently an append must have arrived to count as "live".
const LIVE_FRESH: std::time::Duration = std::time::Duration::from_secs(10);

/// Duration of a Follow camera glide between focus targets.
const GLIDE_SECS: f64 = 0.5;

/// Offset distance (world units) below which a glide is a no-op snap — avoids
/// micro-glides and lets a stationary followed agent settle exactly on target.
const GLIDE_SNAP_EPS: f64 = 0.5;

/// An in-progress eased pan of the viewport offset between two points (Follow).
///
/// Tweens the viewport `offset` so a Follow focus change glides rather than
/// teleports. Offsets (not world centers) are stored so the end state is
/// byte-exact with `center_on(target)` regardless of zoom — the destination is
/// captured by probing `center_on` once when the glide starts.
#[derive(Debug, Clone, Copy)]
pub struct CameraGlide {
    pub(crate) from: (f64, f64),
    pub(crate) to: (f64, f64),
    /// Progress in `0.0..=1.0`.
    pub(crate) t: f64,
}

impl CameraGlide {
    /// Eased offset at the current progress (smoothstep ease-in-out).
    fn offset(&self) -> (f64, f64) {
        let e = self.t * self.t * (3.0 - 2.0 * self.t);
        (
            self.from.0 + (self.to.0 - self.from.0) * e,
            self.from.1 + (self.to.1 - self.from.1) * e,
        )
    }
}

/// Items folded between two rungs of the snapshot ladder.
///
/// The trade is latency against memory: a backward seek re-folds at most this
/// many items, and the ladder holds `folded / STRIDE` rungs. Snapshots are
/// cheap because [`SessionModel`] is built from persistent collections — a rung
/// shares structure with its neighbours instead of copying the model.
///
/// Measured on the pathological bench scale (~31k items, 293 agents), halving
/// this to 512 bought no measurable seek time but cost ~9 MB of retained
/// rungs. That is because the fold is no longer what a backward seek spends
/// its time on: `rebuild_to` still discards the whole `Flow` and re-projects
/// every node, which dominates whatever the fold costs. Shrink this only once
/// that is fixed — until then it would buy memory for nothing.
const SNAPSHOT_STRIDE: usize = 1024;

/// A folded model captured at a known point on the timeline.
struct Snapshot {
    /// How many items were folded into `model`.
    folded: usize,
    /// The [`Timeline::generation`] this was taken under. A rung from an older
    /// generation describes a prefix that no longer exists — see the field's
    /// docs — and must be discarded rather than restored.
    generation: u64,
    /// The model as of `folded` items. Restoring it is a clone, which is O(1).
    ///
    /// This includes derived projections (liveness, workflow rollups) as they
    /// stood when the rung was taken, which are meaningless at a different
    /// playhead — every restore re-runs them via `resync`.
    model: SessionModel,
}

/// The central application state, owned by the single UI task.
pub struct App {
    /// The rendered flow graph (agent cards + step edges).
    pub flow: AgentFlow,
    /// The pure domain model the graph is projected from — the **focused**
    /// session: the one the timeline, scrubber and detail panel are bound to.
    pub session: SessionModel,
    /// Models for every other watched session, keyed by session id.
    ///
    /// Deliberately separate from [`session`](Self::session) rather than one
    /// map holding all of them, because the asymmetry is real: the focused
    /// session has a [`Timeline`] and can be scrubbed, the others are folded
    /// straight off their live edge and only ever render. Flattening the two
    /// would mean either giving every session a timeline it never uses, or
    /// pretending the focused one is not special when every seek path says it
    /// is. Never contains [`current_session_id`](Self::current_session_id).
    pub others: std::collections::BTreeMap<String, SessionModel>,
    /// Live vs replay.
    pub mode: Mode,
    /// Play/pause state — freezes the playhead in **both** replay and live (a
    /// live pause parks at the edge and buffers appends). See `toggle_play_pause`.
    pub is_paused: bool,
    /// Id of the session currently being watched; events for other ids are
    /// dropped (stale buffered messages across a switch).
    pub current_session_id: String,
    /// Who drives the viewport: overview (auto-fit), follow (track activity),
    /// or manual. Manual pan/zoom takes the camera; `o`/`f` give it back.
    pub camera: Camera,
    /// Scroll offset for the selected agent's tool-call list in the detail
    /// panel. The renderer clamps this to the real maximum and writes it back, so
    /// it always reflects the actual top line (the scroll indicator reads it).
    pub detail_scroll: u16,
    /// Whether the detail panel auto-tails (bottom-anchors to the newest tool
    /// call). True by default and after selecting an agent; scrolling up detaches
    /// it; scrolling back to the bottom re-attaches. Independent of the camera.
    pub detail_follow: bool,
    /// Ephemeral tool-call chips drawn as an overlay below agent cards.
    pub chips: ChipTray,
    /// Whether the help overlay is shown (`?` toggles, `esc` closes).
    pub show_help: bool,
    /// Layout deferred while the camera is Manual: structural changes use
    /// local placement only (nothing existing moves under the user); the full
    /// Sugiyama runs when the camera re-engages (`o`/`f`).
    pub layout_dirty: bool,
    /// Most recent tailer error, surfaced in the status bar (`None` = healthy).
    pub last_error: Option<String>,
    /// True once the event loop should terminate.
    pub should_quit: bool,
    /// In-progress Follow camera glide, advanced each frame by
    /// [`tick_camera`](Self::tick_camera). `None` when the camera is settled or
    /// not in Follow.
    pub camera_glide: Option<CameraGlide>,
    /// The unified replay/live timeline: the ts-ordered item list plus the
    /// playhead. The App folds the prefix `items[0..fold_target()]` into
    /// [`session`](Self::session); advanced each frame by
    /// [`tick_timeline`](Self::tick_timeline).
    pub timeline: Timeline,
    /// Screen rect of the scrubber bar from the last render, so the input
    /// handler can map a click/drag on it to a seek. `None` when not drawn.
    pub scrubber_area: Option<ratatui::layout::Rect>,
    /// Inner rect of the session rail from the last render, so a click can be
    /// mapped back to a row. `None` when the rail is not drawn.
    pub rail_area: Option<ratatui::layout::Rect>,
    /// Cached per-column scrubber tallies (head-independent), recomputed only when
    /// the item count or bar width changes — not every frame. See
    /// [`crate::ui::ScrubberTally`].
    pub(crate) scrubber_tally: Option<crate::ui::ScrubberTally>,
    /// Cached era-header flags for the detail panel's tool list — the
    /// O(calls × prompts) attribution, recomputed only when the selected
    /// agent / its calls / the prompts change. See [`crate::ui::panel::EraCache`].
    pub(crate) era_cache: Option<crate::ui::panel::EraCache>,
    /// When the last genuine append (tail `Batch`) folded — drives the emergent
    /// "live" state ([`transport`](Self::transport)). `None` until one arrives.
    pub last_batch_at: Option<web_time::Instant>,
    /// Session-level metadata kept OFF the timeline (final mode/permission-mode,
    /// last prompt, queued/file-edit counts), shown in the `i` overlay. Session-
    /// constant, so it survives seek rebuilds (lives here, not in `session`).
    pub session_info: SessionInfo,
    /// Whether the `i` session-info overlay is shown (`i` toggles, `esc` closes).
    pub show_info: bool,
    /// Node id awaiting a center-glide, set on a user selection and consumed by
    /// `draw` AFTER the flow has rendered into the split (narrower) canvas —
    /// `center_on` probes the last-rendered size, so centering at event time
    /// would target the pre-split width.
    pub pending_center: Option<String>,
    /// Scrub target (bar fraction) queued by input handling, applied once per
    /// frame by [`tick_timeline`](Self::tick_timeline) — a burst of drag events
    /// costs ONE seek (a backward seek rebuilds the whole model), not one per
    /// mouse event.
    pub pending_seek: Option<f64>,
    /// Every live session, and how the workspace is laid out. Populated by
    /// [`UiEvent::Sessions`](crate::tailer::UiEvent::Sessions) sweeps; empty in
    /// the browser frontend, which has no filesystem to sweep.
    pub rail: crate::state::rail::SessionRail,
    /// Ladder of folded-model snapshots, ascending by `folded`, used to start a
    /// backward seek near its target instead of re-folding from item zero.
    snapshots: Vec<Snapshot>,
    /// A session the user asked to focus, queued for the event loop to send to
    /// the tailer as a `Watch`. Queued rather than sent directly because input
    /// handling is synchronous and owns no channel — the same shape as
    /// [`pending_seek`](Self::pending_seek) and
    /// [`pending_center`](Self::pending_center).
    pub pending_watch: Option<std::path::PathBuf>,
    /// The project directory this App was launched against, while it has no
    /// session of its own yet.
    ///
    /// Launching in a project that has never been recorded leaves
    /// [`current_session_id`](Self::current_session_id) empty until discovery
    /// names a session. Discovery describes the WHOLE workspace, newest first,
    /// so the first row it reports usually belongs to some other project —
    /// adopting it would silently show the user a session they did not ask for.
    /// Cleared as soon as a session is adopted.
    awaiting_project: Option<std::path::PathBuf>,
}

impl App {
    /// Construct the initial app state for a session id and mode, with an empty
    /// configured flow and a fresh model.
    pub fn new(session_id: String, mode: Mode) -> Self {
        App {
            flow: graph::new_flow(),
            session: SessionModel::new(session_id.clone()),
            others: std::collections::BTreeMap::new(),
            mode,
            is_paused: false,
            current_session_id: session_id,
            camera: Camera::Overview,
            detail_scroll: 0,
            detail_follow: true,
            chips: ChipTray::default(),
            show_help: false,
            layout_dirty: false,
            last_error: None,
            should_quit: false,
            camera_glide: None,
            timeline: Timeline::new(),
            scrubber_area: None,
            rail_area: None,
            scrubber_tally: None,
            era_cache: None,
            last_batch_at: None,
            session_info: SessionInfo::default(),
            show_info: false,
            pending_center: None,
            pending_seek: None,
            rail: Default::default(),
            snapshots: Vec::new(),
            pending_watch: None,
            awaiting_project: None,
        }
    }

    /// Derive the emergent transport state for display (status badge + scrubber
    /// tag). "Live" requires both following the edge AND a recent append, so an
    /// old session followed to its (static) edge reads `Idle`, not `Live`.
    pub fn transport(&self) -> Transport {
        // Paused is explicit user intent, so it outranks "parked in the past":
        // pausing drops `follow_head` (it parks the cursor), and a deliberate
        // pause should read `Paused`, not `History`.
        if self.is_paused {
            return Transport::Paused;
        }
        if !self.timeline.follow_head {
            return Transport::History;
        }
        let fresh = self.last_batch_at.is_some_and(|t| t.elapsed() < LIVE_FRESH);
        if fresh && self.timeline.at_edge() {
            return Transport::Live;
        }
        // Following but behind the edge = playing forward (a replay playing
        // through OR a live session catching up after a scrub-back), regardless
        // of the `replay` flag.
        if !self.timeline.at_edge() {
            return Transport::Playing;
        }
        Transport::Idle
    }

    /// Seek to a fraction (`0.0..=1.0`) along the timeline — a scrubber
    /// click/drag. Index-based (see [`Timeline::progress`]), so the playhead
    /// lands under the cursor and activity is evenly reachable. No-op when empty.
    pub fn seek_to_fraction(&mut self, f: f64) {
        let len = self.timeline.items.len();
        if len == 0 {
            return;
        }
        // Map the click to a folded-item count over the reachable range so the
        // leftmost click lands on the start clump (position 0).
        let target = self.timeline.fold_at_fraction(f).clamp(1, len);
        self.timeline.cursor = self.timeline.ts_at_index(target - 1);
        self.timeline.follow_head = target >= len;
        self.commit_seek(target);
    }

    /// Step the playhead to the previous/next prompt-era boundary (the natural
    /// tick marks of a session). Re-pins to the edge when stepping past the last
    /// prompt; clamps to the start when stepping before the first.
    pub fn seek_prompt(&mut self, forward: bool) {
        let Some(cur) = self.timeline.cursor else {
            return;
        };
        // Prompt timestamps from the WHOLE timeline, sorted — NOT the folded
        // model. The folded model is (re)built from `items[0..folded]`, so it
        // can't see prompts ahead of the playhead; `]` (next era) needs those,
        // or it overshoots straight to the edge.
        let mut bounds: Vec<chrono::DateTime<chrono::Utc>> = self
            .timeline
            .items
            .iter()
            .filter_map(|i| match &i.update {
                crate::tailer::Update::Entry {
                    source: crate::tailer::Source::Main,
                    entry: crate::transcript::Entry::User(e),
                } if e.is_human_prompt() => i.ts().or(e.envelope.timestamp),
                _ => None,
            })
            .collect();
        bounds.sort_unstable();

        let target = if forward {
            bounds.into_iter().find(|t| *t > cur)
        } else {
            bounds.into_iter().rev().find(|t| *t < cur)
        };
        match target {
            Some(t) => self.seek(t),
            // Past the last prompt → snap to the edge; before the first → start.
            None if forward => self.go_live(),
            None => {
                if let Some(start) = self.timeline.start_ts() {
                    self.seek(start);
                }
            }
        }
    }

    /// Ask to watch the session `delta` rows away in the rail.
    ///
    /// Queues the switch rather than performing it: the tailer owns which files
    /// are open, and it reports the change back as a
    /// [`SessionReset`](crate::tailer::UiEvent::SessionReset), which is what
    /// moves `current_session_id` and therefore the rail's marker. A no-op when
    /// there is nowhere to move.
    pub fn focus_session(&mut self, delta: isize) {
        if let Some(path) = self.rail.step_focus(&self.current_session_id, delta) {
            self.watch_session(path);
        }
    }

    /// Ask to watch the rail row at `index` (a click).
    pub fn focus_session_index(&mut self, index: usize) {
        if let Some(path) = self.rail.focus_index(&self.current_session_id, index) {
            self.watch_session(path);
        }
    }

    /// Switch the focused session to the one at `path`.
    ///
    /// The switch happens HERE, not when the tailer reports back. The tailer no
    /// longer announces which session it attached to — it watches what it is
    /// told and stamps events with that id — so the App is the only thing that
    /// knows a switch happened, and the only thing that can reset the timeline
    /// for it. Doing it here also means the id is right before the first batch
    /// of the new session arrives, so nothing is dropped or misrouted.
    fn watch_session(&mut self, path: std::path::PathBuf) {
        let session_id = crate::transcript::session_id_from_path(&path);
        if session_id.is_empty() || self.is_current(&session_id) {
            return;
        }
        // The session being left keeps its place on the canvas only if
        // something still feeds it; the supervisor decides that on its next
        // sweep. Drop its focused model either way — a monitor will rebuild it.
        graph::remove_session(&mut self.flow, &self.current_session_id);
        // The session being entered may already be on the canvas as a monitored
        // one; the focused model replaces it, and two models projecting the
        // same node ids would fight over every card's content.
        self.others.remove(&session_id);

        self.current_session_id = session_id.clone();
        self.session = SessionModel::new(session_id);
        self.timeline = Timeline::new();
        self.snapshots.clear();
        self.chips.clear();
        self.camera = Camera::Overview;
        self.camera_glide = None;
        self.detail_scroll = 0;
        self.detail_follow = true;
        self.layout_dirty = false;
        self.pending_watch = Some(path);
    }

    /// Wait for a session to appear in `project` rather than adopting whatever
    /// discovery reports first. Only meaningful before any session is watched.
    pub fn await_project(&mut self, project: std::path::PathBuf) {
        if self.current_session_id.is_empty() {
            self.awaiting_project = Some(project);
        }
    }

    /// Focus the session a just-selected node belongs to, if it is not already
    /// the focused one.
    ///
    /// The switch is queued like any other (`pending_watch`), so the model and
    /// timeline arrive a frame or two later; the selection itself survives
    /// because node ids are session-qualified and stable across the switch.
    fn focus_selected_session(&mut self, node_id: &str) {
        let Some((session, _)) = graph::split_node_id(node_id) else {
            return;
        };
        if session == self.current_session_id {
            return;
        }
        // Only sessions discovery knows about can be watched — it is the rail
        // row that carries the transcript path.
        let Some(row) = self.rail.rows.iter().find(|r| r.session_id == session) else {
            return;
        };
        let path = row.main_path.clone();
        self.watch_session(path);
    }

    /// The rail row for the session being watched, if the rail lists it.
    pub fn focused_row(&self) -> Option<&crate::state::rail::RailRow> {
        self.rail.focused_row(&self.current_session_id)
    }

    /// Re-pin to the timeline edge ("go live" / jump to end) and resume playback.
    pub fn go_live(&mut self) {
        self.is_paused = false;
        if let Some(head) = self.timeline.head_ts() {
            self.seek(head);
        } else {
            self.timeline.follow_head = true;
        }
    }

    /// Fold a tailer [`UiEvent`] into state.
    ///
    /// Routes by `session_id` rather than dropping: the canvas shows every
    /// watched session, so a batch for a session that is not focused folds into
    /// [`others`](Self::others) instead of being discarded. Only the focused
    /// session's batches reach the [`Timeline`] — see
    /// [`fold_other`](Self::fold_other).
    pub fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Batch {
                session_id,
                updates,
            } => {
                // Not the focused session: it is one of the others sharing the
                // canvas. Same event, routed by id — the App is the only thing
                // that knows which session is focused, so it is the only thing
                // that should be deciding this.
                if !self.is_current(&session_id) {
                    self.fold_other(&session_id, updates);
                    return;
                }
                // Route untimed session-level metadata to the info store; only
                // real activity (timestamped / dated) goes on the timeline.
                let mut activity = Vec::with_capacity(updates.len());
                for update in updates {
                    if let crate::tailer::Update::Entry { entry, .. } = &update
                        && entry.is_timeline_noise()
                    {
                        self.session_info.apply(entry);
                        continue;
                    }
                    activity.push(update);
                }
                // Stamp freshness for the emergent "live" state.
                self.last_batch_at = Some(web_time::Instant::now());
                // Whether we're riding the edge — decide BEFORE `append_live`
                // moves the cursor.
                let following = self.timeline.following();
                if following {
                    // The model is order-independent, so apply the new updates
                    // DIRECTLY (their position in the now-ts-sorted `items` is
                    // irrelevant) — out-of-order live arrivals never force a
                    // rebuild. Then mark the whole stream folded.
                    let mut structural = false;
                    for update in &activity {
                        structural |= self.session.apply_update(update);
                    }
                    self.timeline.append_live(activity);
                    self.timeline.folded = self.timeline.items.len();
                    self.note_fold();
                    self.commit_fold(structural);
                } else {
                    // Scrubbed back: new live data extends the (sorted) timeline
                    // but stays hidden until we seek forward. A rare late PAST
                    // item (ts ≤ cursor) becomes due → rebuild to fold it.
                    self.timeline.append_live(activity);
                    let target = self.timeline.fold_target();
                    if target != self.timeline.folded {
                        self.rebuild_to(target);
                    }
                }
            }
            UiEvent::ReplayLoaded {
                session_id,
                items,
                speed,
                info,
            } => {
                if !self.is_current(&session_id) {
                    return;
                }
                // Bulk hand-off: the App owns pacing from here. Fold the first
                // moment immediately so t=0 renders; `tick_timeline` paces on.
                self.session_info = info;
                self.timeline.load_replay(items, speed);
                if self.mode == Mode::Live {
                    // `--follow` on a file: ride the (possibly growing) edge
                    // instead of replaying from the start.
                    self.timeline.replay = false;
                    self.timeline.follow_head = true;
                    self.timeline.cursor = self.timeline.head_ts();
                }
                let target = self.timeline.fold_target();
                self.fold_to(target);
            }
            UiEvent::SessionReset { session_id } => {
                // A monitored session truncated or rotated: drop it and let its
                // own tailer re-attach. Never touches the focused session.
                if !self.is_current(&session_id) {
                    self.forget_session(&session_id);
                    return;
                }

                // The focused session truncated or rotated. Rebuild it from
                // scratch so stale nodes cannot linger — but take only ITS
                // subtree off the canvas; the rest belongs to other sessions
                // that are still being fed.
                let had_nodes = self
                    .flow
                    .nodes()
                    .any(|n| graph::split_node_id(&n.id).is_some_and(|(s, _)| s == session_id));
                graph::remove_session(&mut self.flow, &session_id);
                self.session = SessionModel::new(session_id);
                // The rungs describe a file that no longer exists, and a fresh
                // `Timeline` restarts the generation counter — so they could not
                // be told apart by it either.
                self.snapshots.clear();
                // A reset is a fresh live timeline (only live emits resets).
                // Replay arrives via `ReplayLoaded`, never a reset.
                self.timeline = Timeline::new();
                // An empty graph means nothing was showing yet, so resetting the
                // view would clobber a camera choice made while waiting.
                if had_nodes {
                    self.camera = Camera::Overview;
                    self.detail_scroll = 0;
                    self.detail_follow = true;
                    self.layout_dirty = false;
                    self.chips.clear();
                    self.camera_glide = None;
                }
            }
            UiEvent::Sessions(rows) => {
                // Not gated on `is_current`: a sweep describes the whole
                // workspace, not one session, so it is never stale for the
                // watched one.
                // Launched on a project with no session yet: adopt the newest
                // session OF THAT PROJECT once one appears, so the tailer gets
                // a concrete file and the canvas has something to show. Rows
                // are workspace-wide and newest-first, so without the project
                // filter this adopts an unrelated project's session.
                if self.current_session_id.is_empty()
                    && let Some(project) = self.awaiting_project.clone()
                    && let Some(row) = rows
                        .iter()
                        .find(|r| r.main_path.parent() == Some(project.as_path()))
                {
                    let path = row.main_path.clone();
                    self.awaiting_project = None;
                    self.watch_session(path);
                }
                self.apply_session_labels(&rows);
                self.rail.adopt(rows);
            }
            UiEvent::Error(msg) => {
                self.last_error = Some(msg);
            }
        }
    }

    /// Fold the timeline prefix forward to `target` items and reconcile the view.
    ///
    /// The shared fold path for both feeders (live `Batch` append and replay
    /// pacing): apply newly-due updates to the model, re-sync the graph, spawn/
    /// retire overlay chips, and reframe for the camera. A no-op when nothing new
    /// is due. Backward moves (`target < folded`) are a seek and rebuild, handled
    /// in [`seek`](Self::seek) — not here.
    fn fold_to(&mut self, target: usize) {
        if target <= self.timeline.folded {
            return;
        }
        let structural = self.fold_range(self.timeline.folded, target);
        self.timeline.folded = target;
        self.commit_fold(structural);
    }

    /// Fold `items[from..to]` into the model, taking a ladder rung every
    /// [`SNAPSHOT_STRIDE`] items. Returns whether anything structural changed.
    ///
    /// Rungs are taken INSIDE the loop, not after it: a jump straight to the
    /// live edge folds the whole timeline in one call, and a ladder that only
    /// recorded where each fold *ended* would hold a single rung at the edge —
    /// useless for seeking backward, which needs a rung at or before its target.
    fn fold_range(&mut self, from: usize, to: usize) -> bool {
        let generation = self.timeline.generation;
        self.drop_stale_rungs(generation);
        let mut structural = false;
        for i in from..to {
            structural |= self.session.apply_update(&self.timeline.items[i].update);
            self.maybe_snapshot(i + 1, generation);
        }
        structural
    }

    /// Push a rung if `folded` is a full stride past the last one.
    ///
    /// Shared by both fold paths so the cadence cannot drift between them.
    fn maybe_snapshot(&mut self, folded: usize, generation: u64) {
        let last = self.snapshots.last().map_or(0, |s| s.folded);
        if folded >= last + SNAPSHOT_STRIDE {
            self.snapshots.push(Snapshot {
                folded,
                generation,
                model: self.session.clone(),
            });
            // This rung folds the prefix as it stands NOW, so every re-sort
            // recorded so far is already baked into it — including the ones
            // that predate the whole ladder (a replay load moves every index).
            // Leaving them pending would make the next prune apply them to
            // rungs they cannot possibly invalidate, and the ladder would
            // collapse to a single rung on the first out-of-order batch.
            self.timeline.take_disturbance();
        }
    }

    /// Record a ladder rung for a fold that did not go through
    /// [`fold_range`](Self::fold_range) — the live `Batch` path, which applies
    /// updates directly (the model is order-independent) and jumps `folded` to
    /// the edge. Successive batches leave rungs roughly a stride apart, which
    /// is what a later backward seek restores from.
    fn note_fold(&mut self) {
        let generation = self.timeline.generation;
        self.drop_stale_rungs(generation);
        self.maybe_snapshot(self.timeline.folded, generation);
    }

    /// Reconcile the ladder with a re-sorted or replaced item list.
    ///
    /// A rung describes `items[0..folded]`, so it survives exactly as long as
    /// that prefix does. The timeline reports how much of it the re-sorts left
    /// alone ([`Timeline::take_disturbance`]); rungs below that line are still
    /// accurate and are re-stamped to the new generation instead of thrown
    /// away. This is what keeps the ladder alive in a live multi-agent session,
    /// where a sidecar entry arriving a moment out of order re-sorts the tail
    /// on most batches — voiding the whole ladder there left every backward
    /// seek folding from item zero, which is the case it exists for.
    fn drop_stale_rungs(&mut self, generation: u64) {
        if self
            .snapshots
            .last()
            .is_some_and(|s| s.generation != generation)
        {
            let stable = self.timeline.take_disturbance();
            self.snapshots.retain(|s| s.folded <= stable);
            for rung in &mut self.snapshots {
                rung.generation = generation;
            }
        }
    }

    /// Post-apply step shared by `fold_to` (forward, index-based — replay pacing
    /// and seeks) and the live `Batch` path (which applies updates directly,
    /// since the model is order-independent and `items` is kept ts-sorted): roll
    /// workflow status up, re-sync the graph, animate new chips (or seed silently
    /// on the first live backfill), and reframe the camera.
    fn commit_fold(&mut self, structural: bool) {
        let sync_structural = self.resync();

        // The chip tray is derived from model state every frame by
        // `chips.reconcile` (in `tick_timeline`), so a fold needs no per-fold
        // spawn. The one exception is the live-attach backfill: the first live
        // fold carries the whole existing file, and history isn't activity —
        // absorb it silently so `reconcile` won't animate it as new completions.
        // (Replay is paced, so every fold there is genuine activity we let
        // `reconcile` pick up.)
        if self.mode == Mode::Live && !self.chips.is_seeded() {
            self.chips.adopt_baseline(&self.session);
        }

        match self.camera {
            // Overview: a structural change re-frames the graph (deferred fit,
            // safe before first render).
            Camera::Overview => {
                if (structural || sync_structural) && self.session.agent_count() > 0 {
                    self.flow.request_fit_view();
                }
            }
            // Follow: keep the most recently active agent centered.
            Camera::Follow => self.track_activity(),
            Camera::Manual => {}
        }
    }

    /// Advance the timeline playhead one frame and fold any newly-due items.
    ///
    /// Replay paces the cursor forward by `elapsed × speed`; live following is a
    /// no-op here (the cursor is pinned to the head as `Batch`es arrive). On
    /// replay end, settle interactive agents to idle exactly once.
    pub fn tick_timeline(&mut self, elapsed: std::time::Duration) {
        // Apply at most one queued scrub per frame (see `pending_seek`).
        if let Some(f) = self.pending_seek.take() {
            self.seek_to_fraction(f);
        }
        self.timeline.advance(elapsed, self.is_paused);
        let target = self.timeline.fold_target();
        self.fold_to(target);
        if self.timeline.just_ended() {
            // The recording is over: every interactive agent goes idle
            // (completion is unclaimable; activity is provably absent).
            self.session.end_of_stream();
            self.resync();
        }
        // Reconcile the chip tray against model state. Completed-run afterglows
        // age in playing wall-time: real time accrues only while the playhead
        // advances (following, not paused), so the afterglow is a stable viewing
        // window regardless of speed or gap-compression, and a chip you pause on
        // stays put. Runs every frame so a fold's new activity and a seek's
        // reconstructed in-flight tools surface without a per-fold spawn.
        let playing = self.timeline.follow_head && !self.is_paused;
        self.chips.reconcile(elapsed, playing, &self.session);
    }

    /// Seek the playhead to `target` (a scrubber drag/jump) and rebuild the view
    /// as-of-then. Forward seeks fold the new prefix in place (cheap); backward
    /// seeks rebuild a fresh model from the prefix (see `rebuild_to`). Re-pins
    /// to the edge when seeking to/past the head. A seek is discontinuous, so
    /// ephemerals (chips, glide) reset rather than animate across the jump.
    ///
    pub fn seek(&mut self, target: chrono::DateTime<chrono::Utc>) {
        let head = self.timeline.head_ts();
        self.timeline.cursor = Some(target);
        self.timeline.follow_head = head.is_none_or(|h| target >= h);
        let target_fold = self.timeline.fold_target();
        self.commit_seek(target_fold);
    }

    /// Move the model to `target` folded items — the shared body of every seek
    /// (by time, fraction, or index). Assumes `cursor`/`follow_head` are already
    /// set. Forward folds in place; backward rebuilds (see `rebuild_to`). A
    /// seek is discontinuous, so ephemerals (chips, glide) reset rather than
    /// animate across the jump.
    ///
    fn commit_seek(&mut self, target: usize) {
        // A seek changes which tools the panel shows — a stale scroll offset would
        // blank the (now-shorter) list until the user scrolled.
        self.detail_scroll = 0;
        self.detail_follow = true;
        // A seek is a discontinuous jump → drop the stale per-gap pacing budget.
        self.timeline.reset_pacing();
        // Seeking to the live edge means "follow from here" — a lingering pause
        // would freeze the playhead at the edge instead of riding it.
        if self.timeline.follow_head {
            self.is_paused = false;
        }
        if target < self.timeline.folded {
            self.rebuild_to(target);
        } else {
            self.fold_range(self.timeline.folded, target);
            self.timeline.folded = target;
            self.resync();
        }

        self.camera_glide = None;
        // Re-baseline from the post-seek model: absorb the completed history at
        // this playhead silently (don't replay its afterglow), leaving the next
        // `reconcile` to reconstruct the tools in-flight here — a pending tool is
        // state and must appear wherever you scrub into its interval.
        self.chips.adopt_baseline(&self.session);
        // A seek is time-navigation, not a spatial change — the camera is the
        // user's, so don't reframe it (that snapped the graph on every scrub
        // click). Only Follow tracks the action, and it glides (smooth, not a
        // snap). Overview re-fits on live GROWTH, not on a scrub.
        if self.camera == Camera::Follow {
            self.track_activity();
        }
    }

    /// Rebuild the model and graph from scratch by re-folding `items[0..target]`
    /// into a fresh [`SessionModel`] — the only way to move the playhead
    /// *backward* (folding is forward-only). Preserves view state across the
    /// rebuild: node positions (the user's arrangement / a stable layout) and
    /// the selected node are carried over by id.
    fn rebuild_to(&mut self, target: usize) {
        // Start from the newest ladder rung at or before the target instead of
        // from item zero. Restoring a rung is a clone of a persistent model —
        // O(1) — so the fold that follows is bounded by SNAPSHOT_STRIDE rather
        // than by how far back the user seeked.
        let generation = self.timeline.generation;
        self.drop_stale_rungs(generation);
        let resume = self
            .snapshots
            .iter()
            .rev()
            .find(|s| s.folded <= target)
            .map(|s| (s.model.clone(), s.folded));
        let (model, start) = match resume {
            Some((model, folded)) => (model, folded),
            None => (SessionModel::new(self.current_session_id.clone()), 0),
        };

        // Keep the model we are replacing, to work out what left the canvas.
        let previous = std::mem::replace(&mut self.session, model);
        // The label comes from the discovery sweep, not from folding, so a rung
        // taken before the sweep carries a stale one. Restoring it would rename
        // the root card back to "claude" on every backward seek until the next
        // sweep happened to notice.
        self.session.label = previous.label.clone();
        // Re-folding also rebuilds the ladder above `start`, so a scrub that
        // walks backward repeatedly keeps finding rungs near where it lands.
        self.fold_range(start, target);
        self.timeline.folded = target;

        // Seeking back can only REMOVE agents (a fold never un-spawns one that
        // the earlier prefix already had), and `sync` adds and updates but never
        // removes — so taking those few nodes off the canvas is the whole
        // difference between here and a full rebuild.
        //
        // `OrdMap::diff` skips subtrees the two models share, and they share
        // almost everything: the new model is a rung cloned from this very
        // lineage. So this costs what actually changed, not what exists.
        let departed: Vec<String> = previous
            .agents
            .diff(&self.session.agents)
            .filter_map(|change| match change {
                imbl::ordmap::DiffItem::Remove(id, _) => Some(id.clone()),
                _ => None,
            })
            .collect();
        graph::remove_agents(&mut self.flow, &self.current_session_id, &departed);

        // No flow wipe, so node positions, the viewport, the selection and every
        // OTHER session's subtree survive on their own — none of it has to be
        // captured and restored around a teardown any more.
        self.resync();
    }

    /// Roll workflow-group status up from children, then project the model onto
    /// the flow. Every event arm that mutates the model funnels through here so
    /// the workflow rollup can never be skipped before a sync (a just-completed
    /// group would otherwise render Running with an animated edge until the next
    /// batch). Returns whether the sync changed graph structure.
    fn resync(&mut self) -> bool {
        self.session.recompute_workflow_status();
        // Interactive sidechains (forks) derive liveness against the timeline's
        // "now": wall clock at a live edge, the playhead when replaying or
        // scrubbed back (so the as-of-then state shows, no wall-clock bleed).
        let now = self.timeline.now_reference();
        self.session.recompute_liveness(now);
        // Layout is user-driven: a Sugiyama pass on every new node reflows the
        // whole graph and reads as "jumpy" as a session grows. So sync NEVER
        // auto-relayouts — new nodes keep their local placement (below parent,
        // fanned past siblings) and nothing existing moves until the user asks
        // to tidy (`r`, or re-engaging the camera with `o`/`f`). `layout_dirty`
        // tracks that there is un-applied growth for the on-demand path.
        let mut structural = graph::sync(&mut self.flow, &self.session, false);
        // Every other watched session is on the same canvas. Their liveness is
        // wall-clock derived (they have no playhead of their own), and their
        // workflow rollups need the same pass the focused session gets.
        let now = chrono::Utc::now();
        let mut others = std::mem::take(&mut self.others);
        for model in others.values_mut() {
            structural |= Self::project(&mut self.flow, model, now);
        }
        self.others = others;
        if structural {
            self.layout_dirty = true;
            // A session that just gained a node may now overlap its neighbour.
            graph::repack_if_overlapping(&mut self.flow);
        }
        structural
    }

    /// Fold a batch belonging to a session that is NOT focused.
    ///
    /// Straight into that session's model — no timeline, so no pacing and no
    /// seeking. A monitored session is something you watch move, not something
    /// you scrub; giving it a playhead would mean holding its whole item list
    /// for a view that only ever shows the live edge.
    fn fold_other(&mut self, session_id: &str, updates: Vec<crate::tailer::Update>) {
        // A monitor's batch can still be in flight when its session becomes the
        // focused one. Folding it here would build a second model projecting
        // onto the same node ids as the focused model, and the two would
        // overwrite each other's card content every sync.
        if self.is_current(session_id) {
            return;
        }
        // Taken OUT of the map so the model and the flow can be borrowed at
        // once; put back below.
        let mut model = self
            .others
            .remove(session_id)
            .unwrap_or_else(|| SessionModel::new(session_id.to_string()));
        for update in &updates {
            model.apply_update(update);
        }
        // Project ONLY this session. Every monitor polls several times a second
        // and a full `resync` walks every agent of every model, so syncing the
        // whole canvas per batch scales with sessions squared for no reason —
        // nothing else changed. Time-derived liveness for the other sessions is
        // refreshed by `status_tick` regardless.
        let structural = Self::project(&mut self.flow, &mut model, chrono::Utc::now());
        self.others.insert(session_id.to_string(), model);
        if structural {
            self.layout_dirty = true;
            graph::repack_if_overlapping(&mut self.flow);
        }
    }

    /// Re-derive a monitored session's projections and draw it on `flow`.
    ///
    /// The order matters: the workflow rollup reads child statuses that
    /// liveness has yet to set, so it runs first — the same sequence `resync`
    /// applies to the focused session. Returns whether the canvas gained or
    /// lost nodes.
    fn project(
        flow: &mut graph::AgentFlow,
        model: &mut SessionModel,
        now: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        model.recompute_workflow_status();
        model.recompute_liveness(Some(now));
        graph::sync(flow, model, false)
    }

    /// Name each watched session's root card from the sweep's project names.
    ///
    /// The models are folded from transcripts, which do not carry a project
    /// name; the sweep reads it from each session's `cwd`. Applied on every
    /// sweep rather than once, because a session can be folded from its batches
    /// before discovery has ever described it.
    fn apply_session_labels(&mut self, rows: &[crate::state::rail::RailRow]) {
        let mut changed = false;
        for row in rows {
            let label = Some(row.project.clone());
            if self.is_current(&row.session_id) {
                if self.session.label != label {
                    self.session.label = label;
                    changed = true;
                }
            } else if let Some(model) = self.others.get_mut(&row.session_id)
                && model.label != label
            {
                model.label = label;
                changed = true;
            }
        }
        // One resync for the whole sweep: it walks every agent of every model,
        // so doing it per row would scale with sessions squared.
        if changed {
            self.resync();
        }
    }

    /// Stop watching `session_id`: drop its model and take its subtree off the
    /// canvas. No-op for the focused session, which is owned elsewhere.
    pub fn forget_session(&mut self, session_id: &str) {
        if session_id == self.current_session_id {
            return;
        }
        if self.others.remove(session_id).is_some() {
            graph::remove_session(&mut self.flow, session_id);
            self.layout_dirty = true;
        }
    }

    /// Tidy the graph on demand (`r`): run Sugiyama now and reframe for the
    /// current camera. Layout is never automatic (see `resync`),
    /// so this is the user's explicit "rearrange". Forces a pass even when not
    /// `layout_dirty`, so it also re-tidies after manual node dragging.
    pub fn relayout_now(&mut self) {
        graph::relayout(&mut self.flow);
        self.layout_dirty = false;
        match self.camera {
            Camera::Overview => self.flow.request_fit_view(),
            Camera::Follow => self.track_activity(),
            Camera::Manual => {}
        }
    }

    /// Whether `session_id` matches the session currently being watched.
    pub fn is_current(&self, session_id: &str) -> bool {
        self.current_session_id == session_id
    }

    /// Periodic status re-derivation, called by the event loop (~1s).
    ///
    /// Interactive liveness is time-based, so a fully quiet session (no
    /// batches arriving) still needs its running→idle transitions to render —
    /// but the common nothing-changed tick must be near-free, so the graph is
    /// only re-synced when a status actually flipped (no per-second content
    /// rebuilds). Follow's auto-narration also re-engages here: esc-unpin and
    /// stale centering must not wait for the next batch.
    pub fn status_tick(&mut self) {
        let now = self.timeline.now_reference();
        if self.session.recompute_liveness(now) {
            self.session.recompute_workflow_status();
            // Status flips never change topology, so this is a content-only sync;
            // layout stays user-driven (no auto-relayout — see `resync`).
            graph::sync(&mut self.flow, &self.session, false);
        }

        // The monitored sessions share the canvas and their liveness is just as
        // time-derived, but nothing else ticks them: a quiet one produces no
        // batches, and `fold_other` only runs when one arrives. Without this
        // their cards stay green until the supervisor drops the monitor and the
        // whole subtree vanishes, instead of settling to done.
        let App { others, flow, .. } = self;
        let live = chrono::Utc::now();
        let mut structural = false;
        for model in others.values_mut() {
            structural |= Self::project(flow, model, live);
        }
        if structural {
            self.layout_dirty = true;
            graph::repack_if_overlapping(&mut self.flow);
        }

        if self.camera == Camera::Follow {
            self.track_activity();
        }
    }

    /// Center the camera on the most recently active agent (Follow mode).
    ///
    /// Quiet no-op when there are no agents or before the first render
    /// (`center_on` needs the canvas size from the last render). The actual move
    /// is an eased glide (`focus_camera` + per-frame
    /// [`tick_camera`](Self::tick_camera)) rather than a teleport.
    pub fn track_activity(&mut self) {
        let Some(agent) = self.session.last_active_agent_id() else {
            return;
        };
        // Model id → flow id: the camera and selection speak flow ids.
        let id = self.node_id(&agent);
        self.center_node(&id, true); // Follow: clamp zoom for readability.
        // In Follow the panel narrates the followed agent. `select_node` is quiet
        // (no SelectionChanged), so this never trips the drop-Follow detection in
        // the handler — a user selection, which does fire it, drops to Manual and
        // stops this auto-narration entirely.
        if self.selected_agent_id().as_deref() != Some(agent.as_str()) {
            self.flow.select_node(&id);
            self.detail_scroll = 0;
            self.detail_follow = true;
        }
    }

    /// Glide the camera so `id`'s node ends up centered in the canvas — the
    /// shared move behind Follow's tracking and click-to-center. Quiet no-op
    /// for an unknown id or before the first render.
    ///
    /// The pan always glides. `clamp_zoom` additionally snaps the zoom into the
    /// card-readable band — raised to [`FOLLOW_ZOOM`], then clamped down so the
    /// whole card fits the (possibly panel-split) canvas. That snap belongs to
    /// **Follow** (auto-tracking must guarantee legibility) and explicit centering,
    /// so those pass `true`. Manual spatial-nav passes `false`: an arrow press is
    /// just a pan, and must not yank the zoom the user set (the snap-vs-glide
    /// mismatch reads as jumpy, and Manual = the user's camera). Idempotent when
    /// already at the target, so Follow's per-tick re-tracking never restarts it.
    pub fn center_node(&mut self, id: &str, clamp_zoom: bool) {
        let Some(node) = self.flow.node(id) else {
            return;
        };
        let (w, h) = (node.width, node.height);
        let center = (node.position.x + w / 2.0, node.position.y + h / 2.0);

        let canvas = self.flow.canvas_size();
        if clamp_zoom && canvas.width > 0.0 && canvas.height > 0.0 && w > 0.0 && h > 0.0 {
            // Fit zoom: the largest zoom at which the card (plus breathing
            // room) still fits both canvas dimensions.
            let fit = ((canvas.width - 2.0 * NODE_FIT_PAD) / w)
                .min((canvas.height - 2.0 * NODE_FIT_PAD) / h);
            let mut target = self.flow.viewport.zoom.max(FOLLOW_ZOOM);
            if fit > 0.0 {
                target = target.min(fit);
            }
            self.flow.zoom_to(target);
        }
        self.focus_camera(center);
    }

    /// Glide the viewport so `center` (a world point) ends up centered.
    ///
    /// Probes the destination offset by momentarily calling `center_on` and
    /// reading the result back (then restoring), so the glide lands byte-exact
    /// regardless of zoom. Snaps instead of gliding for sub-pixel moves, and
    /// leaves an in-flight glide undisturbed when the target is unchanged (so
    /// re-tracking the same stationary agent each tick doesn't restart it).
    fn focus_camera(&mut self, center: (f64, f64)) {
        let from = (self.flow.viewport.x, self.flow.viewport.y);
        // Probe: center_on mutates the viewport — capture the offset it lands on,
        // then restore so the glide (or snap) drives the actual move.
        self.flow.center_on(center);
        let to = (self.flow.viewport.x, self.flow.viewport.y);
        self.flow.viewport.set_offset(from.0, from.1);

        let settled =
            (to.0 - from.0).abs() < GLIDE_SNAP_EPS && (to.1 - from.1).abs() < GLIDE_SNAP_EPS;
        if settled {
            self.flow.viewport.set_offset(to.0, to.1);
            self.camera_glide = None;
            return;
        }
        // Same destination as an in-flight glide → let it finish, don't restart.
        if let Some(g) = &self.camera_glide
            && (g.to.0 - to.0).abs() < GLIDE_SNAP_EPS
            && (g.to.1 - to.1).abs() < GLIDE_SNAP_EPS
        {
            return;
        }
        self.camera_glide = Some(CameraGlide { from, to, t: 0.0 });
    }

    /// Advance an in-progress camera glide by `dt`, writing the eased offset to
    /// the viewport. Called every frame from the event loop; a no-op when no
    /// glide is active. Viewport writes are quiet, so this never trips the
    /// Manual-camera detection.
    pub fn tick_camera(&mut self, dt: std::time::Duration) {
        let Some(glide) = self.camera_glide.as_mut() else {
            return;
        };
        glide.t = (glide.t + dt.as_secs_f64() / GLIDE_SECS).min(1.0);
        let (x, y) = glide.offset();
        let done = glide.t >= 1.0;
        let to = glide.to;
        self.flow.viewport.set_offset(x, y);
        if done {
            self.flow.viewport.set_offset(to.0, to.1);
            self.camera_glide = None;
        }
    }

    /// The node id of the currently selected agent, if any (read from the flow
    /// during render; copy it out before borrowing `app` mutably).
    pub fn selected_agent_id(&self) -> Option<String> {
        let node = self.selected_node_id()?;
        let (session, agent) = graph::split_node_id(&node)?;
        // A node from another session has no agent in THIS model; returning its
        // id would make the detail panel look it up here and come back empty.
        (session == self.current_session_id).then(|| agent.to_string())
    }

    /// The selected node's `Flow` id — session-qualified, unlike
    /// [`selected_agent_id`](Self::selected_agent_id). This is what the camera
    /// and selection APIs take.
    pub fn selected_node_id(&self) -> Option<String> {
        self.flow.selected_nodes().next().map(|n| n.id.clone())
    }

    /// The `Flow` node id for an agent in the session being watched.
    pub fn node_id(&self, agent: &str) -> String {
        graph::node_id(&self.current_session_id, agent)
    }

    /// Unified play/pause (`space`) that works from any state — **including at a
    /// live edge**:
    /// - **playing** (following the edge, not paused) → park at the current
    ///   playhead;
    /// - **paused or scrubbed into the past** → resume playing **from the current
    ///   cursor** (re-engage `follow_head` so `advance` paces forward from where
    ///   you are — it does NOT jump to the live edge; `End` does that).
    ///
    /// Pause drops `follow_head` on purpose: that's what freezes a *live* session.
    /// `append_live` snaps the cursor to the edge only while following, so a
    /// parked cursor lets new events buffer behind the edge (the same path a
    /// scrub-back uses) instead of yanking the view to each new event. Resume
    /// then catches up through the buffered gap.
    pub fn toggle_play_pause(&mut self) {
        let playing = self.timeline.follow_head && !self.is_paused;
        if playing {
            self.is_paused = true;
            self.timeline.follow_head = false;
        } else {
            self.is_paused = false;
            self.timeline.follow_head = true;
        }
    }

    /// React to the flow events from a user input gesture.
    ///
    /// Every event here is the product of a USER gesture (programmatic mutations
    /// are quiet), so the rule is uniform: Follow yields to ANY interaction;
    /// Overview yields only to a viewport change (pan/zoom). A selection change
    /// resets the detail-panel scroll. Shared by the native input handler and
    /// the browser frontend.
    pub fn process_flow_events(&mut self, events: impl Iterator<Item = rataflow::FlowEvent>) {
        use rataflow::FlowEvent;
        for event in events {
            let drop_camera = self.camera == Camera::Follow
                || (self.camera == Camera::Overview
                    && matches!(event, FlowEvent::ViewportChanged { .. }));
            if drop_camera {
                self.camera = Camera::Manual;
                self.camera_glide = None;
            }
            if let FlowEvent::SelectionChanged { node_ids, .. } = &event {
                self.detail_scroll = 0;
                self.detail_follow = true;
                // Pan the selected node to center (pan-only — the arrow-nav path
                // does NOT clamp zoom; see `center_node`). Deferred to the next
                // draw, which sees the post-split canvas width. A deselection
                // clears any not-yet-consumed one.
                self.pending_center = node_ids.first().cloned();
                // Selecting a card in a session we are only monitoring used to
                // do nothing visible: the node highlights, but the detail panel
                // reads the FOCUSED model and finds no such agent, so it renders
                // blank with no hint why. Reading a card is a request to look at
                // that session, so treat it as one.
                if let Some(id) = node_ids.first() {
                    self.focus_selected_session(id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tailer::{UiEvent, Update};
    use crate::transcript::SubagentMeta;

    fn meta_update(agent_id: &str) -> Update {
        Update::SubagentMeta {
            agent_id: agent_id.to_string(),
            workflow: None,
            meta: SubagentMeta {
                agent_type: Some("guide".into()),
                description: None,
                tool_use_id: Some("ag1".into()),
                stopped_by_user: None,
            },
        }
    }

    /// The whole point of the canvas: sessions the user is NOT focused on are
    /// folded and drawn too, each under its own root.
    #[test]
    fn the_canvas_shows_every_watched_session() {
        let mut app = App::new("focused".to_string(), Mode::Live);
        let t: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();

        // The focused session, through the timeline as usual.
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "focused".into(),
            items: vec![crate::tailer::ReplayItem::at(Some(t), meta_update("sub-a"))],
            speed: 8.0,
            info: Default::default(),
        });

        // A monitored session, folded straight off its live edge.
        app.handle_ui_event(UiEvent::Batch {
            session_id: "other".into(),
            updates: vec![meta_update("sub-b")],
        });

        fn has(app: &App, session: &str, agent: &str) -> bool {
            app.flow.node(&graph::node_id(session, agent)).is_some()
        }
        assert!(has(&app, "focused", "main"), "focused root");
        assert!(has(&app, "focused", "sub-a"), "focused subagent");
        assert!(has(&app, "other", "main"), "monitored root");
        assert!(has(&app, "other", "sub-b"), "monitored subagent");

        // The monitored session has a model of its own, and the focused
        // session's model is untouched by it.
        assert!(app.others.contains_key("other"));
        assert!(app.session.agent("sub-b").is_none());
        assert_eq!(
            1 + app.others.len(),
            2,
            "the focused session plus one other"
        );

        // Selecting a monitored node reports no agent in the FOCUSED model —
        // the detail panel reads that model, and would otherwise render blank
        // for a node that plainly exists.
        app.flow.select_node(&graph::node_id("other", "sub-b"));
        assert!(app.selected_agent_id().is_none());
        app.flow.select_node(&graph::node_id("focused", "sub-a"));
        assert_eq!(app.selected_agent_id().as_deref(), Some("sub-a"));

        // When it stops being live, it leaves the canvas without disturbing
        // the focused session.
        app.handle_ui_event(UiEvent::SessionReset {
            session_id: "other".into(),
        });
        assert!(!has(&app, "other", "main"));
        assert!(has(&app, "focused", "main"));
        assert!(app.others.is_empty());
    }

    /// Selecting a card in a monitored session asks to focus that session,
    /// rather than highlighting a node whose detail panel can only be blank.
    #[test]
    fn selecting_a_monitored_node_focuses_its_session() {
        use rataflow::FlowEvent;

        let mut app = App::new("focused".to_string(), Mode::Live);
        app.handle_ui_event(UiEvent::Batch {
            session_id: "other".into(),
            updates: vec![meta_update("sub")],
        });
        let n = chrono::Utc::now();
        app.rail.adopt(vec![crate::state::rail::RailRow {
            session_id: "other".to_string(),
            project: "bravo".to_string(),
            main_path: std::path::PathBuf::from("/p/other.jsonl"),
            agents: 2,
            failures: 0,
            last_activity: Some(n),
            sidecar_active: false,
        }]);

        let foreign = graph::node_id("other", "sub");
        app.process_flow_events(
            vec![FlowEvent::SelectionChanged {
                node_ids: vec![foreign.clone()],
                edge_ids: vec![],
            }]
            .into_iter(),
        );
        assert_eq!(
            app.pending_watch,
            Some(std::path::PathBuf::from("/p/other.jsonl")),
            "reading a monitored card should ask to watch that session"
        );

        // Selecting inside the focused session is not a switch.
        app.pending_watch = None;
        app.process_flow_events(
            vec![FlowEvent::SelectionChanged {
                node_ids: vec![graph::node_id("focused", "main")],
                edge_ids: vec![],
            }]
            .into_iter(),
        );
        assert_eq!(app.pending_watch, None);

        // Neither is selecting a session discovery has never heard of — there
        // is no transcript path to watch.
        app.process_flow_events(
            vec![FlowEvent::SelectionChanged {
                node_ids: vec![graph::node_id("unknown", "main")],
                edge_ids: vec![],
            }]
            .into_iter(),
        );
        assert_eq!(app.pending_watch, None);
    }

    /// One event pair, routed by `session_id`. The App is the only thing that
    /// knows which session is focused, so it is the only thing that decides
    /// whether a batch goes to the timeline or to a monitored model.
    #[test]
    fn batches_route_by_session_id() {
        let mut app = App::new("focused".to_string(), Mode::Live);

        app.handle_ui_event(UiEvent::Batch {
            session_id: "focused".into(),
            updates: vec![meta_update("mine")],
        });
        app.handle_ui_event(UiEvent::Batch {
            session_id: "other".into(),
            updates: vec![meta_update("theirs")],
        });

        assert!(app.session.agent("mine").is_some(), "focused → the model");
        assert!(
            app.session.agent("theirs").is_none(),
            "another session must not land in the focused model"
        );
        assert!(
            app.others["other"].agent("theirs").is_some(),
            "→ that session's own model"
        );
    }

    /// A reset names the session it is about, and nothing else. It used to be
    /// able to mean "the focused session is now a different one", which is why
    /// an unfamiliar id was ambiguous.
    #[test]
    fn a_reset_only_touches_the_session_it_names() {
        let mut app = App::new("focused".to_string(), Mode::Live);
        app.handle_ui_event(UiEvent::Batch {
            session_id: "focused".into(),
            updates: vec![meta_update("mine")],
        });
        app.handle_ui_event(UiEvent::Batch {
            session_id: "other".into(),
            updates: vec![meta_update("theirs")],
        });

        // A monitored session rotates: it leaves, the focused one is untouched
        // and stays focused.
        app.handle_ui_event(UiEvent::SessionReset {
            session_id: "other".into(),
        });
        assert_eq!(
            app.current_session_id, "focused",
            "focus is not the tailer's"
        );
        assert!(app.others.is_empty());
        assert!(app.flow.node(&graph::node_id("other", "theirs")).is_none());
        assert!(app.session.agent("mine").is_some());
        assert!(app.flow.node(&graph::node_id("focused", "mine")).is_some());

        // The focused session rotates: its model is rebuilt in place.
        app.handle_ui_event(UiEvent::SessionReset {
            session_id: "focused".into(),
        });
        assert_eq!(app.current_session_id, "focused");
        assert!(app.session.agent("mine").is_none(), "rebuilt from scratch");
        assert!(app.flow.node(&graph::node_id("focused", "mine")).is_none());
    }

    #[test]
    fn camera_defaults_to_overview_and_resets() {
        let mut app = App::new("s".into(), Mode::Live);
        assert_eq!(app.camera, Camera::Overview);

        // Something is showing, then the session is truncated…
        app.handle_ui_event(UiEvent::Batch {
            session_id: "s".into(),
            updates: vec![meta_update("sub")],
        });
        app.camera = Camera::Manual; // user took the camera…
        app.handle_ui_event(UiEvent::SessionReset {
            session_id: "s".into(),
        });
        // …and a session rebuilt from nothing re-engages the default.
        assert_eq!(app.camera, Camera::Overview);
    }

    #[test]
    fn first_live_batch_seeds_chips_silently() {
        let mut app = App::new("s".into(), Mode::Live);
        app.handle_ui_event(UiEvent::Batch {
            session_id: "s".into(),
            updates: vec![meta_update("sub1")],
        });
        assert!(
            app.chips.is_seeded(),
            "live attach must adopt backfill without chipping"
        );

        // Replay never seeds — every paced batch is genuine activity.
        let mut replay = App::new("s".into(), Mode::Replay);
        replay.handle_ui_event(UiEvent::Batch {
            session_id: "s".into(),
            updates: vec![meta_update("sub1")],
        });
        assert!(!replay.chips.is_seeded());
    }

    #[test]
    fn announce_reset_preserves_camera_on_empty_graph() {
        // The tailer's initial announce is a same-id reset before any content:
        // a camera choice made while waiting must survive it.
        let mut app = App::new("s".into(), Mode::Live);
        app.camera = Camera::Follow; // user pressed f while waiting
        app.handle_ui_event(UiEvent::SessionReset {
            session_id: "s".into(),
        });
        assert_eq!(
            app.camera,
            Camera::Follow,
            "announce must not clobber camera"
        );

        // A genuine reset (graph populated) still resets the view.
        app.handle_ui_event(UiEvent::Batch {
            session_id: "s".into(),
            updates: vec![meta_update("sub1")],
        });
        app.handle_ui_event(UiEvent::SessionReset {
            session_id: "s".into(),
        });
        assert_eq!(app.camera, Camera::Overview);
    }

    #[test]
    fn status_tick_resumes_follow_narration() {
        let mut app = App::new("s".into(), Mode::Live);
        app.camera = Camera::Follow;
        // Seed chips (live mode first batch) then deliver an agent.
        app.handle_ui_event(UiEvent::Batch {
            session_id: "s".into(),
            updates: vec![meta_update("sub1")],
        });
        // Simulate a moment with no selection (e.g. just after a reset).
        app.flow.clear_selection();
        assert!(app.selected_agent_id().is_none());

        // The 1s status tick must re-engage auto-narration without a batch.
        app.status_tick();
        assert!(
            app.selected_agent_id().is_some(),
            "follow must re-track on the status tick, not wait for a batch"
        );
    }

    #[test]
    fn batches_from_stale_session_are_dropped() {
        let mut app = App::new("CURRENT".into(), Mode::Live);
        app.handle_ui_event(UiEvent::Batch {
            session_id: "STALE".into(),
            updates: vec![meta_update("ghost")],
        });
        assert!(
            app.session.agent("ghost").is_none(),
            "stale-session batch must be dropped"
        );
    }

    #[test]
    fn camera_glide_eases_and_lands_exactly() {
        let mut app = App::new("s".into(), Mode::Live);
        app.camera_glide = Some(CameraGlide {
            from: (0.0, 0.0),
            to: (120.0, 0.0),
            t: 0.0,
        });

        // Halfway through the glide: smoothstep(0.5) == 0.5 → x == 60.
        app.tick_camera(std::time::Duration::from_secs_f64(GLIDE_SECS / 2.0));
        assert!(app.camera_glide.is_some(), "glide still running at t=0.5");
        assert!(
            (app.flow.viewport.x - 60.0).abs() < 1e-6,
            "eased midpoint should be 60, got {}",
            app.flow.viewport.x
        );

        // Overshooting the remaining time clamps to t=1: lands exactly, clears.
        app.tick_camera(std::time::Duration::from_secs_f64(GLIDE_SECS));
        assert!(app.camera_glide.is_none(), "glide cleared on completion");
        assert!(
            (app.flow.viewport.x - 120.0).abs() < 1e-6,
            "glide must land byte-exact on target"
        );
    }

    #[test]
    fn manual_spatial_nav_pans_but_leaves_zoom_untouched() {
        use crate::tailer::ReplayItem;
        use ratatui::widgets::Widget;

        let mut app = App::new("s".into(), Mode::Replay);
        let t: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "s".into(),
            items: vec![ReplayItem::at(Some(t), meta_update("sub1"))],
            speed: 8.0,
            info: Default::default(),
        });
        // Render once so the flow has a non-zero canvas (the zoom clamp needs it).
        let area = ratatui::layout::Rect::new(0, 0, 100, 30);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        (&mut app.flow).render(area, &mut buf);
        assert!(
            app.flow.node(&app.node_id("sub1")).is_some(),
            "the subagent node exists"
        );

        // Zoom OUT past the readable band — the clamp WOULD want to snap it in.
        app.flow.zoom_to(0.3);
        let zoom = app.flow.viewport.zoom;
        assert!(zoom < FOLLOW_ZOOM);

        // Manual spatial-nav (clamp_zoom = false) → pans, leaves the zoom alone.
        let node = app.node_id("sub1");
        app.center_node(&node, false);
        assert_eq!(
            app.flow.viewport.zoom, zoom,
            "arrow-nav must not touch the user's zoom"
        );

        // Follow / explicit center (clamp_zoom = true) → snaps the zoom in for
        // readability — the movement that used to ride along with every arrow.
        app.center_node(&app.node_id("sub1"), true);
        assert!(
            app.flow.viewport.zoom > zoom,
            "Follow still bumps zoom for readability"
        );
    }

    // Drives `handler::process_flow_events`, which is native-only.
    #[cfg(feature = "native")]
    #[test]
    fn user_pan_cancels_glide() {
        let mut app = App::new("s".into(), Mode::Live);
        app.camera = Camera::Follow;
        app.camera_glide = Some(CameraGlide {
            from: (0.0, 0.0),
            to: (50.0, 0.0),
            t: 0.2,
        });

        // A user viewport gesture hands over the camera AND stops the glide, so
        // the app never fights the user's pan.
        crate::handler::process_flow_events(
            &mut app,
            vec![rataflow::FlowEvent::ViewportChanged {
                x: 1.0,
                y: 2.0,
                zoom: 1.0,
            }]
            .into_iter(),
        );
        assert_eq!(app.camera, Camera::Manual);
        assert!(app.camera_glide.is_none(), "user pan must cancel the glide");
    }

    /// The ladder is only allowed to be FASTER. Restoring a rung and folding
    /// forward from it must land on exactly the model a from-scratch rebuild
    /// would produce — otherwise a backward seek silently shows a different
    /// session than the same seek did yesterday.
    /// Seeking back patches the canvas instead of rebuilding it: agents that do
    /// not exist yet at the target leave, and everything the old teardown had to
    /// capture and restore by hand — node positions, the viewport, the
    /// selection, other sessions' subtrees — survives because nothing is thrown
    /// away.
    #[test]
    fn seeking_back_patches_the_canvas_instead_of_rebuilding_it() {
        let t0: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        let mut app = App::new("focused".to_string(), Mode::Replay);
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "focused".into(),
            items: (0..6)
                .map(|i| {
                    crate::tailer::ReplayItem::at(
                        Some(t0 + chrono::Duration::seconds(i as i64)),
                        meta_update(&format!("sub{i}")),
                    )
                })
                .collect(),
            speed: 8.0,
            info: Default::default(),
        });
        app.go_live();

        // A second session shares the canvas, and must be untouched by a seek
        // in the focused one.
        app.handle_ui_event(UiEvent::Batch {
            session_id: "other".into(),
            updates: vec![meta_update("watched")],
        });

        let early = graph::node_id("focused", "sub1");
        let late = graph::node_id("focused", "sub5");
        let foreign = graph::node_id("other", "watched");
        assert!(app.flow.node(&late).is_some(), "precondition: sub5 exists");

        // Arrange the canvas the way a user would: drag a node, pan, select.
        app.flow
            .set_node_positions(std::iter::once((&early, (123.0, 456.0))));
        app.flow.zoom_to(2.5);
        let viewport = app.flow.viewport;
        app.flow.select_node(&early);

        // Seek back to before sub5 (and sub4, sub3…) were ever spawned.
        app.seek_to_fraction(2.0 / 6.0);

        assert!(
            app.flow.node(&late).is_none(),
            "an agent that does not exist at this playhead must leave the canvas"
        );
        assert!(app.session.agent("sub5").is_none(), "and leave the model");
        assert!(app.flow.node(&early).is_some(), "sub1 predates the target");

        // The three things the old rebuild had to save and restore explicitly.
        let node = app.flow.node(&early).unwrap();
        assert_eq!(
            (node.position.x, node.position.y),
            (123.0, 456.0),
            "a dragged position survived the seek"
        );
        assert_eq!(app.flow.viewport.zoom, viewport.zoom, "viewport survived");
        assert_eq!(app.selected_agent_id().as_deref(), Some("sub1"));

        // And the monitored session was never in the focused model's diff.
        assert!(
            app.flow.node(&foreign).is_some(),
            "another session's subtree must not be collateral"
        );
    }

    #[test]
    fn a_ladder_restore_matches_a_full_rebuild() {
        // Enough items that several rungs are taken.
        let count = SNAPSHOT_STRIDE * 3 + 17;
        let t0: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        let items = |n: usize| -> Vec<crate::tailer::ReplayItem> {
            (0..n)
                .map(|i| {
                    crate::tailer::ReplayItem::at(
                        Some(t0 + chrono::Duration::seconds(i as i64)),
                        // Re-touch earlier agents as well as adding new ones, so
                        // the fold is not purely insert-only.
                        meta_update(&format!("sub{}", i % (n / 4).max(1))),
                    )
                })
                .collect()
        };

        let load = |app: &mut App| {
            app.handle_ui_event(UiEvent::ReplayLoaded {
                session_id: "s".into(),
                items: items(count),
                speed: 8.0,
                info: Default::default(),
            });
            app.go_live();
        };

        let target = SNAPSHOT_STRIDE * 2 + 5;

        // With the ladder: fold to the edge (taking rungs), then seek back.
        let mut laddered = App::new("s".to_string(), Mode::Replay);
        load(&mut laddered);
        assert!(
            laddered.snapshots.len() >= 3,
            "expected several rungs, got {}",
            laddered.snapshots.len()
        );
        laddered.seek_to_fraction(target as f64 / count as f64);
        let restored_from = laddered.timeline.folded;

        // Without: same App, same seek, but the ladder emptied first so
        // `rebuild_to` has to fold from item zero.
        let mut plain = App::new("s".to_string(), Mode::Replay);
        load(&mut plain);
        plain.snapshots.clear();
        plain.seek_to_fraction(target as f64 / count as f64);

        assert_eq!(
            restored_from, plain.timeline.folded,
            "both paths must land on the same item"
        );
        assert!(
            laddered.session == plain.session,
            "ladder restore diverged from a full rebuild"
        );
    }

    /// Monitored sessions share the canvas, and their liveness is time-derived
    /// like the focused one's — but they produce no batches once quiet, so the
    /// periodic tick is the only thing that can settle them.
    #[test]
    fn the_status_tick_settles_monitored_sessions_too() {
        use crate::state::session::{AgentStatus, MAIN_ID};

        let mut app = App::new("focused".to_string(), Mode::Live);

        // A session whose last entry is long past the idle threshold, but whose
        // main agent is still marked running — the state a session lands in when
        // it goes quiet between two monitor batches.
        let stale = chrono::Utc::now() - chrono::Duration::seconds(600);
        let mut model = SessionModel::new("other".to_string());
        model.apply_update(&Update::Entry {
            source: crate::tailer::Source::Main,
            entry: crate::transcript::parse_line(&format!(
                r#"{{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"{}","message":{{"role":"user","content":"hi"}}}}"#,
                stale.to_rfc3339()
            ))
            .unwrap(),
        });
        model.agents.get_mut(MAIN_ID).unwrap().status = AgentStatus::Running;
        app.others.insert("other".to_string(), model);

        let running =
            |app: &App| app.others["other"].agent(MAIN_ID).unwrap().status == AgentStatus::Running;
        assert!(running(&app));

        app.status_tick();
        assert!(
            !running(&app),
            "a quiet monitored session must settle on the tick, not linger green"
        );
    }

    /// The project label is sweep-supplied, not folded, so a rung taken before
    /// the sweep must not carry an old one back onto the root card.
    #[test]
    fn a_backward_seek_keeps_the_session_label() {
        let t0: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        let mut app = app_with_rungs(SNAPSHOT_STRIDE * 2, t0);
        // The sweep names the project only after the rungs were taken.
        app.session.label = Some("zoetrope".to_string());

        app.seek_to_fraction(0.25);
        assert_eq!(app.session.label.as_deref(), Some("zoetrope"));
    }

    /// Launching in a project with nothing recorded must not adopt another
    /// project's session — discovery is workspace-wide and newest-first, so the
    /// first row it reports is usually somebody else's work.
    #[test]
    fn an_empty_project_waits_for_its_own_session() {
        use crate::state::rail::RailRow;
        let n = chrono::Utc::now();
        let row = |project: &str, id: &str| RailRow {
            session_id: id.to_string(),
            project: project.to_string(),
            main_path: std::path::PathBuf::from(format!("/projects/{project}/{id}.jsonl")),
            agents: 1,
            failures: 0,
            last_activity: Some(n),
            sidecar_active: false,
        };

        let mut app = App::new(String::new(), Mode::Live);
        app.await_project(std::path::PathBuf::from("/projects/mine"));

        // A sweep that only knows about another project changes nothing.
        app.handle_ui_event(UiEvent::Sessions(vec![row("theirs", "other")]));
        assert_eq!(app.current_session_id, "");
        assert_eq!(app.pending_watch, None);

        // Once one of ours appears — still not the newest row — we take it.
        app.handle_ui_event(UiEvent::Sessions(vec![
            row("theirs", "other"),
            row("mine", "ours"),
        ]));
        assert_eq!(app.current_session_id, "ours");
        assert_eq!(
            app.pending_watch,
            Some(std::path::PathBuf::from("/projects/mine/ours.jsonl"))
        );
    }

    /// Seed a live App with `n` dated items one second apart, folded to the
    /// edge so the ladder has rungs.
    fn app_with_rungs(n: usize, t0: chrono::DateTime<chrono::Utc>) -> App {
        let mut app = App::new("s".to_string(), Mode::Live);
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "s".into(),
            items: (0..n)
                .map(|i| {
                    crate::tailer::ReplayItem::at(
                        Some(t0 + chrono::Duration::seconds(i as i64)),
                        meta_update(&format!("sub{i}")),
                    )
                })
                .collect(),
            speed: 8.0,
            info: Default::default(),
        });
        app.go_live();
        assert!(!app.snapshots.is_empty(), "rungs were taken");
        app
    }

    /// An undated item sorts to the HEAD, so one arriving shifts every index
    /// after it: no rung describes its prefix any more, and all must go.
    #[test]
    fn an_undated_arrival_voids_the_whole_ladder() {
        let t0: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        let mut app = app_with_rungs(SNAPSHOT_STRIDE * 2, t0);

        // A meta with no timestamp of its own — the `needs_dating` path.
        app.handle_ui_event(UiEvent::Batch {
            session_id: "s".into(),
            updates: vec![meta_update("late")],
        });
        // Only a rung taken at the new edge remains; every rung describing the
        // old prefix is gone.
        assert!(
            app.snapshots
                .iter()
                .all(|s| s.folded == app.timeline.folded),
            "a rung describing the pre-sort prefix survived: {:?}",
            app.snapshots.iter().map(|s| s.folded).collect::<Vec<_>>()
        );
    }

    /// The case the ladder exists for: a live session whose sidecars land a
    /// moment out of order. The re-sort touches only the tail, so the rungs
    /// below it still describe their prefix exactly — voiding them wholesale
    /// sent every backward seek back to folding from item zero.
    #[test]
    fn an_out_of_order_batch_keeps_the_rungs_below_it() {
        let count = SNAPSHOT_STRIDE * 3;
        let t0: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        let mut app = app_with_rungs(count, t0);
        let before = app.snapshots.len();
        assert!(before >= 2, "expected several rungs, got {before}");

        // A sidecar entry timestamped just before the edge: out of order, but
        // it cannot disturb anything earlier than itself.
        app.handle_ui_event(UiEvent::Batch {
            session_id: "s".into(),
            updates: vec![crate::tailer::Update::Entry {
                source: crate::tailer::Source::Sub("late".into()),
                entry: crate::transcript::parse_line(&format!(
                    r#"{{"type":"assistant","uuid":"u","parentUuid":null,"timestamp":"{}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"b1","name":"Bash","input":{{}}}}]}}}}"#,
                    (t0 + chrono::Duration::seconds(count as i64 - 2)).to_rfc3339()
                ))
                .unwrap(),
            }],
        });

        assert!(
            app.snapshots.len() >= before - 1,
            "the untouched prefix lost its rungs: {} -> {}",
            before,
            app.snapshots.len()
        );
        assert!(
            app.snapshots
                .iter()
                .all(|s| s.generation == app.timeline.generation),
            "a surviving rung kept a stale generation stamp"
        );

        // And a rung is only worth keeping if restoring it is still correct.
        let target = SNAPSHOT_STRIDE + 5;
        let total = app.timeline.items.len();
        app.seek_to_fraction(target as f64 / total as f64);
        let laddered = app.session.clone();
        let landed = app.timeline.folded;

        app.snapshots.clear();
        app.seek_to_fraction(1.0);
        app.snapshots.clear();
        app.seek_to_fraction(target as f64 / total as f64);
        assert_eq!(landed, app.timeline.folded, "both paths land on one item");
        assert!(
            laddered == app.session,
            "a surviving rung diverged from a full rebuild"
        );
    }

    #[test]
    fn seek_forward_then_back_rebuilds_as_of_then() {
        use crate::tailer::ReplayItem;
        let t1: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        let t2: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:10.000Z".parse().unwrap();
        let items = vec![
            ReplayItem::at(Some(t1), meta_update("sub1")),
            ReplayItem::at(Some(t2), meta_update("sub2")),
        ];

        let mut app = App::new("s".into(), Mode::Replay);
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "s".into(),
            items,
            speed: 8.0,
            info: Default::default(),
        });
        // ReplayLoaded folds the first moment only: sub1 present, sub2 not due.
        assert!(app.session.agent("sub1").is_some());
        assert!(app.session.agent("sub2").is_none());

        // Seek to the end: forward fold brings sub2 in. Select it... then sub1.
        app.seek(t2);
        assert!(app.session.agent("sub2").is_some());
        let node = app.node_id("sub1");
        app.flow.select_node(&node);

        // Seek back before sub2 existed: rebuild must drop it AND keep selection.
        app.seek("2026-06-05T10:00:05.000Z".parse().unwrap());
        assert!(app.session.agent("sub1").is_some());
        assert!(
            app.session.agent("sub2").is_none(),
            "backward seek must rebuild to the earlier state (sub2 gone)"
        );
        assert_eq!(
            app.selected_agent_id().as_deref(),
            Some("sub1"),
            "selection must survive the rebuild"
        );
        assert!(
            !app.timeline.follow_head,
            "seeking into the past unpins follow"
        );
    }

    #[test]
    fn seek_prompt_steps_between_eras() {
        use crate::tailer::{ReplayItem, Source};
        let ts = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
        let prompt = |uuid: &str, t: &str, text: &str| {
            let line = format!(
                r#"{{"type":"user","uuid":"{uuid}","parentUuid":null,"origin":{{"kind":"human"}},"timestamp":"{t}","message":{{"role":"user","content":"{text}"}}}}"#
            );
            ReplayItem::at(
                Some(ts(t)),
                Update::Entry {
                    source: Source::Main,
                    entry: crate::transcript::parse_line(&line).unwrap(),
                },
            )
        };
        let items = vec![
            prompt("p1", "2026-06-05T10:00:00.000Z", "first"),
            prompt("p2", "2026-06-05T11:00:00.000Z", "second"),
            prompt("p3", "2026-06-05T12:00:00.000Z", "third"),
            // Trailing activity after the last era.
            ReplayItem::at(Some(ts("2026-06-05T13:00:00.000Z")), meta_update("sub1")),
        ];
        let mut app = App::new("s".into(), Mode::Replay);
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "s".into(),
            items,
            speed: 8.0,
            info: Default::default(),
        });
        assert_eq!(app.timeline.cursor, Some(ts("2026-06-05T10:00:00.000Z")));

        // ']' from the start lands on the NEXT era boundary — including ones
        // the playhead hasn't folded yet (the model can't know them).
        app.seek_prompt(true);
        assert_eq!(app.timeline.cursor, Some(ts("2026-06-05T11:00:00.000Z")));
        app.seek_prompt(true);
        assert_eq!(app.timeline.cursor, Some(ts("2026-06-05T12:00:00.000Z")));

        // Past the last prompt → re-pins to the edge.
        app.seek_prompt(true);
        assert_eq!(app.timeline.cursor, Some(ts("2026-06-05T13:00:00.000Z")));
        assert!(app.timeline.follow_head);

        // '[' steps back to the previous era — strictly before the cursor, so
        // sitting exactly on a boundary steps to the era before it.
        app.seek_prompt(false);
        assert_eq!(app.timeline.cursor, Some(ts("2026-06-05T12:00:00.000Z")));
        app.seek_prompt(false);
        assert_eq!(app.timeline.cursor, Some(ts("2026-06-05T11:00:00.000Z")));
        app.seek_prompt(false);
        assert_eq!(app.timeline.cursor, Some(ts("2026-06-05T10:00:00.000Z")));

        // Before the first prompt → clamps to the start (a no-op here).
        app.seek_prompt(false);
        assert_eq!(app.timeline.cursor, Some(ts("2026-06-05T10:00:00.000Z")));
    }

    #[test]
    fn go_live_repins_to_edge_and_folds_to_end() {
        use crate::tailer::ReplayItem;
        let t1: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        let t2: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:10.000Z".parse().unwrap();
        let items = vec![
            ReplayItem::at(Some(t1), meta_update("sub1")),
            ReplayItem::at(Some(t2), meta_update("sub2")),
        ];
        let mut app = App::new("s".into(), Mode::Replay);
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "s".into(),
            items,
            speed: 8.0,
            info: Default::default(),
        });

        // Scrub back into history, then "go live": re-pins and folds to the edge.
        app.seek(t2);
        app.seek("2026-06-05T10:00:05.000Z".parse().unwrap());
        assert!(!app.timeline.follow_head);

        app.go_live();
        assert!(app.timeline.follow_head, "go live re-pins to the edge");
        assert!(
            app.session.agent("sub2").is_some(),
            "go live folds forward to the head"
        );
    }

    #[test]
    fn transport_is_emergent_not_a_mode() {
        use crate::tailer::ReplayItem;
        let t1: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        let t2: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:10.000Z".parse().unwrap();
        let mut app = App::new("s".into(), Mode::Replay);
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "s".into(),
            items: vec![
                ReplayItem::at(Some(t1), meta_update("sub1")),
                ReplayItem::at(Some(t2), meta_update("sub2")),
            ],
            speed: 8.0,
            info: Default::default(),
        });

        // Paced, playing forward, not yet at the edge → Playing.
        assert_eq!(app.transport(), Transport::Playing);

        // At the edge with no fresh append → Idle (a finished/quiet session),
        // NOT "Live" just because it was opened as replay.
        app.seek(t2);
        assert_eq!(app.transport(), Transport::Idle);

        // A genuine append lands at the edge (the file resumed): even a paced
        // replay now reads Live — "live" is following + fresh, not a mode.
        app.handle_ui_event(UiEvent::Batch {
            session_id: "s".into(),
            updates: vec![meta_update("sub3")],
        });
        assert_eq!(app.transport(), Transport::Live);
        assert!(
            app.session.agent("sub3").is_some(),
            "a resumed replay folds the new append"
        );

        // Scrub back off the edge → History.
        app.seek(t1);
        assert_eq!(app.transport(), Transport::History);
    }

    #[test]
    fn space_pauses_and_resumes_from_the_current_cursor() {
        use crate::tailer::ReplayItem;
        let t1: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        let t2: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:10.000Z".parse().unwrap();
        let mut app = App::new("s".into(), Mode::Replay);
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "s".into(),
            items: vec![
                ReplayItem::at(Some(t1), meta_update("sub1")),
                ReplayItem::at(Some(t2), meta_update("sub2")),
            ],
            speed: 8.0,
            info: Default::default(),
        });

        // Scrub into the past → stopped (History).
        app.seek("2026-06-05T10:00:05.000Z".parse().unwrap());
        assert!(!app.timeline.follow_head);

        // Space resumes from HERE (re-engages follow_head, does not jump to edge).
        app.toggle_play_pause();
        assert!(app.timeline.follow_head);
        assert!(!app.is_paused);
        assert_eq!(app.transport(), Transport::Playing);
        assert!(
            app.session.agent("sub2").is_none(),
            "resume plays forward from the cursor, not a jump to the live edge"
        );

        // Space again freezes in place.
        app.toggle_play_pause();
        assert!(app.is_paused);
        assert_eq!(app.transport(), Transport::Paused);
    }

    #[test]
    fn seeking_to_the_edge_clears_pause() {
        use crate::tailer::ReplayItem;
        let t1: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        let t2: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:10.000Z".parse().unwrap();
        let mut app = App::new("s".into(), Mode::Replay);
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "s".into(),
            items: vec![
                ReplayItem::at(Some(t1), meta_update("sub1")),
                ReplayItem::at(Some(t2), meta_update("sub2")),
            ],
            speed: 8.0,
            info: Default::default(),
        });

        // Paused, then drag to the live edge: pause must clear so the playhead
        // rides the edge instead of freezing there.
        app.is_paused = true;
        app.seek(t2);
        assert!(app.timeline.follow_head, "landed at the edge");
        assert!(!app.is_paused, "seeking to the edge clears pause");
        assert_ne!(app.transport(), Transport::Paused, "not frozen at live");
        assert!(
            app.session.agent("sub2").is_some(),
            "seek to the edge folds the whole stream"
        );
    }

    #[test]
    fn transport_paused_outranks_parked_in_the_past() {
        use crate::tailer::ReplayItem;
        let t1: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:00.000Z".parse().unwrap();
        let t2: chrono::DateTime<chrono::Utc> = "2026-06-05T10:00:10.000Z".parse().unwrap();
        let mut app = App::new("s".into(), Mode::Replay);
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "s".into(),
            items: vec![
                ReplayItem::at(Some(t1), meta_update("sub1")),
                ReplayItem::at(Some(t2), meta_update("sub2")),
            ],
            speed: 8.0,
            info: Default::default(),
        });

        // Scrubbed back off the edge → History.
        app.seek(t1);
        assert_eq!(app.transport(), Transport::History);

        // A deliberate pause outranks "parked in the past" — `transport` checks
        // `is_paused` before `follow_head`, so a paused-and-scrubbed view reads
        // Paused, not History. (Pause parks the cursor, so both flags are set.)
        app.is_paused = true;
        assert_eq!(app.transport(), Transport::Paused);
    }

    #[test]
    fn pausing_at_the_live_edge_buffers_appends_instead_of_snapping() {
        let e = |ts: &str| {
            Update::Entry {
            source: crate::tailer::Source::Main,
            entry: crate::transcript::parse_line(&format!(
                r#"{{"type":"user","uuid":"u","timestamp":"{ts}","message":{{"role":"user","content":"x"}}}}"#
            ))
            .unwrap(),
        }
        };
        let mut app = App::new("s".into(), Mode::Live);
        app.handle_ui_event(UiEvent::SessionReset {
            session_id: "s".into(),
        });
        // Ride the live edge at t0.
        app.handle_ui_event(UiEvent::Batch {
            session_id: "s".into(),
            updates: vec![e("2026-06-05T10:00:00.000Z")],
        });
        assert!(app.timeline.follow_head, "live session follows the edge");
        let cursor_at_pause = app.timeline.cursor;
        let folded_at_pause = app.timeline.folded;

        // Pause AT the edge → parks (drops follow_head) so the view holds still.
        app.toggle_play_pause();
        assert!(app.is_paused);
        assert!(
            !app.timeline.follow_head,
            "pause parks the cursor at the edge"
        );
        assert_eq!(app.transport(), Transport::Paused);

        // A new live event lands while paused: it must BUFFER (extend the
        // timeline) without moving the parked cursor or folding into the view —
        // the whole point of live-pause. (Previously it snapped the playhead.)
        app.handle_ui_event(UiEvent::Batch {
            session_id: "s".into(),
            updates: vec![e("2026-06-05T10:00:10.000Z")],
        });
        assert_eq!(
            app.timeline.cursor, cursor_at_pause,
            "paused view holds still"
        );
        assert_eq!(
            app.timeline.folded, folded_at_pause,
            "the new event is buffered, not folded"
        );
        assert!(
            app.timeline.items.len() > app.timeline.folded,
            "and it IS buffered behind the edge"
        );
        assert_eq!(app.transport(), Transport::Paused);

        // Resume → re-engages the edge and catches up through the buffered gap.
        app.toggle_play_pause();
        assert!(app.timeline.follow_head);
        assert!(!app.is_paused);
        app.tick_timeline(std::time::Duration::from_secs(30));
        assert_eq!(
            app.timeline.folded,
            app.timeline.items.len(),
            "resume catches up through what buffered while paused"
        );
    }

    #[test]
    fn out_of_order_live_batch_folds_all_and_keeps_items_sorted() {
        let mut app = App::new("s".into(), Mode::Live);
        app.handle_ui_event(UiEvent::SessionReset {
            session_id: "s".into(),
        });
        let e = |ts: &str| {
            Update::Entry {
            source: crate::tailer::Source::Main,
            entry: crate::transcript::parse_line(&format!(
                r#"{{"type":"user","uuid":"u","timestamp":"{ts}","message":{{"role":"user","content":"x"}}}}"#
            ))
            .unwrap(),
        }
        };
        // An out-of-order batch (file-grouped arrival / backfilled block).
        app.handle_ui_event(UiEvent::Batch {
            session_id: "s".into(),
            updates: vec![
                e("2026-06-05T10:00:05.000Z"),
                e("2026-06-05T10:00:01.000Z"),
                e("2026-06-05T10:00:03.000Z"),
            ],
        });

        // Following the live edge → the whole (order-independent) batch is folded.
        assert!(app.timeline.folded > 0);
        assert_eq!(app.timeline.folded, app.timeline.items.len());
        // And `items` is ts-sorted, so a later backward seek folds the right prefix.
        let got: Vec<_> = app.timeline.items.iter().filter_map(|i| i.ts()).collect();
        let mut want = got.clone();
        want.sort();
        assert_eq!(got, want);
    }
}
