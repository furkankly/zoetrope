//! Incremental projection of [`SessionModel`] onto a rataflow `Flow`.
//!
//! Never rebuilds: per agent, either mutate the existing node content in place
//! via `node_content_mut`, or `add_node` + `add_edge` (duplicate-id `Err` is an
//! idempotent no-op). Nodes are added before their edges. A structural change
//! (node/edge added) marks layout dirty; at sync end we run Sugiyama. Selection
//! survives because node ids are stable and we never clear-and-re-add.

use rataflow::{Edge, Flow, Handle, HandlePosition, Node, Reconnectable, Sugiyama, Theme};
use ratatui::style::Color;

use super::session::{AgentInfo, AgentKind, AgentStatus, SessionModel};
use crate::ui::edges::AgentEdge;
use crate::ui::nodes::{AgentNode, MAIN_NODE_DIMS, SUB_NODE_DIMS};

/// The concrete `Flow` type zoetrope uses: agent-card nodes, step-routed parent
/// edges (no labels — liveness reads from color alone).
pub type AgentFlow = Flow<AgentNode, AgentEdge>;

/// Separator between the session and the within-session agent id in a `Flow`
/// node id.
///
/// Node ids must be unique across the WHOLE flow, but every
/// [`SessionModel`] names its root `"main"` — so a flow holding more than one
/// session needs the session in the id or the roots collide. Session ids are
/// UUIDs and agent ids are hex/workflow tokens, so neither can contain this.
pub const ID_SEP: char = '/';

/// The `Flow` node id for `agent` within `session`.
pub fn node_id(session: &str, agent: &str) -> String {
    format!("{session}{ID_SEP}{agent}")
}

/// Split a `Flow` node id back into `(session, agent)`.
///
/// `None` for an id that was not produced by [`node_id`] — callers treat that
/// as "not one of ours" rather than guessing which half is which.
pub fn split_node_id(id: &str) -> Option<(&str, &str)> {
    id.split_once(ID_SEP)
}

/// Build an empty, fully-configured `Flow` for zoetrope.
///
/// Config: `with_deselect_on_pane_click(false)`, `deselect_on_drag = false`
/// (detail panel persists), `with_min_zoom(0.1)` (Sugiyama trees outgrow the
/// default fit-view limit). Hidden source/target handles for a clean look.
pub fn new_flow() -> AgentFlow {
    // zoetrope identity palette: stock dark base, but `accent` becomes GOLD —
    // selection highlights, done medals, the REPLAY badge. Green stays
    // exclusively "alive" (status), red "failed". Every surface resolves from
    // flow.theme, so this one assignment brands the whole app.
    let mut palette = Theme::Dark.palette();
    palette.accent = Color::Indexed(178);
    let mut flow = Flow::new()
        .with_theme(Theme::Custom(palette))
        .with_deselect_on_pane_click(false)
        // We drive the camera on selection ourselves (a center-glide via
        // `pending_center`), so suppress the library's instant ensure-visible pan
        // — otherwise the two stack into a jump-then-glide on off-screen nodes.
        .with_selection_reveal(rataflow::SelectionReveal::None)
        .with_min_zoom(0.1);
    flow.deselect_on_drag = false;
    flow
}

/// Title line for a node, given its kind and agent type.
///
/// `label` is the session's project name, used for the root card so that a
/// canvas of several sessions names them instead of repeating "claude".
fn node_title(info: &AgentInfo, label: Option<&str>) -> String {
    match info.kind {
        AgentKind::Main => label.unwrap_or("claude").to_string(),
        AgentKind::WorkflowGroup => info
            .agent_type
            .clone()
            .unwrap_or_else(|| "workflow".to_string()),
        AgentKind::Subagent => info
            .agent_type
            .clone()
            .unwrap_or_else(|| "subagent".to_string()),
    }
}

