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
use crate::i18n::Locale;
use crate::ui::edges::AgentEdge;
use crate::ui::nodes::{AgentNode, MAIN_NODE_DIMS, SUB_NODE_DIMS};

/// The concrete `Flow` type zoetrope uses: agent-card nodes, step-routed parent
/// edges (no labels — liveness reads from color alone).
pub type AgentFlow = Flow<AgentNode, AgentEdge>;

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
fn node_title(info: &AgentInfo) -> String {
    match info.kind {
        AgentKind::Main => "claude".to_string(),
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
/// Locale is compared too: a language switch must rebuild every card.
fn content_matches(info: &AgentInfo, node: &AgentNode, locale: Locale) -> bool {
    let title_ok = match info.kind {
        AgentKind::Main => node.title == "claude",
        AgentKind::WorkflowGroup => node.title == info.agent_type.as_deref().unwrap_or("workflow"),
        AgentKind::Subagent => node.title == info.agent_type.as_deref().unwrap_or("subagent"),
    };
    title_ok
        && node.locale == locale
        && node.description.as_deref() == info.description.as_deref()
        && node.status == info.status
        && node.tool_count == info.tool_calls.len()
        && node.last_tool.as_deref() == info.last_tool()
        && node.output_tokens == info.output_tokens
        && node.interactive == info.is_interactive()
}

/// Build the [`AgentNode`] content mirrored from an [`AgentInfo`].
fn build_content(info: &AgentInfo, locale: Locale) -> AgentNode {
    AgentNode {
        title: node_title(info),
        description: info.description.clone(),
        status: info.status,
        tool_count: info.tool_calls.len(),
        last_tool: info.last_tool().map(str::to_string),
        output_tokens: info.output_tokens,
        interactive: info.is_interactive(),
        locale,
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
/// `locale` is stamped onto every card (the render layer has no App access).
/// Returns `true` if structure changed.
pub fn sync(flow: &mut AgentFlow, model: &SessionModel, relayout: bool, locale: Locale) -> bool {
    let mut structural = false;

    // First pass: nodes (must exist before their edges).
    for id in &model.spawn_order {
        let Some(info) = model.agent(id) else {
            continue;
        };
        if let Some(existing) = flow.node_content_mut(id) {
            // Steady state: only rebuild (String clones) when something
            // visible changed — the per-second status tick and per-batch
            // syncs walk every agent, and most are unchanged.
            if !content_matches(info, existing, locale) {
                *existing = build_content(info, locale);
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
            let content = build_content(info, locale);
            let (w, h) = node_dims(info.kind);
            // Local placement: below the parent, fanned past prior siblings.
            // Overwritten by Sugiyama when `relayout` runs; kept verbatim in
            // Manual so existing nodes never move underneath the user.
            let pos = info
                .parent
                .as_deref()
                .and_then(|p| flow.node(p))
                .map(|parent| {
                    (
                        parent.position.x + siblings as f64 * (w + LOCAL_H_GAP),
                        parent.position.y + parent.height + LOCAL_V_GAP,
                    )
                })
                .unwrap_or((0.0, 0.0));
            // Read-only monitor: nodes are selectable (detail panel) and
            // draggable (manual arrangement) — but never deletable and never
            // connection sources. Enforced at the DTO level, not just the key
            // whitelist, so no input path can mutate the graph.
            let node = Node::new(id.clone(), pos, (w, h), content)
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
        let edge_id = edge_id(id);
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
            let edge = Edge::new(edge_id.clone(), parent.clone(), id.clone())
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

/// Apply the Sugiyama vertical layout to `flow`.
///
/// Split out so it can be called explicitly and unit-tested independently of
/// the per-agent diffing in [`sync`].
pub fn relayout(flow: &mut AgentFlow) {
    flow.apply_layout(Sugiyama::vertical());
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

    /// A model with main + one direct subagent (running).
    fn model_with_subagent() -> SessionModel {
        let mut m = SessionModel::new("s1".into());
        let meta = SubagentMeta {
            agent_type: Some("guide".into()),
            description: Some("research".into()),
            tool_use_id: Some("ag1".into()),
            stopped_by_user: None,
        };
        m.apply_meta("abc123", None, &meta);
        m
    }

    #[test]
    fn sync_creates_nodes_and_edge() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        let structural = sync(&mut flow, &model, true, Locale::default());
        assert!(structural);
        assert!(flow.node_content_mut("main").is_some());
        assert!(flow.node_content_mut("abc123").is_some());
        // One edge main -> abc123.
        assert_eq!(flow.edges().len(), 1);
    }

    #[test]
    fn sync_idempotent() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        let first = sync(&mut flow, &model, true, Locale::default());
        assert!(first);
        let node_count = flow.nodes().count();
        let edge_count = flow.edges().len();

        // Applying the same model again adds nothing structural.
        let second = sync(&mut flow, &model, true, Locale::default());
        assert!(!second);
        assert_eq!(flow.nodes().count(), node_count);
        assert_eq!(flow.edges().len(), edge_count);
    }

    #[test]
    fn sync_preserves_selection() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true, Locale::default());
        flow.select_node("abc123");
        assert_eq!(
            flow.selected_nodes().next().map(|n| n.id.clone()),
            Some("abc123".to_string())
        );

        // Re-sync after a non-structural change (e.g. a tool call added).
        let mut model2 = model;
        if let Some(a) = model2.agents.get_mut("abc123") {
            a.output_tokens += 100;
        }
        sync(&mut flow, &model2, true, Locale::default());
        assert_eq!(
            flow.selected_nodes().next().map(|n| n.id.clone()),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn edge_animation_follows_status() {
        let mut model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true, Locale::default());
        // Running subagent -> animated edge.
        let edge_id = edge_id("abc123");
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
        sync(&mut flow, &model, true, Locale::default());
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
        sync(&mut flow, &model, true, Locale::default());

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
        sync(&mut flow, &model, true, Locale::default());
        let main_pos = flow.node("main").unwrap().position;
        let first_sub = flow.node("abc123").unwrap().position;

        // Camera now Manual: a second subagent arrives, relayout deferred.
        let meta2 = SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag2".into()),
            stopped_by_user: None,
        };
        model.apply_meta("def456", None, &meta2);
        let structural = sync(&mut flow, &model, false, Locale::default());
        assert!(structural);

        // Nothing existing moved...
        assert_eq!(flow.node("main").unwrap().position, main_pos);
        assert_eq!(flow.node("abc123").unwrap().position, first_sub);
        // ...and the newcomer landed below its parent, not at the origin.
        let new_pos = flow.node("def456").unwrap().position;
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
        sync(&mut flow, &model, true, Locale::default());
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
