//! The headless text view of a session: what `zoe inspect` prints, and what a
//! provider's golden test compares against. One renderer for both, so the
//! thing a human reads to check a provider is the thing the test checks.

use crate::fact::AgentKind;
use crate::state::info::SessionInfo;
use crate::state::session::{SessionModel, ToolState};

/// The session header: title, the provider's labelled rows, and totals.
pub fn header(model: &SessionModel, info: &SessionInfo) -> String {
    let mut out = String::new();
    let title = info.title.as_deref().unwrap_or("(untitled)");
    out.push_str(&format!("session {} — {title}\n", model.session_id));
    for (label, value) in &info.fields {
        out.push_str(&format!("  {label}: {value}\n"));
    }
    let tallies = info
        .tallies
        .iter()
        .map(|(label, n)| format!("{n} {label}"))
        .collect::<Vec<_>>()
        .join(" · ");
    out.push_str(&format!(
        "  {} agent(s), {} tool call(s)",
        model.agent_count(),
        model.tool_count()
    ));
    if !tallies.is_empty() {
        out.push_str(&format!(" · {tallies}"));
    }
    out.push('\n');
    out
}

/// The agent tree: roots first, children indented underneath, in spawn order.
pub fn agents(model: &SessionModel) -> String {
    let mut out = String::new();
    tree(model, None, 0, &mut out);
    out
}

/// Header, a blank line, then the tree: the whole `inspect` report.
pub fn report(model: &SessionModel, info: &SessionInfo) -> String {
    format!("{}\n{}", header(model, info), agents(model))
}

fn tree(model: &SessionModel, parent: Option<&str>, depth: usize, out: &mut String) {
    for id in model.spawn_order() {
        let Some(agent) = model.agent(id) else {
            continue;
        };
        if agent.parent.as_deref() != parent {
            continue;
        }

        let indent = "  ".repeat(depth + 1);
        let kind = match agent.kind {
            AgentKind::Main => "main",
            AgentKind::Subagent => "subagent",
            AgentKind::Group => "group",
        };
        // Single source: same wording + glyph the cards/panel use.
        let status = agent.status_word();
        let glyph = agent.status.glyph();

        let label = agent
            .agent_type
            .as_deref()
            .or(agent.description.as_deref())
            .unwrap_or(id);

        let (mut ok, mut err, mut pending) = (0u32, 0u32, 0u32);
        for t in agent.tool_calls() {
            match t.state {
                ToolState::Ok => ok += 1,
                ToolState::Err => err += 1,
                ToolState::Pending => pending += 1,
            }
        }

        out.push_str(&format!(
            "{indent}{glyph} [{kind}] {label}  ({status}) — id={id}\n"
        ));
        if let Some(desc) = &agent.description
            && agent.agent_type.is_some()
        {
            out.push_str(&format!("{indent}    {desc}\n"));
        }
        if let Some(model_name) = &agent.model {
            out.push_str(&format!("{indent}    model: {model_name}\n"));
        }
        out.push_str(&format!(
            "{indent}    tools: {} ({ok}✓ {err}✗ {pending}⏳)   tokens: {}\n",
            agent.tool_calls().len(),
            agent.output_tokens
        ));
        // Provenance: what triggered this agent (the panel's `↳ prompt`/`↳ thought`).
        if let Some(ctx) = model.provenance(agent) {
            if let Some(prompt) = model.provenance_prompt(ctx) {
                out.push_str(&format!("{indent}    ↳ prompt: {prompt}\n"));
            }
            if let Some(reasoning) = &ctx.reasoning {
                out.push_str(&format!("{indent}    ↳ thought: {reasoning}\n"));
            }
        }

        // Recurse into this agent's children (groups have subagent children,
        // main has direct subagents + groups).
        tree(model, Some(id), depth + 1, out);
    }
}