/// Fixed card dimensions for a node kind.
fn node_dims(kind: AgentKind) -> (f64, f64) {
    match kind {
        AgentKind::Main | AgentKind::WorkflowGroup => MAIN_NODE_DIMS,
        AgentKind::Subagent => SUB_NODE_DIMS,
    }
}

/// Whether a node's content already mirrors the agent — allocation-free
/// comparison so unchanged agents skip [`build_content`]'s String clones on
/// every sync (the steady state for almost all agents on almost all ticks).
fn content_matches(info: &AgentInfo, node: &AgentNode, label: Option<&str>) -> bool {
    let title_ok = match info.kind {
        // Compared against the SAME label `node_title` would produce — a
        // literal "claude" here would mismatch every sync once the session is
        // labelled, rebuilding every root card's content on every frame.
        AgentKind::Main => node.title == label.unwrap_or("claude"),
        AgentKind::WorkflowGroup => node.title == info.agent_type.as_deref().unwrap_or("workflow"),
        AgentKind::Subagent => node.title == info.agent_type.as_deref().unwrap_or("subagent"),
    };
    title_ok
        && node.description.as_deref() == info.description.as_deref()
        && node.status == info.status
        && node.tool_count == info.tool_calls.len()
        && node.last_tool.as_deref() == info.last_tool()
        && node.output_tokens == info.output_tokens
        && node.interactive == info.is_interactive()
}

/// Build the [`AgentNode`] content mirrored from an [`AgentInfo`].
fn build_content(info: &AgentInfo, label: Option<&str>) -> AgentNode {
    AgentNode {
        title: node_title(info, label),
        description: info.description.clone(),
        status: info.status,
        tool_count: info.tool_calls.len(),
        last_tool: info.last_tool().map(str::to_string),
        output_tokens: info.output_tokens,
        interactive: info.is_interactive(),
    }
}

/// Horizontal gap between locally-placed siblings (world units).
const LOCAL_H_GAP: f64 = 4.0;
/// Vertical gap below a parent for locally-placed children (world units).
const LOCAL_V_GAP: f64 = 5.0;

