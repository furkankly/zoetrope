//! Deterministic synthetic session generator for the benchmarks.
//!
//! Produces transcripts in the on-disk shape (`<uuid>.jsonl` + `subagents/`
//! sidecars) without touching `~/.claude/projects`: benches must be
//! reproducible on any machine and must never read a real session. Every line
//! is built with `serde_json::json!`, so the output parses by construction.
//!
//! Shape and volume are calibrated against the aggregate statistics of real
//! transcripts: a line is a couple of kilobytes, spawns land every few prompt
//! eras, and the subagent sidecars outweigh the main transcript. The scales are
//! deliberately spread so a result can be read as a trend rather than a point —
//! see `Spec::SMALL/MEDIUM/LARGE`.

#![allow(dead_code)]

use serde_json::json;

/// Filler text of exactly `n` bytes — JSON-safe ASCII, no escapes, so a line's
/// serialized size is predictable.
fn filler(n: usize) -> String {
    const WORDS: [&str; 8] = [
        "resolve ",
        "the ",
        "handler ",
        "before ",
        "folding ",
        "into ",
        "the model ",
        "again ",
    ];
    let mut s = String::with_capacity(n + 16);
    let mut i = 0;
    while s.len() < n {
        s.push_str(WORDS[i % WORDS.len()]);
        i += 1;
    }
    s.truncate(n);
    s
}

/// ISO-8601 stamp `t` seconds after a fixed epoch. Monotone across a session.
fn ts(t: u64) -> String {
    let base = chrono::DateTime::parse_from_rfc3339("2026-06-01T09:00:00.000Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    (base + chrono::Duration::seconds(t as i64))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// The volume knobs of a generated session.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// Human prompts — the era spine.
    pub eras: usize,
    /// Assistant turns per era.
    pub turns_per_era: usize,
    /// `tool_use` blocks per assistant turn (each gets a `tool_result`).
    pub tools_per_turn: usize,
    /// Direct `Agent` spawns (one subagent sidecar each).
    pub subagents: usize,
    /// Assistant turns inside each subagent transcript.
    pub sub_turns: usize,
    /// `Workflow` launches (one group + `wf_agents` sidecars + a journal each).
    pub workflows: usize,
    /// Subagents per workflow.
    pub wf_agents: usize,
    /// Filler bytes per text/thinking/result payload — the line-size dial.
    pub payload: usize,
}

impl Spec {
    /// A short session: 222 lines / ~0.3 MB. Generation is deterministic, so
    /// these figures describe the generator, not a measurement.
    pub const SMALL: Spec = Spec {
        eras: 12,
        turns_per_era: 3,
        tools_per_turn: 3,
        subagents: 4,
        sub_turns: 8,
        workflows: 1,
        wf_agents: 3,
        payload: 900,
    };
    /// A long working session: 2,391 lines / ~4.6 MB.
    pub const MEDIUM: Spec = Spec {
        eras: 60,
        turns_per_era: 5,
        tools_per_turn: 4,
        subagents: 20,
        sub_turns: 20,
        workflows: 4,
        wf_agents: 5,
        payload: 1100,
    };
    /// A pathologically large session: 31,371 lines / ~67 MB, the order of the
    /// largest single transcripts that occur in practice.
    pub const LARGE: Spec = Spec {
        eras: 450,
        turns_per_era: 6,
        tools_per_turn: 5,
        subagents: 130,
        sub_turns: 45,
        workflows: 18,
        wf_agents: 8,
        payload: 1300,
    };
}

/// One generated subagent sidecar: transcript text + `meta.json` text.
pub struct Sidecar {
    pub agent_id: String,
    pub meta: String,
    pub transcript: String,
    pub workflow: Option<String>,
    pub journal: bool,
}

/// A generated session: the main transcript plus every sidecar.
pub struct Session {
    pub main: String,
    pub sidecars: Vec<Sidecar>,
}

impl Session {
    /// Total bytes across every file — what a cold parse actually reads.
    pub fn bytes(&self) -> usize {
        self.main.len()
            + self
                .sidecars
                .iter()
                .map(|s| s.transcript.len() + s.meta.len())
                .sum::<usize>()
    }

    /// Total lines across every file.
    pub fn lines(&self) -> usize {
        self.main.lines().count()
            + self
                .sidecars
                .iter()
                .map(|s| s.transcript.lines().count())
                .sum::<usize>()
    }

    /// Borrowed view in the shape `tailer::replay_from_session` expects.
    pub fn demo_subagents(&self) -> Vec<zoetrope::tailer::DemoSubagent<'_>> {
        self.sidecars
            .iter()
            .map(|s| zoetrope::tailer::DemoSubagent {
                agent_id: &s.agent_id,
                meta: &s.meta,
                transcript: &s.transcript,
                workflow: s.workflow.as_deref(),
                journal: s.journal,
            })
            .collect()
    }
}

