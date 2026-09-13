//! The pi provider: session entries in, [`Fact`](crate::fact::Fact)s out.

pub mod discovery;
pub mod wire;

use chrono::{DateTime, Utc};

use crate::fact::{AgentKind, AgentStatus, Fact, FactKind, Outcome, Statement};
use crate::provider::summary::{short_path, truncate_summary};
use crate::state::session::MAIN_ID;
use wire::{Block, Entry, parse_line};

/// One pi session file being read.
#[derive(Debug, Clone)]
pub struct Stream {
    /// The node this file is by: `main` for the root, the run id for a run
    /// under the root's directory, the session uuid for a forked run beside
    /// it. Comes off the path, so it is known before any line.
    owner: String,
    /// From the header, for relativising paths in summaries.
    cwd: Option<String>,
    /// When this file's own session began. A run forked from its parent
    /// opens with a copy of the parent's history, every entry of it older.
    began: Option<DateTime<Utc>>,
}

impl Stream {
    pub fn new(owner: String) -> Self {
        Stream {
            owner,
            cwd: None,
            began: None,
        }
    }

    /// Parse one line and state what it says. `None` for a blank or
    /// unparsable line, or one that states nothing.
    pub fn push(&mut self, line: &str) -> Option<Statement> {
        let entry = parse_line(line)?;
        let facts = self.facts(&entry);
        if facts.is_empty() {
            return None;
        }
        Some(Statement {
            at: entry.timestamp,
            facts,
        })
    }

    fn is_root(&self) -> bool {
        self.owner == MAIN_ID
    }

    fn facts(&mut self, entry: &Entry) -> Vec<Fact> {
        let mut out = Vec::new();
        if entry.kind == "session" {
            self.cwd = entry.cwd.clone();
            self.began = entry.timestamp;
        } else if !self.is_root()
            && let (Some(at), Some(began)) = (entry.timestamp, self.began)
            && at < began
        {
            // The parent's history, copied into a forked run. Not ours.
            return out;
        }
        if entry.kind == "session" && !self.is_root() {
            out.push(Fact {
                agent: Some(self.owner.clone()),
                ts: entry.timestamp,
                kind: FactKind::Agent {
                    kind: AgentKind::Subagent,
                    parent: Some(MAIN_ID.into()),
                    agent_type: None,
                    description: None,
                    // The spawning call is the parent's record to state.
                    spawned_by: None,
                    interactive: false,
                },
            });
        } else if entry.kind == "session" {
            out.push(Fact {
                agent: Some(self.owner.clone()),
                ts: entry.timestamp,
                kind: FactKind::Agent {
                    kind: AgentKind::Main,
                    parent: None,
                    agent_type: Some("pi".into()),
                    description: None,
                    spawned_by: None,
                    interactive: true,
                },
            });
            if let Some(cwd) = &entry.cwd {
                out.push(Fact {
                    agent: None,
                    ts: None,
                    kind: FactKind::Session {
                        label: "cwd".into(),
                        value: cwd.clone(),
                    },
                });
            }
        } else if entry.kind == "model_change" {
            if let Some(model) = &entry.model_id {
                out.push(Fact {
                    agent: Some(self.owner.clone()),
                    ts: entry.timestamp,
                    kind: FactKind::Model(model.clone()),
                });
            }
        } else if entry.kind == "session_info" && !self.is_root() {
            if let Some(agent) = entry.name.as_deref().and_then(|n| self.run_agent(n)) {
                out.push(Fact {
                    agent: Some(self.owner.clone()),
                    ts: entry.timestamp,
                    kind: FactKind::Label {
                        agent_type: Some(agent.to_string()),
                        description: None,
                    },
                });
            }
        } else if let Some(message) = &entry.message {
            let by = |kind| Fact {
                agent: Some(self.owner.clone()),
                ts: entry.timestamp,
                kind,
            };
            match message.role.as_deref() {
                // A run's user message is its spawner's task, not a person's.
                Some("user") if self.is_root() => {
                    let text = message.content.text();
                    if !text.trim().is_empty() {
                        out.push(by(FactKind::Prompt(text.trim_end().to_string())));
                    }
                }
                Some("assistant") => {
                    let text = message.content.text();
                    let said = if text.trim().is_empty() {
                        message.content.thinking()
                    } else {
                        text
                    };
                    if !said.trim().is_empty() {
                        out.push(by(FactKind::Reasoning(said)));
                    }
                    if let Some(model) = &message.model {
                        out.push(by(FactKind::Model(model.clone())));
                    }
                    if let Some(output) = message.usage.as_ref().and_then(|u| u.output)
                        && output > 0
                    {
                        out.push(by(FactKind::Tokens {
                            output,
                            dedup: None,
                        }));
                    }
                    for block in message.content.blocks() {
                        if block.kind.as_deref() != Some("toolCall") {
                            continue;
                        }
                        let Some(call) = &block.id else { continue };
                        let name = block.name.clone().unwrap_or_default();
                        let spawns = is_spawn(&name, block);
                        out.push(by(FactKind::ToolStart {
                            call: call.clone(),
                            summary: self.summarize(&name, block),
                            name,
                        }));
                        if spawns {
                            out.push(by(FactKind::Spawn { call: call.clone() }));
                        }
                    }
                }
                Some("toolResult") => {
                    if let (Some(call), Some(is_error)) = (&message.tool_call_id, message.is_error)
                    {
                        out.push(by(FactKind::ToolEnd {
                            call: call.clone(),
                            outcome: if is_error { Outcome::Err } else { Outcome::Ok },
                        }));
                    }
                    if message.tool_name.as_deref() == Some("subagent")
                        && let Some(call) = &message.tool_call_id
                    {
                        self.runs_launched(call, &message.details, entry, &mut out);
                    }
                }
                _ => {}
            }
            if out.is_empty() {
                out.push(by(FactKind::Activity));
            }
        }
        out
    }