/// Incrementally sync `flow` to `model`.
///
/// For each agent in spawn order: mutate the existing node content in place, or
/// add the node (then its parent edge). Updates edge `animated` from target
/// status. New nodes get LOCAL placement (below their parent, offset past
/// siblings) so they land somewhere sensible even without a relayout.
///
/// When `relayout` is true, any structural change ends with a full
/// `Sugiyama::vertical()` pass (which overwrites the local placements). When
/// false — Manual camera: the user owns the view — nothing existing moves;
/// the caller tracks dirtiness and relayouts when the camera re-engages.
/// Returns `true` if structure changed.
pub fn sync(flow: &mut AgentFlow, model: &SessionModel, relayout: bool) -> bool {
    let mut structural = false;

    // First pass: nodes (must exist before their edges).
    for id in &model.spawn_order {
        let Some(info) = model.agent(id) else {
            continue;
        };
        // Model ids are session-local; flow ids are not. This is the ONLY place
        // the two namespaces meet.
        let nid = node_id(&model.session_id, id);
        if let Some(existing) = flow.node_content_mut(&nid) {
            // Steady state: only rebuild (String clones) when something
            // visible changed — the per-second status tick and per-batch
            // syncs walk every agent, and most are unchanged.
            if !content_matches(info, existing, model.label.as_deref()) {
                *existing = build_content(info, model.label.as_deref());
            }
        } else {
            // Sibling index for local placement — computed only for the rare
            // new node; the no-new-nodes steady state skips it entirely.
            let siblings = info
                .parent
                .as_deref()
                .map(|p| {
                    model
                        .spawn_order
                        .iter()
                        .take_while(|x| *x != id)
                        .filter(|x| model.agent(x).and_then(|a| a.parent.as_deref()) == Some(p))
                        .count()
                })
                .unwrap_or(0);
            let content = build_content(info, model.label.as_deref());
            let (w, h) = node_dims(info.kind);
            // Local placement: below the parent, fanned past prior siblings.
            // Overwritten by Sugiyama when `relayout` runs; kept verbatim in
            // Manual so existing nodes never move underneath the user.
            let pos = info
                .parent
                .as_deref()
                .map(|p| node_id(&model.session_id, p))
                .and_then(|p| flow.node(&p))
                .map(|parent| {
                    (
                        parent.position.x + siblings as f64 * (w + LOCAL_H_GAP),
                        parent.position.y + parent.height + LOCAL_V_GAP,
                    )
                })
                // A parentless node is a session root. Layout is never
                // automatic here (see `resync`), so a root cannot wait for a
                // Sugiyama pass to be placed — at the origin it would land on
                // top of the first session's root until the user pressed `r`.
                // Park it clear of everything already on the canvas instead.
                .unwrap_or_else(|| (next_root_x(flow), 0.0));
            // Read-only monitor: nodes are selectable (detail panel) and
            // draggable (manual arrangement) — but never deletable and never
            // connection sources. Enforced at the DTO level, not just the key
            // whitelist, so no input path can mutate the graph.
            let node = Node::new(nid.clone(), pos, (w, h), content)
                .with_deletable(false)
                .with_connectable(false)
                .with_handles(vec![
                    Handle::source(HandlePosition::Bottom).with_hidden(true),
                    Handle::target(HandlePosition::Top).with_hidden(true),
                ]);
            // Duplicate-id is an idempotent no-op; a genuine add is structural.
            if flow.add_node(node).is_ok() {
                structural = true;
            }
        }
    }

    // Second pass: edges from each agent to its parent.
    for id in &model.spawn_order {
        let Some(info) = model.agent(id) else {
            continue;
        };
        let Some(parent) = &info.parent else {
            continue;
        };
        let animated = info.status == AgentStatus::Running;
        let nid = node_id(&model.session_id, id);
        let parent_nid = node_id(&model.session_id, parent);
        let edge_id = edge_id(&nid);
        // Edge already present (the steady state on every sync): just refresh
        // animation — probing via `edge_content_mut` first avoids building a
        // throwaway Edge (three String clones) per agent per sync only for
        // `add_edge` to reject it as a duplicate. Edges carry no selectable
        // meaning in zoetrope (no edge panel), and a stray edge click would pin
        // Follow mode while closing the node panel — a dead state. Fully inert:
        // not selectable, deletable, or reconnectable. Liveness shows as the
        // running color + marching ants, NOT a label — the current tool already
        // shows in the child's chips and detail panel.
        if let Some(content) = flow.edge_content_mut(&edge_id) {
            content.running = animated;
            flow.set_edge_animated(&edge_id, animated);
        } else {
            let edge = Edge::new(edge_id.clone(), parent_nid, nid)
                .with_animated(animated)
                .with_selectable(false)
                .with_deletable(false)
                .with_reconnectable(Reconnectable::None);
            if flow.add_edge(edge).is_ok() {
                structural = true;
            }
            if let Some(content) = flow.edge_content_mut(&edge_id) {
                content.running = animated;
            }
        }
    }

    if structural && relayout {
        self::relayout(flow);
    }
    structural
}

/// Stable id for the (single) parent edge of `child`.
///
/// Keyed by the child alone: every agent has exactly one parent edge, and
/// `sync` never removes edges — so the id must never change once created.
/// Keying on `spawned_by_tool_use` or the parent would orphan a stale edge if
/// either field were filled in after the edge existed (latent today, armed by
/// any future meta re-emission).
fn edge_id(child: &str) -> String {
    format!("e-{child}")
}