/// 17-hex agent id, the width real `agent-<id>.jsonl` filenames use.
fn agent_id(n: usize) -> String {
    format!("{n:017x}")
}

/// Build a full synthetic session from `spec`.
///
/// The main transcript interleaves prompt eras with spawns so the model
/// exercises every join it has: era attribution, `Agent`/`Workflow` spawn
/// context, `tool_result` completion, and the workflow journal's `result`
/// ledger.
pub fn session(spec: Spec) -> Session {
    let mut main = String::new();
    let mut sidecars = Vec::new();
    let mut t = 0u64;
    let mut uuid = 0usize;
    let mut tool_seq = 0usize;
    let next_uuid = |uuid: &mut usize| {
        *uuid += 1;
        format!("u{uuid:07}")
    };

    // Flat session metadata (untimed — routed to SessionInfo, off the timeline).
    for line in [
        json!({"type":"ai-title","aiTitle":"Synthetic benchmark session","sessionId":"bench"}),
        json!({"type":"mode","mode":"normal","sessionId":"bench"}),
        json!({"type":"permission-mode","permissionMode":"acceptEdits","sessionId":"bench"}),
    ] {
        main.push_str(&line.to_string());
        main.push('\n');
    }

    // Spawns are spread evenly across the eras rather than clustered at the
    // front, so a mid-timeline seek always lands inside live subagent work.
    let total_spawns = spec.subagents + spec.workflows;
    let spawn_every = spec
        .eras
        .checked_div(total_spawns)
        .map_or(usize::MAX, |n| n.max(1));
    let mut spawned_subs = 0usize;
    let mut spawned_wfs = 0usize;

    let mut prev = String::new();
    for era in 0..spec.eras {
        // --- the human prompt: an era boundary ---
        let id = next_uuid(&mut uuid);
        t += 30;
        main.push_str(
            &json!({
                "type":"user","uuid":id,
                "parentUuid": if prev.is_empty() { serde_json::Value::Null } else { json!(prev) },
                "timestamp": ts(t),
                "sessionId":"bench","cwd":"/home/dev/project",
                "origin":{"kind":"human"},
                "message":{"role":"user","content":format!("era {era}: {}", filler(spec.payload / 4))}
            })
            .to_string(),
        );
        main.push('\n');
        prev = id;

        // --- assistant turns, each answered by a tool_result user turn ---
        for _ in 0..spec.turns_per_era {
            let a_id = next_uuid(&mut uuid);
            t += 7;
            let tools: Vec<_> = (0..spec.tools_per_turn)
                .map(|_| {
                    tool_seq += 1;
                    let tid = format!("toolu_{tool_seq:08}");
                    (
                        tid.clone(),
                        json!({"type":"tool_use","id":tid,"name":"Read",
                               "input":{"file_path":"/home/dev/project/src/state/mod.rs","offset":1,"limit":200}}),
                    )
                })
                .collect();
            let mut content = vec![
                json!({"type":"thinking","thinking":filler(spec.payload),"signature":"sig"}),
                json!({"type":"text","text":filler(spec.payload / 2)}),
            ];
            content.extend(tools.iter().map(|(_, b)| b.clone()));
            main.push_str(
                &json!({
                    "type":"assistant","uuid":a_id,"parentUuid":prev,"timestamp":ts(t),
                    "sessionId":"bench","requestId":"req_bench",
                    "message":{"role":"assistant","model":"claude-opus-5","stop_reason":"tool_use",
                               "content":content,
                               "usage":{"input_tokens":12000,"output_tokens":800,
                                        "cache_read_input_tokens":9000}}
                })
                .to_string(),
            );
            main.push('\n');
            prev = a_id;

            let r_id = next_uuid(&mut uuid);
            t += 4;
            // One result per call; every 9th fails, so failure markers land on
            // the scrubber the way a real session's do.
            let blocks: Vec<_> = tools
                .iter()
                .enumerate()
                .map(|(i, (tid, _))| {
                    json!({"type":"tool_result","tool_use_id":tid,
                           "is_error": i % 9 == 8,
                           "content": filler(spec.payload)})
                })
                .collect();
            main.push_str(
                &json!({
                    "type":"user","uuid":r_id,"parentUuid":prev,"timestamp":ts(t),
                    "sessionId":"bench","message":{"role":"user","content":blocks}
                })
                .to_string(),
            );
            main.push('\n');
            prev = r_id;
        }

        // --- periodic spawn: a direct subagent, then a workflow ---
        if era % spawn_every == 0 {
            if spawned_subs < spec.subagents {
                let n = spawned_subs;
                spawned_subs += 1;
                tool_seq += 1;
                let tid = format!("toolu_{tool_seq:08}");
                let aid = agent_id(0x1000 + n);
                let a_id = next_uuid(&mut uuid);
                t += 6;
                main.push_str(
                    &json!({
                        "type":"assistant","uuid":a_id,"parentUuid":prev,"timestamp":ts(t),
                        "sessionId":"bench",
                        "message":{"role":"assistant","model":"claude-opus-5","stop_reason":"tool_use",
                            "content":[
                                {"type":"text","text":filler(spec.payload / 2)},
                                {"type":"tool_use","id":tid,"name":"Agent",
                                 "input":{"description":format!("probe subsystem {n}"),
                                          "subagent_type":"Explore",
                                          "prompt":filler(spec.payload / 2)}}]}
                    })
                    .to_string(),
                );
                main.push('\n');
                prev = a_id;

                // The spawn ack (a tool_result), then the subagent's own file.
                let r_id = next_uuid(&mut uuid);
                t += 2;
                main.push_str(
                    &json!({
                        "type":"user","uuid":r_id,"parentUuid":prev,"timestamp":ts(t),
                        "sessionId":"bench",
                        "message":{"role":"user","content":[
                            {"type":"tool_result","tool_use_id":tid,"content":filler(spec.payload / 2)}]}
                    })
                    .to_string(),
                );
                main.push('\n');
                prev = r_id;

                sidecars.push(Sidecar {
                    meta: json!({"agentType":"Explore",
                                 "description":format!("probe subsystem {n}"),
                                 "toolUseId":tid})
                    .to_string(),
                    transcript: subagent_transcript(&aid, t, spec),
                    agent_id: aid,
                    workflow: None,
                    journal: false,
                });
            } else if spawned_wfs < spec.workflows {
                let n = spawned_wfs;
                spawned_wfs += 1;
                tool_seq += 1;
                let tid = format!("toolu_{tool_seq:08}");
                let run_id = format!("wf_bench{n:04}");
                let a_id = next_uuid(&mut uuid);
                t += 6;
                main.push_str(
                    &json!({
                        "type":"assistant","uuid":a_id,"parentUuid":prev,"timestamp":ts(t),
                        "sessionId":"bench",
                        "message":{"role":"assistant","model":"claude-opus-5","stop_reason":"tool_use",
                            "content":[{"type":"tool_use","id":tid,"name":"Workflow",
                                        "input":{"description":"review the change across dimensions",
                                                 "name":"review-changes"}}]}
                    })
                    .to_string(),
                );
                main.push('\n');
                prev = a_id;

                // The launch ack carries the runId — ground truth for the group id.
                let r_id = next_uuid(&mut uuid);
                t += 3;
                main.push_str(
                    &json!({
                        "type":"user","uuid":r_id,"parentUuid":prev,"timestamp":ts(t),
                        "sessionId":"bench",
                        "toolUseResult":{"taskType":"local_workflow","runId":run_id,
                                         "workflowName":"review-changes",
                                         "summary":"Review changed files across dimensions"},
                        "message":{"role":"user","content":[
                            {"type":"tool_result","tool_use_id":tid,"content":"started"}]}
                    })
                    .to_string(),
                );
                main.push('\n');
                prev = r_id;

                let mut journal = String::new();
                for k in 0..spec.wf_agents {
                    let aid = agent_id(0x2000 + n * 100 + k);
                    journal.push_str(
                        &json!({"type":"started","key":format!("agent-{k}"),"agentId":aid})
                            .to_string(),
                    );
                    journal.push('\n');
                    journal.push_str(
                        &json!({"type":"result","key":format!("agent-{k}"),"agentId":aid,
                                "result":{"ok":true,"text":filler(spec.payload / 4)}})
                        .to_string(),
                    );
                    journal.push('\n');
                    sidecars.push(Sidecar {
                        meta: json!({"agentType":"workflow-subagent"}).to_string(),
                        transcript: subagent_transcript(&aid, t, spec),
                        agent_id: aid,
                        workflow: Some(run_id.clone()),
                        journal: false,
                    });
                }
                sidecars.push(Sidecar {
                    agent_id: String::new(),
                    meta: String::new(),
                    transcript: journal,
                    workflow: Some(run_id),
                    journal: true,
                });
            }
        }
    }

    Session { main, sidecars }
}