    /// The agent in a run's session name, `subagent-<agent>-<run>-<step>`.
    /// The run id there is pi-subagents' own; a forked run's node is its
    /// session uuid instead, so the id is dropped rather than matched.
    fn run_agent<'a>(&self, name: &'a str) -> Option<&'a str> {
        let (rest, step) = name.strip_prefix("subagent-")?.rsplit_once('-')?;
        step.parse::<u32>().ok()?;
        let (agent, _run) = rest.rsplit_once('-')?;
        (!agent.is_empty()).then_some(agent)
    }

    /// The children a finished `subagent` call reports. A run's session file
    /// names the child's node (see [`run_of_session_file`]); the call is its
    /// spawner, and its recorded exit code is its end. A result without
    /// `results` (a launch still running in the background, a management
    /// action) names nobody.
    fn runs_launched(
        &self,
        call: &str,
        details: &serde_json::Value,
        entry: &Entry,
        out: &mut Vec<Fact>,
    ) {
        let Some(results) = details.get("results").and_then(|r| r.as_array()) else {
            return;
        };
        for result in results {
            let Some(run) = result
                .get("sessionFile")
                .and_then(|f| f.as_str())
                .and_then(run_of_session_file)
            else {
                continue;
            };
            let about = |kind| Fact {
                agent: Some(run.to_string()),
                ts: entry.timestamp,
                kind,
            };
            out.push(about(FactKind::Agent {
                kind: AgentKind::Subagent,
                parent: Some(self.owner.clone()),
                agent_type: result
                    .get("agent")
                    .and_then(|a| a.as_str())
                    .map(str::to_owned),
                description: None,
                spawned_by: Some(call.to_string()),
                interactive: false,
            }));
            if let Some(code) = result.get("exitCode").and_then(|c| c.as_i64()) {
                out.push(about(FactKind::Ended(if code == 0 {
                    AgentStatus::Done
                } else {
                    AgentStatus::Failed
                })));
            }
        }
    }

    /// One line for a tool call, from pi's built-in tool vocabulary: the
    /// command for `bash`, the file for the file tools.
    fn summarize(&self, name: &str, call: &Block) -> Option<String> {
        match name {
            "bash" => call.argument("command").map(truncate_summary),
            "read" | "write" | "edit" => call.argument("path").map(|p| {
                // pi on Windows records native paths; `short_path` splits on `/`.
                let cwd = self.cwd.as_deref().map(|c| c.replace('\\', "/"));
                short_path(&p.replace('\\', "/"), cwd.as_deref())
            }),
            _ => None,
        }
    }
}