/// Remove every node and edge belonging to `session`.
///
/// Used when a session stops being watched: its subtree leaves the canvas
/// without disturbing the others, which a whole-flow rebuild would (it drops
/// every node position the user arranged).
pub fn remove_session(flow: &mut AgentFlow, session: &str) -> bool {
    let doomed: Vec<String> = flow
        .nodes()
        .filter(|n| split_node_id(&n.id).is_some_and(|(s, _)| s == session))
        .map(|n| n.id.clone())
        .collect();
    let removed = !doomed.is_empty();
    for id in doomed {
        // Edges are keyed off the child node id, so removing the node's edge by
        // the same derivation keeps the two in step.
        flow.remove_edge(&edge_id(&id));
        flow.remove_node(&id);
    }
    removed
}

/// Apply the Sugiyama vertical layout to `flow`.
///
/// Split out so it can be called explicitly and unit-tested independently of
/// the per-agent diffing in [`sync`].
pub fn relayout(flow: &mut AgentFlow) {
    flow.apply_layout(Sugiyama::vertical());
    spread_sessions(flow);
}

/// Where a newly-appearing session root goes: clear of everything on the
/// canvas, or the origin when it is the first.
fn next_root_x(flow: &AgentFlow) -> f64 {
    flow.nodes()
        .map(|n| n.position.x + n.width)
        .reduce(f64::max)
        .map_or(0.0, |right| right + SESSION_GUTTER)
}

/// Horizontal gap between two sessions' trees, in world units.
///
/// A whole main card wide ([`MAIN_NODE_DIMS`]), so the gutter between sessions
/// always reads wider than the gaps *inside* one — otherwise two adjacent trees
/// look like one tree with an odd branch.
const SESSION_GUTTER: f64 = MAIN_NODE_DIMS.0;

/// Lay each session's tree out beside the previous one.
///
/// **Required after every Sugiyama pass.** rust-sugiyama lays out each
/// weakly-connected component in its own coordinate space, and rataflow applies
/// each component's coordinates verbatim — it iterates `for (result, _, _)`,
/// discarding exactly the per-component width and height that would let it
/// offset them. With one session that is invisible (one component). With
/// several it stacks every session's tree at the origin, overlapping.
///
/// Sessions keep their first-seen order (the flow's node order), so a session
/// appearing does not reshuffle the ones already on screen; it appends to the
/// right. Vertical positions are left alone: every root is rank 0, so leaving
/// `y` as Sugiyama produced it is what keeps the roots on one line.
fn spread_sessions(flow: &mut AgentFlow) {
    // Per session, in first-seen order: its node ids and horizontal extent.
    let mut order: Vec<String> = Vec::new();
    let mut extent: std::collections::HashMap<String, (f64, f64)> =
        std::collections::HashMap::new();

    for node in flow.nodes() {
        let Some((session, _)) = split_node_id(&node.id) else {
            continue;
        };
        let (left, right) = (node.position.x, node.position.x + node.width);
        match extent.get_mut(session) {
            Some(span) => {
                span.0 = span.0.min(left);
                span.1 = span.1.max(right);
            }
            None => {
                order.push(session.to_string());
                extent.insert(session.to_string(), (left, right));
            }
        }
    }
    if order.len() < 2 {
        return;
    }

    // Shift each session so the trees sit side by side, the first one keeping
    // the origin the single-session layout would have given it.
    let mut cursor = extent.get(&order[0]).map_or(0.0, |e| e.0);
    let mut shift: std::collections::HashMap<String, f64> =
        std::collections::HashMap::with_capacity(order.len());
    for session in &order {
        let Some(&(left, right)) = extent.get(session) else {
            continue;
        };
        shift.insert(session.clone(), cursor - left);
        cursor += (right - left) + SESSION_GUTTER;
    }

    let moved: Vec<(String, (f64, f64))> = flow
        .nodes()
        .filter_map(|node| {
            let (session, _) = split_node_id(&node.id)?;
            let dx = shift.get(session)?;
            Some((node.id.clone(), (node.position.x + dx, node.position.y)))
        })
        .collect();
    flow.set_node_positions(moved.iter().map(|(id, pos)| (id, *pos)));
}