/// A subagent's own transcript: a sidechain prompt then `sub_turns` tool turns,
/// every line stamped with `agentId` (the join key the model folds on).
fn subagent_transcript(aid: &str, start: u64, spec: Spec) -> String {
    let mut s = String::new();
    let mut t = start;
    let mut seq = 0usize;

    let id = format!("{aid}-u0");
    t += 1;
    s.push_str(
        &json!({
            "type":"user","uuid":id,"parentUuid":serde_json::Value::Null,"timestamp":ts(t),
            "agentId":aid,"isSidechain":true,
            "message":{"role":"user","content":filler(spec.payload)}
        })
        .to_string(),
    );
    s.push('\n');
    let mut prev = id;

    for turn in 0..spec.sub_turns {
        seq += 1;
        let tid = format!("toolu_{aid}_{seq:05}");
        let a_id = format!("{aid}-a{turn}");
        t += 5;
        s.push_str(
            &json!({
                "type":"assistant","uuid":a_id,"parentUuid":prev,"timestamp":ts(t),
                "agentId":aid,"attributionAgent":aid,
                "message":{"role":"assistant","model":"claude-sonnet-5","stop_reason":"tool_use",
                    "content":[
                        {"type":"thinking","thinking":filler(spec.payload),"signature":"sig"},
                        {"type":"tool_use","id":tid,"name":"Grep",
                         "input":{"pattern":"fn apply_update","path":"src"}}],
                    "usage":{"input_tokens":8000,"output_tokens":400}}
            })
            .to_string(),
        );
        s.push('\n');
        prev = a_id;

        let r_id = format!("{aid}-r{turn}");
        t += 3;
        s.push_str(
            &json!({
                "type":"user","uuid":r_id,"parentUuid":prev,"timestamp":ts(t),
                "agentId":aid,"isSidechain":true,
                "message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":tid,"content":filler(spec.payload)}]}
            })
            .to_string(),
        );
        s.push('\n');
        prev = r_id;
    }
    s
}