/// Whether a tool call spawns agents. pi-subagents' `subagent` tool launches
/// work unless it is asked for an `action` (list, status, ...) on runs that
/// already exist.
fn is_spawn(name: &str, call: &Block) -> bool {
    name == "subagent" && call.arguments.get("action").is_none()
}

/// A child's node id from its session file path, written with whichever
/// separator the host uses: the run id for `…/<run>/run-<n>/session.jsonl`,
/// the session uuid for a forked run's top-level `…/<timestamp>_<uuid>.jsonl`.
fn run_of_session_file(path: &str) -> Option<&str> {
    let mut parts = path.rsplit(['/', '\\']);
    let file = parts.next()?;
    if file != "session.jsonl" {
        return discovery::session_of_stem(file.strip_suffix(".jsonl")?);
    }
    parts.next()?.strip_prefix("run-")?;
    parts.next().filter(|run| !run.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_header_states_main_and_its_cwd() {
        let mut s = Stream::new(MAIN_ID.into());
        let st = s
            .push(r#"{"type":"session","version":3,"id":"01a09797-7e2c-7002-a725-c0a3455ef1c3","timestamp":"2026-09-12T21:48:02.988Z","cwd":"C:\\p"}"#)
            .unwrap();
        assert_eq!(st.at.unwrap().to_rfc3339(), "2026-09-12T21:48:02.988+00:00");
        assert_eq!(st.facts[0].agent.as_deref(), Some("main"));
        assert!(matches!(
            &st.facts[0].kind,
            FactKind::Agent { kind: AgentKind::Main, interactive: true, agent_type, parent: None, .. }
                if agent_type.as_deref() == Some("pi")
        ));
        assert!(st.facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::Session { label, value } if label == "cwd" && value == "C:\\p"
        )));
    }

    /// A pi-subagents run writes a whole session of its own. Its header makes
    /// the run a subagent of the root, not a second main, and names no
    /// session rows: those are the root's.
    #[test]
    fn run_header_states_a_subagent_of_main() {
        let mut s = Stream::new("3602b259".into());
        let st = s
            .push(r#"{"type":"session","version":3,"id":"01a097a0-e6f7-775b-99c6-2170ee38ef85","timestamp":"2026-09-12T21:58:19.639Z","cwd":"C:\\p"}"#)
            .unwrap();
        assert_eq!(st.facts.len(), 1);
        assert_eq!(st.facts[0].agent.as_deref(), Some("3602b259"));
        assert!(matches!(
            &st.facts[0].kind,
            FactKind::Agent { kind: AgentKind::Subagent, interactive: false, parent, .. }
                if parent.as_deref() == Some("main")
        ));
    }

    /// A user message on the root is a person's prompt, in either content
    /// shape the format allows. On a run it is the spawner's task: activity,
    /// not a prompt.
    #[test]
    fn user_text_is_a_prompt_only_on_the_root() {
        let user = r#"{"type":"message","id":"1f420707","parentId":"99fdee0e","timestamp":"2026-09-12T21:48:32.269Z","message":{"role":"user","content":[{"type":"text","text":"read the plan"}],"timestamp":1789249712264}}"#;
        let mut root = Stream::new(MAIN_ID.into());
        let st = root.push(user).unwrap();
        assert_eq!(st.facts.len(), 1);
        assert_eq!(st.facts[0].agent.as_deref(), Some("main"));
        assert!(matches!(&st.facts[0].kind, FactKind::Prompt(t) if t == "read the plan"));
        let plain = root
            .push(r#"{"type":"message","id":"a","parentId":"b","timestamp":"2026-09-12T21:49:00.000Z","message":{"role":"user","content":"and go"}}"#)
            .unwrap();
        assert!(matches!(&plain.facts[0].kind, FactKind::Prompt(t) if t == "and go"));

        let mut run = Stream::new("3602b259".into());
        let st = run.push(user).unwrap();
        assert_eq!(st.facts.len(), 1);
        assert_eq!(st.facts[0].agent.as_deref(), Some("3602b259"));
        assert!(matches!(st.facts[0].kind, FactKind::Activity));
    }

    /// pi-subagents names a run's session `subagent-<agent>-<run>-<step>`:
    /// the agent is a label for the run, not a node of its own.
    #[test]
    fn run_session_name_labels_the_run() {
        let mut run = Stream::new("23869c1a".into());
        let st = run
            .push(r#"{"type":"session_info","id":"65ea0749","parentId":"1318f01c","timestamp":"2026-08-11T10:36:19.980Z","name":"subagent-code-analysis-scout-23869c1a-1"}"#)
            .unwrap();
        assert_eq!(
            st.facts,
            vec![Fact {
                agent: Some("23869c1a".into()),
                ts: st.at,
                kind: FactKind::Label {
                    agent_type: Some("code-analysis-scout".into()),
                    description: None
                },
            }]
        );
    }

    /// A run started with `context: "fork"` is a top-level session that opens
    /// with a copy of its parent's history, every entry older than its own
    /// header. The copy is the parent's record, already stated by the
    /// parent's file; the run states only what follows it. Its session name
    /// carries a run id that is not its node id, and still labels it.
    #[test]
    fn forked_run_skips_the_copied_history() {
        let mut run = Stream::new("01a097b0-1111-7222-8333-444455556666".into());
        let header = run
            .push(r#"{"type":"session","version":3,"id":"01a097b0-1111-7222-8333-444455556666","timestamp":"2026-09-12T21:59:00.000Z","cwd":"/p","parentSession":"/s/--p--/2026-09-12T21-48-02-988Z_01a09797-7e2c-7002-a725-c0a3455ef1c3.jsonl"}"#)
            .unwrap();
        assert!(matches!(
            &header.facts[0].kind,
            FactKind::Agent {
                kind: AgentKind::Subagent,
                ..
            }
        ));
        assert!(run.push(r#"{"type":"model_change","id":"0b4c973d","parentId":null,"timestamp":"2026-09-12T21:48:06.495Z","provider":"openai-codex","modelId":"gpt-5.6-luna"}"#).is_none());
        assert!(run.push(r#"{"type":"message","id":"ebf295fc","parentId":"1f420707","timestamp":"2026-09-12T21:48:35.396Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_ls","name":"bash","arguments":{"command":"ls"}}],"model":"m","usage":{"output":40}}}"#).is_none());

        let label = run
            .push(r#"{"type":"session_info","id":"22995b18","parentId":"1bab4112","timestamp":"2026-09-12T21:59:02.000Z","name":"subagent-reviewer-7c66204f-1"}"#)
            .unwrap();
        assert_eq!(
            label.facts[0].kind,
            FactKind::Label {
                agent_type: Some("reviewer".into()),
                description: None
            }
        );
        let own = run
            .push(r#"{"type":"message","id":"368849f0","parentId":"6442cfa6","timestamp":"2026-09-12T21:59:10.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_rv","name":"read","arguments":{"path":"/p/a.js"}}],"model":"m","usage":{"output":20}}}"#)
            .unwrap();
        assert!(own.facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::ToolStart { call, .. } if call == "call_rv"
        )));
    }

    /// A forked run's result names its top-level session file: the file's
    /// uuid is the child's node.
    #[test]
    fn subagent_result_joins_a_forked_run_by_its_session_file() {
        let mut s = Stream::new(MAIN_ID.into());
        let done = s
            .push(r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-08-13T11:32:07.000Z","message":{"role":"toolResult","toolCallId":"call_f","toolName":"subagent","content":[],"details":{"results":[{"agent":"reviewer","exitCode":0,"context":"fork","sessionFile":"C:\\Users\\me\\.pi\\agent\\sessions\\--p--\\2026-08-13T11-28-02-664Z_019ffae1-1468-7c46-afe5-06fd1b4ca79a.jsonl"}]},"isError":false}}"#)
            .unwrap();
        assert!(done.facts.iter().any(|f| f.agent.as_deref()
            == Some("019ffae1-1468-7c46-afe5-06fd1b4ca79a")
            && matches!(&f.kind, FactKind::Agent { spawned_by, .. } if spawned_by.as_deref() == Some("call_f"))));
    }

    /// The parent's side of a spawn. A `subagent` call that launches work is a
    /// spawn; one that manages runs (`action`) is not. Its result names each
    /// child's session file, whose run directory is the child's node: that is
    /// the join from call to child, and the child's recorded exit is its end.
    #[test]
    fn subagent_result_joins_each_run_to_its_call() {
        let mut s = Stream::new(MAIN_ID.into());
        let launch = s
            .push(r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-08-16T21:09:50.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_w|fc_w","name":"subagent","arguments":{"workflowScript":"return runs.all([])"}},{"type":"toolCall","id":"call_l|fc_l","name":"subagent","arguments":{"action":"list"}}],"model":"m","usage":{"output":5}}}"#)
            .unwrap();
        let spawns: Vec<&str> = launch
            .facts
            .iter()
            .filter_map(|f| match &f.kind {
                FactKind::Spawn { call } => Some(call.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(spawns, vec!["call_w|fc_w"]);

        let done = s
            .push(r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-08-16T21:13:27.000Z","message":{"role":"toolResult","toolCallId":"call_w|fc_w","toolName":"subagent","content":[{"type":"text","text":"done"}],"details":{"mode":"workflow","results":[{"agent":"code-analysis.scout","exitCode":0,"sessionFile":"C:\\Users\\me\\.pi\\agent\\sessions\\--p--\\2026-08-16T21-08-53-060Z_01a00c67-eec4-79cf-885f-b7f878f79f5c\\95131924\\run-0\\session.jsonl"},{"agent":"reviewer","exitCode":1,"sessionFile":"/h/.pi/agent/sessions/--p--/2026-08-16T21-08-53-060Z_01a00c67-eec4-79cf-885f-b7f878f79f5c/8cd6a29a/run-0/session.jsonl"}]},"isError":false}}"#)
            .unwrap();
        let about = |run: &str| -> Vec<&FactKind> {
            done.facts
                .iter()
                .filter(|f| f.agent.as_deref() == Some(run))
                .map(|f| &f.kind)
                .collect()
        };
        let scout = about("95131924");
        assert!(matches!(
            scout[0],
            FactKind::Agent { kind: AgentKind::Subagent, parent, spawned_by, agent_type, interactive: false, .. }
                if parent.as_deref() == Some("main")
                    && spawned_by.as_deref() == Some("call_w|fc_w")
                    && agent_type.as_deref() == Some("code-analysis.scout")
        ));
        assert_eq!(scout[1], &FactKind::Ended(AgentStatus::Done));
        assert_eq!(about("8cd6a29a")[1], &FactKind::Ended(AgentStatus::Failed));
        assert!(done.facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "call_w|fc_w"
        )));
    }

    /// An assistant message states what it said (its text, else its thinking),
    /// the model that said it, and the output tokens it cost. A `model_change`
    /// entry states the model too.
    #[test]
    fn assistant_states_words_model_and_tokens() {
        let mut s = Stream::new(MAIN_ID.into());
        let st = s
            .push(r#"{"type":"message","id":"5469341e","parentId":"9c96850b","timestamp":"2026-09-12T21:50:47.516Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"**Weighing it**"},{"type":"text","text":"Confirmed and recorded."}],"provider":"openai-codex","model":"gpt-5.6-luna","usage":{"input":12505,"output":49,"totalTokens":12554},"stopReason":"stop"}}"#)
            .unwrap();
        let kinds: Vec<&FactKind> = st.facts.iter().map(|f| &f.kind).collect();
        assert!(kinds.contains(&&FactKind::Reasoning("Confirmed and recorded.".into())));
        assert!(kinds.contains(&&FactKind::Model("gpt-5.6-luna".into())));
        assert!(kinds.contains(&&FactKind::Tokens {
            output: 49,
            dedup: None
        }));

        let thinking_only = s
            .push(r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-09-12T21:50:48.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"**Planning inspection**"}],"model":"gpt-5.6-luna","usage":{"output":0}}}"#)
            .unwrap();
        assert!(
            thinking_only
                .facts
                .iter()
                .any(|f| f.kind == FactKind::Reasoning("**Planning inspection**".into()))
        );
        assert!(
            !thinking_only
                .facts
                .iter()
                .any(|f| matches!(f.kind, FactKind::Tokens { .. })),
            "no tokens stated for an empty turn"
        );

        let change = s
            .push(r#"{"type":"model_change","id":"0b4c973d","parentId":null,"timestamp":"2026-09-12T21:48:06.495Z","provider":"openai-codex","modelId":"gpt-5.6-terra"}"#)
            .unwrap();
        assert_eq!(
            change.facts[0].kind,
            FactKind::Model("gpt-5.6-terra".into())
        );
    }

    /// Every `toolCall` block starts a call; its `toolResult` ends it with the
    /// outcome the format records in `isError`.
    #[test]
    fn tool_calls_start_and_results_end_them() {
        let mut s = Stream::new(MAIN_ID.into());
        s.push(r#"{"type":"session","version":3,"id":"u","timestamp":"2026-09-12T21:48:02.988Z","cwd":"C:\\p"}"#);
        let calls = s
            .push(r#"{"type":"message","id":"9e50b6b2","parentId":"1b2c7649","timestamp":"2026-09-12T21:48:48.214Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"**Reading**"},{"type":"toolCall","id":"call_1|fc_1","name":"bash","arguments":{"command":"ls -la"}},{"type":"toolCall","id":"call_2|fc_2","name":"read","arguments":{"path":"C:\\p\\plan.md"}}],"model":"gpt-5.6-luna","usage":{"input":10,"output":49,"totalTokens":59},"stopReason":"toolUse"}}"#)
            .unwrap();
        let starts: Vec<(&str, &str, Option<&str>)> = calls
            .facts
            .iter()
            .filter_map(|f| match &f.kind {
                FactKind::ToolStart {
                    call,
                    name,
                    summary,
                } => Some((call.as_str(), name.as_str(), summary.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![
                ("call_1|fc_1", "bash", Some("ls -la")),
                ("call_2|fc_2", "read", Some("plan.md")),
            ]
        );

        let ok = s
            .push(r#"{"type":"message","id":"1bab4112","parentId":"9e50b6b2","timestamp":"2026-09-12T21:48:49.000Z","message":{"role":"toolResult","toolCallId":"call_1|fc_1","toolName":"bash","content":[{"type":"text","text":"total 16"}],"isError":false}}"#)
            .unwrap();
        assert_eq!(
            ok.facts,
            vec![Fact {
                agent: Some("main".into()),
                ts: ok.at,
                kind: FactKind::ToolEnd {
                    call: "call_1|fc_1".into(),
                    outcome: Outcome::Ok
                },
            }]
        );
        let err = s
            .push(r#"{"type":"message","id":"2c","parentId":"1bab4112","timestamp":"2026-09-12T21:48:50.000Z","message":{"role":"toolResult","toolCallId":"call_2|fc_2","toolName":"read","content":[{"type":"text","text":"ENOENT"}],"isError":true}}"#)
            .unwrap();
        assert!(matches!(
            &err.facts[0].kind,
            FactKind::ToolEnd { call, outcome: Outcome::Err } if call == "call_2|fc_2"
        ));
    }
}
