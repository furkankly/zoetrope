//! Application state.
//!
//! [`App`] owns the rataflow `Flow` (the rendered graph), the pure
//! [`SessionModel`] (domain truth), and all UI state (pause, mode, scroll, the
//! current session id, the follow camera, and the chip tray).
//! [`App::handle_ui_event`] folds tailer events into the model and re-syncs the
//! graph; events for stale sessions are dropped via [`App::is_current`].

pub mod graph;
pub mod info;
pub mod session;
pub mod timeline;

use self::graph::AgentFlow;
pub use self::info::SessionInfo;
use self::session::SessionModel;
use self::timeline::Timeline;
use crate::i18n::Locale;
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

/// The central application state, owned by the single UI task.
pub struct App {
    /// The rendered flow graph (agent cards + step edges).
    pub flow: AgentFlow,
    /// The pure domain model the graph is projected from.
    pub session: SessionModel,
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
    /// UI language for every surface rendered from the App (status bar,
    /// overlays, cards, panel). The native frontend sets it from
    /// `--lang`/environment detection; the browser frontend from
    /// `navigator.language` (via `zoetrope_set_lang`). `L` cycles it at
    /// runtime.
    pub locale: crate::i18n::Locale,
}

impl App {
    /// Construct the initial app state for a session id and mode, with an empty
    /// configured flow and a fresh model.
    pub fn new(session_id: String, mode: Mode) -> Self {
        App {
            flow: graph::new_flow(),
            session: SessionModel::new(session_id.clone()),
            mode,
            locale: Locale::default(),
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
            scrubber_tally: None,
            era_cache: None,
            last_batch_at: None,
            session_info: SessionInfo::default(),
            show_info: false,
            pending_center: None,
            pending_seek: None,
        }
    }

    /// Switch the UI language at runtime. The locale is stamped onto every
    /// `AgentNode` at graph sync, so an in-place refresh of the existing cards
    /// is needed for the change to show before the next structural change —
    /// a fresh `App` (the wasm loaders) just starts in the new locale.
    pub fn set_locale(&mut self, locale: Locale) {
        self.locale = locale;
        let ids: Vec<String> = self.flow.nodes().map(|n| n.id.clone()).collect();
        for id in ids {
            if let Some(node) = self.flow.node_content_mut(&id) {
                node.locale = locale;
            }
        }
    }