/// Write a generated session to disk in the real on-disk layout, under
/// `project_dir`: `<uuid>.jsonl` plus `<uuid>/subagents/agent-<id>.jsonl` and
/// `<uuid>/subagents/workflows/<wf>/…`. Returns the main transcript's path.
///
/// The index and discovery benches measure the filesystem path (mmap, cache
/// sidecars, directory sweeps), so they need real files rather than strings.
pub fn write_to_disk(s: &Session, project_dir: &std::path::Path, uuid: &str) -> std::path::PathBuf {
    use zoetrope::transcript::{subagents_dir, workflow_dir, workflow_journal};

    std::fs::create_dir_all(project_dir).unwrap();
    let main = project_dir.join(format!("{uuid}.jsonl"));
    std::fs::write(&main, &s.main).unwrap();

    // Derive the sidecar layout from the same helpers the app reads it with,
    // so a layout change cannot leave the benches testing a stale shape.
    let subs = subagents_dir(&main).unwrap();
    std::fs::create_dir_all(&subs).unwrap();
    for sc in &s.sidecars {
        let dir = match &sc.workflow {
            Some(wf) => workflow_dir(&subs, wf),
            None => subs.clone(),
        };
        std::fs::create_dir_all(&dir).unwrap();
        if sc.journal {
            let journal = sc
                .workflow
                .as_deref()
                .map(|wf| workflow_journal(&subs, wf))
                .unwrap_or_else(|| dir.join("journal.jsonl"));
            std::fs::write(journal, &sc.transcript).unwrap();
            continue;
        }
        std::fs::write(
            dir.join(format!("agent-{}.jsonl", sc.agent_id)),
            &sc.transcript,
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("agent-{}.meta.json", sc.agent_id)),
            &sc.meta,
        )
        .unwrap();
    }
    main
}