/// Restore saved positions onto whichever nodes still exist (used after a
/// backward-seek rebuild to carry the user's manual arrangement across — and to
/// avoid a layout jump, since a rebuilt subset would otherwise re-place from
/// scratch). Nodes absent from `positions` keep their fresh local placement.
pub fn restore_positions(
    flow: &mut AgentFlow,
    positions: &std::collections::HashMap<String, (f64, f64)>,
) {
    flow.set_node_positions(positions.iter().map(|(id, &pos)| (id, pos)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::SubagentMeta;

    /// The session id every test model uses.
    const S: &str = "s1";

    /// Flow node id for a `s1` agent — the tests reach into the flow, which
    /// speaks session-qualified ids.
    fn n(agent: &str) -> String {
        node_id(S, agent)
    }

    fn meta() -> SubagentMeta {
        SubagentMeta {
            agent_type: Some("guide".into()),
            description: Some("research".into()),
            tool_use_id: Some("ag1".into()),
            stopped_by_user: None,
        }
    }

    /// A model with main + one direct subagent (running).
    fn model_with_subagent() -> SessionModel {
        let mut m = SessionModel::new(S.into());
        m.apply_meta("abc123", None, &meta());
        m
    }

    /// A labelled root names its project instead of saying "claude", and the
    /// label must round-trip through `content_matches` — otherwise every root
    /// card is rebuilt on every sync forever.
    #[test]
    fn a_session_label_names_the_root_card_and_is_stable() {
        let mut model = model_with_subagent();
        model.label = Some("zoetrope".into());
        let mut flow = new_flow();
        sync(&mut flow, &model, false);

        assert_eq!(flow.node_content_mut(&n("main")).unwrap().title, "zoetrope");
        // The subagent keeps its own title — only roots take the label.
        assert_eq!(flow.node_content_mut(&n("abc123")).unwrap().title, "guide");

        let info = model.agent("main").unwrap();
        let node = flow.node_content_mut(&n("main")).unwrap();
        assert!(
            content_matches(info, node, model.label.as_deref()),
            "a labelled root must compare equal to itself"
        );
        // An unlabelled session still reads "claude".
        let plain = SessionModel::new("s3".into());
        let mut flow2 = new_flow();
        sync(&mut flow2, &plain, false);
        assert_eq!(
            flow2
                .node_content_mut(&node_id("s3", "main"))
                .unwrap()
                .title,
            "claude"
        );
    }

    /// Two sessions' trees must not overlap after a layout pass.
    ///
    /// This is the failure rataflow hands us for free: it applies each
    /// connected component's Sugiyama coordinates verbatim, so without
    /// `spread_sessions` both trees land on top of each other at the origin.
    #[test]
    fn sessions_are_laid_out_side_by_side() {
        let mut flow = new_flow();
        let a = model_with_subagent();
        let mut b = SessionModel::new("s2".into());
        b.apply_meta("def456", None, &meta());
        sync(&mut flow, &a, false);
        sync(&mut flow, &b, false);
        relayout(&mut flow);

        // Horizontal extent of one session's nodes.
        let span = |session: &str| {
            flow.nodes()
                .filter(|n| split_node_id(&n.id).is_some_and(|(s, _)| s == session))
                .fold((f64::MAX, f64::MIN), |(lo, hi), n| {
                    (lo.min(n.position.x), hi.max(n.position.x + n.width))
                })
        };
        let (a_lo, a_hi) = span(S);
        let (b_lo, b_hi) = span("s2");

        assert!(a_lo < a_hi && b_lo < b_hi, "both sessions have nodes");
        assert!(
            b_lo >= a_hi,
            "session trees overlap: s1 spans {a_lo}..{a_hi}, s2 spans {b_lo}..{b_hi}"
        );
        assert!(
            b_lo - a_hi >= SESSION_GUTTER,
            "the gutter between sessions must read wider than the gaps inside one"
        );

        // The roots stay on one line — they are all rank 0, and a reader scans
        // across them.
        let root_y = |session: &str| flow.node(&node_id(session, "main")).unwrap().position.y;
        assert_eq!(root_y(S), root_y("s2"));
    }

    /// A session appearing while the canvas is live must not land on top of
    /// one already there. Layout is never automatic, so incremental placement —
    /// not a Sugiyama pass — is what has to get this right.
    #[test]
    fn a_new_session_root_lands_clear_of_the_canvas() {
        let mut flow = new_flow();
        let a = model_with_subagent();
        sync(&mut flow, &a, false);
        relayout(&mut flow);

        // A second session shows up. No relayout — just the incremental sync
        // the live path runs.
        let mut b = SessionModel::new("s2".into());
        b.apply_meta("def456", None, &meta());
        sync(&mut flow, &b, false);

        let a_right = flow
            .nodes()
            .filter(|n| split_node_id(&n.id).is_some_and(|(s, _)| s == S))
            .fold(f64::MIN, |hi, n| hi.max(n.position.x + n.width));
        let b_root = flow.node(&node_id("s2", "main")).unwrap().position.x;
        assert!(
            b_root >= a_right,
            "the new root landed at {b_root}, inside the existing tree ending at {a_right}"
        );
    }

    /// A single session must lay out exactly as it always did — the spreading
    /// pass is a no-op below two sessions, so nothing shifts for the common case.
    #[test]
    fn one_session_is_unaffected_by_spreading() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, false);
        flow.apply_layout(Sugiyama::vertical());
        let before: Vec<_> = flow
            .nodes()
            .map(|n| (n.id.clone(), n.position.x, n.position.y))
            .collect();

        spread_sessions(&mut flow);
        let after: Vec<_> = flow
            .nodes()
            .map(|n| (n.id.clone(), n.position.x, n.position.y))
            .collect();
        assert_eq!(before, after);
    }

    /// Two sessions projected into ONE flow must not fight over a node id.
    ///
    /// Every `SessionModel` names its root `"main"`, so before node ids carried
    /// the session, the second session's root was a duplicate-id no-op: its
    /// agents then hung off the FIRST session's root, silently merging two
    /// unrelated sessions into one tree.
    #[test]
    fn two_sessions_share_a_flow_without_their_roots_colliding() {
        let mut flow = new_flow();
        let a = model_with_subagent();
        let mut b = SessionModel::new("s2".into());
        b.apply_meta("def456", None, &meta());

        sync(&mut flow, &a, false);
        sync(&mut flow, &b, false);

        // Four distinct nodes: two roots, two subagents.
        assert_eq!(flow.nodes().count(), 4);
        for id in [
            node_id(S, "main"),
            node_id(S, "abc123"),
            node_id("s2", "main"),
            node_id("s2", "def456"),
        ] {
            assert!(flow.node(&id).is_some(), "{id} missing");
        }

        // Each subagent's edge lands on ITS OWN root, not the other session's.
        let parent_of = |child: &str| {
            flow.edges()
                .iter()
                .find(|e| e.target == child)
                .map(|e| e.source.clone())
        };
        assert_eq!(parent_of(&node_id(S, "abc123")), Some(node_id(S, "main")));
        assert_eq!(
            parent_of(&node_id("s2", "def456")),
            Some(node_id("s2", "main"))
        );

        // And the ids round-trip, which is what selection relies on.
        assert_eq!(
            split_node_id(&node_id("s2", "def456")),
            Some(("s2", "def456"))
        );
        assert_eq!(split_node_id("unqualified"), None);
    }

    #[test]
    fn sync_creates_nodes_and_edge() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        let structural = sync(&mut flow, &model, true);
        assert!(structural);
        assert!(flow.node_content_mut(&n("main")).is_some());
        assert!(flow.node_content_mut(&n("abc123")).is_some());
        // One edge main -> abc123.
        assert_eq!(flow.edges().len(), 1);
    }

    #[test]
    fn sync_idempotent() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        let first = sync(&mut flow, &model, true);
        assert!(first);
        let node_count = flow.nodes().count();
        let edge_count = flow.edges().len();

        // Applying the same model again adds nothing structural.
        let second = sync(&mut flow, &model, true);
        assert!(!second);
        assert_eq!(flow.nodes().count(), node_count);
        assert_eq!(flow.edges().len(), edge_count);
    }

    #[test]
    fn sync_preserves_selection() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);
        flow.select_node(&n("abc123"));
        assert_eq!(
            flow.selected_nodes().next().map(|node| node.id.clone()),
            Some(n("abc123"))
        );

        // Re-sync after a non-structural change (e.g. a tool call added).
        let mut model2 = model;
        if let Some(a) = model2.agents.get_mut("abc123") {
            a.output_tokens += 100;
        }
        sync(&mut flow, &model2, true);
        assert_eq!(
            flow.selected_nodes().next().map(|node| node.id.clone()),
            Some(n("abc123"))
        );
    }

    #[test]
    fn edge_animation_follows_status() {
        let mut model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);
        // Running subagent -> animated edge.
        let edge_id = edge_id(&n("abc123"));
        let animated = flow
            .edges()
            .iter()
            .find(|e| e.id == edge_id)
            .map(|e| e.animated);
        assert_eq!(animated, Some(true));

        // The edge content mirrors running-ness (drives the distinct color).
        assert!(flow.edge_content_mut(&edge_id).unwrap().running);

        // Mark done, re-sync -> no longer animated, color back to default.
        if let Some(a) = model.agents.get_mut("abc123") {
            a.status = AgentStatus::Done;
        }
        sync(&mut flow, &model, true);
        let animated = flow
            .edges()
            .iter()
            .find(|e| e.id == edge_id)
            .map(|e| e.animated);
        assert_eq!(animated, Some(false));
        assert!(!flow.edge_content_mut(&edge_id).unwrap().running);
    }

    #[test]
    fn graph_is_structurally_read_only() {
        use rataflow::Reconnectable;

        let model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);

        for node in flow.nodes() {
            assert!(node.selectable, "nodes stay selectable (detail panel)");
            assert!(node.draggable, "nodes stay draggable (manual arranging)");
            assert!(!node.deletable, "nodes must not be deletable");
            assert!(!node.connectable, "nodes must not start connections");
        }
        for edge in flow.edges() {
            assert!(!edge.selectable, "edges carry no selectable meaning");
            assert!(!edge.deletable);
            assert_eq!(edge.reconnectable, Reconnectable::None);
        }
    }

    #[test]
    fn manual_mode_local_placement_moves_nothing_existing() {
        let mut model = model_with_subagent();
        let mut flow = new_flow();
        // Initial layout (camera engaged).
        sync(&mut flow, &model, true);
        let main_pos = flow.node(&n("main")).unwrap().position;
        let first_sub = flow.node(&n("abc123")).unwrap().position;

        // Camera now Manual: a second subagent arrives, relayout deferred.
        let meta2 = SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag2".into()),
            stopped_by_user: None,
        };
        model.apply_meta("def456", None, &meta2);
        let structural = sync(&mut flow, &model, false);
        assert!(structural);

        // Nothing existing moved...
        assert_eq!(flow.node(&n("main")).unwrap().position, main_pos);
        assert_eq!(flow.node(&n("abc123")).unwrap().position, first_sub);
        // ...and the newcomer landed below its parent, not at the origin.
        let new_pos = flow.node(&n("def456")).unwrap().position;
        assert!(new_pos.y > main_pos.y, "child placed below parent");
        assert_ne!((new_pos.x, new_pos.y), (0.0, 0.0));
    }

    #[test]
    fn cards_render_into_buffer() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        let model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);
        flow.request_fit_view();

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        (&mut flow).render(area, &mut buf);

        let mut text = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(
            text.contains("claude"),
            "main card title missing from render:\n{text}"
        );
        assert!(
            text.contains("guide"),
            "subagent card title missing from render:\n{text}"
        );
    }
}