    /// Cycle to the next language (the `L` key).
    pub fn cycle_locale(&mut self) {
        let next = self.locale.next();
        self.set_locale(next);
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
    /// Drops events whose `session_id` is not [current](Self::is_current),
    /// applies batched updates to the [`SessionModel`], re-syncs the graph, and
    /// re-fits the view on structural changes while the follow camera is
    /// engaged. Switches the current session id on reset.
    pub fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Batch {
                session_id,
                updates,
            } => {
                if !self.is_current(&session_id) {
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
                // Truncation/rotation/switch: adopt the new id and rebuild
                // from scratch so stale nodes don't linger. The tailer's
                // initial ANNOUNCE arrives as a same-id reset on a still-empty
                // graph — there is nothing to wipe, and resetting view state
                // there would clobber a camera/pin choice the user made while
                // waiting for the session to appear.
                let genuine = !self.is_current(&session_id) || self.flow.nodes().count() > 0;
                self.current_session_id = session_id.clone();
                self.session = SessionModel::new(session_id);
                self.flow = graph::new_flow();
                // A reset is a fresh live timeline (only live emits resets — the
                // initial announce, truncation, or auto-switch). Replay arrives
                // via ReplayLoaded, never a reset.
                self.timeline = Timeline::new();
                if genuine {
                    self.camera = Camera::Overview;
                    self.detail_scroll = 0;
                    self.detail_follow = true;
                    self.layout_dirty = false;
                    self.chips.clear();
                    self.camera_glide = None;
                }
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
        let mut structural = false;
        for i in self.timeline.folded..target {
            structural |= self.session.apply_update(&self.timeline.items[i].update);
        }
        self.timeline.folded = target;
        self.commit_fold(structural);
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
            for i in self.timeline.folded..target {
                self.session.apply_update(&self.timeline.items[i].update);
            }
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
        // Snapshot view state to carry across the wipe.
        let selected = self.selected_agent_id();
        // The camera/viewport (pan + zoom) is the user's — a fresh `Flow` would
        // reset it to the origin, snapping the graph on every backward seek even
        // in Manual/Overview. Carry it across.
        let viewport = self.flow.viewport;
        let positions: std::collections::HashMap<String, (f64, f64)> = self
            .flow
            .nodes()
            .map(|n| (n.id.clone(), (n.position.x, n.position.y)))
            .collect();

        // Fresh model + flow, re-fold the prefix.
        self.session = SessionModel::new(self.current_session_id.clone());
        self.flow = graph::new_flow();
        for i in 0..target {
            self.session.apply_update(&self.timeline.items[i].update);
        }
        self.timeline.folded = target;
        self.resync();

        // Carry the arrangement + selection + viewport across so a seek doesn't
        // jump the layout or the camera.
        graph::restore_positions(&mut self.flow, &positions);
        self.flow.viewport = viewport;
        if let Some(id) = selected {
            self.flow.select_node(&id);
        }
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
        let structural = graph::sync(&mut self.flow, &self.session, false, self.locale);
        if structural {
            self.layout_dirty = true;
        }
        structural
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
            graph::sync(&mut self.flow, &self.session, false, self.locale);
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
        let Some(id) = self.session.last_active_agent_id() else {
            return;
        };
        self.center_node(&id, true); // Follow: clamp zoom for readability.
        // In Follow the panel narrates the followed agent. `select_node` is quiet
        // (no SelectionChanged), so this never trips the drop-Follow detection in
        // the handler — a user selection, which does fire it, drops to Manual and
        // stops this auto-narration entirely.
        if self.selected_agent_id().as_deref() != Some(id.as_str()) {
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
        self.flow.selected_nodes().next().map(|n| n.id.clone())
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

    #[test]
    fn auto_switch_reset_adopts_new_session_id() {
        // The auto-switch path emits SessionReset stamped with the NEW id, then
        // the post-switch tailer emits Batches under the NEW id. The App must
        // adopt the new id so those batches are not dropped by is_current.
        let mut app = App::new("OLD".into(), Mode::Live);

        // Reset carrying the NEW session id (auto-switch signal).
        app.handle_ui_event(UiEvent::SessionReset {
            session_id: "NEW".into(),
        });
        assert_eq!(app.current_session_id, "NEW");

        // A batch from the new session must be accepted, not dropped.
        app.handle_ui_event(UiEvent::Batch {
            session_id: "NEW".into(),
            updates: vec![meta_update("newsub")],
        });
        assert!(
            app.session.agent("newsub").is_some(),
            "new-session subagent must be present after auto-switch"
        );
    }

    #[test]
    fn camera_defaults_to_overview_and_resets() {
        let mut app = App::new("s".into(), Mode::Live);
        assert_eq!(app.camera, Camera::Overview);

        app.camera = Camera::Manual; // user took the camera…
        app.handle_ui_event(UiEvent::SessionReset {
            session_id: "s2".into(),
        });
        // …but a fresh session re-engages the default.
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
        assert!(app.flow.node("sub1").is_some(), "the subagent node exists");

        // Zoom OUT past the readable band — the clamp WOULD want to snap it in.
        app.flow.zoom_to(0.3);
        let zoom = app.flow.viewport.zoom;
        assert!(zoom < FOLLOW_ZOOM);

        // Manual spatial-nav (clamp_zoom = false) → pans, leaves the zoom alone.
        app.center_node("sub1", false);
        assert_eq!(
            app.flow.viewport.zoom, zoom,
            "arrow-nav must not touch the user's zoom"
        );

        // Follow / explicit center (clamp_zoom = true) → snaps the zoom in for
        // readability — the movement that used to ride along with every arrow.
        app.center_node("sub1", true);
        assert!(
            app.flow.viewport.zoom > zoom,
            "Follow still bumps zoom for readability"
        );
    }

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
        app.flow.select_node("sub1");

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
