//! zoetrope — visualize Claude Code agent sessions as a live flow graph.
//!
//! CLI (hand-rolled over `std::env::args`, no clap):
//!
//! ```text
//! zoe                       follow the current project's live session
//! zoe <file.jsonl>          replay a recording, played from the start
//! zoe <dir>                 follow another project's live session
//! zoe <file> --follow       follow a file's live edge instead of replaying
//! zoe <file> --speed N      playback speed multiplier (default 8.0)
//! zoe inspect <file.jsonl>  headless: print the session tree + info
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc;

use zoetrope::i18n::{Locale, fill};
use zoetrope::state::session::{AgentKind, SessionModel, ToolState};
use zoetrope::state::{App, Mode};
use zoetrope::tailer::{Source, TailRequest, UiEvent, Update};
use zoetrope::{tailer, transcript, tui};

/// Channel capacity for the bounded request/event channels.
const CHANNEL_CAP: usize = 32;

/// Parsed CLI invocation.
///
/// One TUI command (`View`) over the unified timeline engine, plus the headless
/// `Inspect`. The launch only sets defaults: *what* to open and *where the
/// playhead starts*. Everything (scrub, follow, go-live, pause) is available
/// once running, regardless of how it launched.
#[derive(Debug, Clone)]
pub enum Cli {
    /// View a session in the TUI. `target`: a session file (replay it from the
    /// start), a project dir (follow its live session), or `None` (the current
    /// project). `follow` starts at the live edge instead of replaying.
    View {
        target: Option<PathBuf>,
        follow: bool,
        speed: f64,
        locale: Locale,
    },
    /// Headless: parse and print the session tree + info; no TUI.
    Inspect { file: PathBuf, locale: Locale },
}

/// Default replay speed multiplier.
const DEFAULT_REPLAY_SPEED: f64 = 8.0;

/// The usage text for a locale (the string table carries one per language).
fn usage(locale: Locale) -> &'static str {
    locale.strs().cli_usage
}

/// Resolve the launch locale: `--lang` > `ZOETROPE_LANG` > the system
/// environment (`LC_ALL`/`LC_MESSAGES`/`LANG`) > English.
fn detect_locale() -> Locale {
    std::env::var("ZOETROPE_LANG")
        .ok()
        .as_deref()
        .and_then(Locale::from_tag)
        .or_else(Locale::detect_env)
        .unwrap_or_default()
}

/// Parse `std::env::args` into a [`Cli`]. Returns a usage error on bad input.
/// `locale` is the fallback (from [`detect_locale`]); `--lang` overrides it.
fn parse_cli(args: impl Iterator<Item = String>, locale: Locale) -> Result<Cli> {
    // Skip argv[0].
    let mut args = args.skip(1).peekable();
    let mut locale = locale;

    // `inspect <file>` is the one distinct (headless) subcommand.
    if args.peek().map(String::as_str) == Some("inspect") {
        args.next();
        let file = args.next().ok_or_else(|| {
            anyhow!(
                "{}\n\n{}",
                locale.strs().err_inspect_requires,
                usage(locale)
            )
        })?;
        let mut extra = args.next();
        // `inspect <file> --lang zh` is accepted alongside the bare form.
        while let Some(arg) = extra {
            match arg.as_str() {
                "--lang" => {
                    let v = args.next().ok_or_else(|| {
                        anyhow!("{}\n\n{}", locale.strs().err_lang_requires, usage(locale))
                    })?;
                    locale = parse_lang(&v, locale)?;
                    extra = args.next();
                }
                other => bail!(
                    "{}: {other:?}\n\n{}",
                    locale.strs().err_single_path,
                    usage(locale)
                ),
            }
        }
        return Ok(Cli::Inspect {
            file: PathBuf::from(file),
            locale,
        });
    }

    // Otherwise: an optional positional target + flags.
    let mut target: Option<PathBuf> = None;
    let mut follow = false;
    let mut speed = DEFAULT_REPLAY_SPEED;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{}", usage(locale));
                std::process::exit(0);
            }
            // Packaging depends on this: the Homebrew formula's `test do`
            // block runs `zoe --version`, and it has to exit 0.
            "-V" | "--version" => {
                println!("zoe {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--follow" => follow = true,
            "--lang" => {
                let v = args.next().ok_or_else(|| {
                    anyhow!("{}\n\n{}", locale.strs().err_lang_requires, usage(locale))
                })?;
                locale = parse_lang(&v, locale)?;
            }
            "--speed" => {
                let v = args.next().ok_or_else(|| {
                    anyhow!("{}\n\n{}", locale.strs().err_speed_requires, usage(locale))
                })?;
                speed = v
                    .parse::<f64>()
                    .with_context(|| fill(locale.strs().err_speed_invalid, &[("v", &v)]))?;
                if !(speed.is_finite() && speed > 0.0) {
                    bail!("{}", fill(locale.strs().err_speed_positive, &[("v", &v)]));
                }
            }
            other if other.starts_with('-') => {
                bail!(
                    "{}\n\n{}",
                    fill(locale.strs().err_unknown_flag, &[("v", other)]),
                    usage(locale)
                );
            }
            _ => {
                if target.is_some() {
                    bail!("{}\n\n{}", locale.strs().err_single_path, usage(locale));
                }
                target = Some(PathBuf::from(arg));
            }
        }
    }

    Ok(Cli::View {
        target,
        follow,
        speed,
        locale,
    })
}

/// Parse a `--lang` value into a [`Locale`], or fail with the localized error.
fn parse_lang(v: &str, current: Locale) -> Result<Locale> {
    Locale::from_tag(v)
        .ok_or_else(|| anyhow!("{}", fill(current.strs().err_unknown_lang, &[("v", v)])))
}

/// Read a transcript file fully and fold its lines into `model` under `source`.
///
/// Defensive: unreadable lines are skipped; only the file-open error propagates.
fn fold_file(model: &mut SessionModel, path: &Path, source: Source, locale: Locale) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| {
        fill(
            locale.strs().err_reading,
            &[("p", &path.display().to_string())],
        )
    })?;
    for line in text.lines() {
        if let Some(entry) = transcript::parse_line(line) {
            model.apply_update(&Update::Entry {
                source: source.clone(),
                entry,
            });
        }
    }
    Ok(())
}

/// Read a `meta.json` sidecar and fold it into `model`. Missing/invalid sidecars
/// are silently ignored — defensiveness over strictness.
fn fold_meta(model: &mut SessionModel, path: &Path, agent_id: &str, workflow: Option<&str>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    if let Some(meta) = transcript::parse_meta(&text) {
        model.apply_meta(agent_id, workflow, &meta);
    }
}

/// Fully parse a session (main + direct subagents + workflow subagents +
/// journals) into a [`SessionModel`], reading every sidecar discovered next to
/// the main transcript. Shared by `inspect`; the live/replay path uses the
/// tailer instead.
fn parse_session_fully(main_file: &Path, locale: Locale) -> Result<SessionModel> {
    let session_id = main_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string();

    let mut model = SessionModel::new(session_id.clone());

    // Subagent sidecars live in `<main_file dir>/<session-uuid>/subagents/`.
    // Parse subagent metas + transcripts first so agents exist before the main
    // transcript's tool_results resolve their statuses. Order is not critical —
    // the model is fold-order independent for completion — but this keeps the
    // tree well-formed.
    if let Some(subs) = transcript::subagents_dir(main_file) {
        // Direct subagents: agent-<id>.jsonl + agent-<id>.meta.json
        collect_subagents(&mut model, &subs, None, locale);

        // Workflow subagents: workflows/<wf-id>/agent-*.jsonl + journal.jsonl
        for wf_id in transcript::scan_workflow_ids(&subs) {
            let wf_path = transcript::workflow_dir(&subs, &wf_id);
            collect_subagents(&mut model, &wf_path, Some(&wf_id), locale);

            // Journal ledger marks workflow-subagent completion.
            let journal = transcript::workflow_journal(&subs, &wf_id);
            if journal.is_file() {
                let _ = fold_file(&mut model, &journal, Source::Journal(wf_id.clone()), locale);
            }
        }
    }

    // Finally the main transcript — its tool_results resolve subagent statuses.
    fold_file(&mut model, main_file, Source::Main, locale)?;

    // Workflow group nodes have no direct completion signal — roll them up from
    // their children once everything is folded.
    model.recompute_workflow_status();

    // Interactive agents (main, forks) have no completion signal: derive
    // their liveness against the wall clock — `inspect` is a point-in-time
    // view, so a recently active session shows `running`, a long-quiet one `idle`.
    model.recompute_liveness(Some(chrono::Utc::now()));

    Ok(model)
}

/// Discover and fold every `agent-<id>.jsonl` (+ `.meta.json`) in `dir`. Used
/// for both direct subagents (`workflow == None`) and workflow subagents.
fn collect_subagents(model: &mut SessionModel, dir: &Path, workflow: Option<&str>, locale: Locale) {
    for file in transcript::scan_subagent_files(dir, workflow) {
        // Fold the meta sidecar first so the node exists with type/desc.
        if file.meta.is_file() {
            fold_meta(model, &file.meta, &file.agent_id, workflow);
        }
        let _ = fold_file(model, &file.transcript, Source::Sub(file.agent_id), locale);
    }
}

/// Run the `inspect` subcommand: fully parse the session and print a tree to
/// stdout. Returns an error (non-zero exit) on an unreadable file. This is the
/// headless smoke test — no TTY required.
async fn run_inspect(file: PathBuf, locale: Locale) -> Result<()> {
    if !file.is_file() {
        bail!(
            "{}",
            fill(
                locale.strs().err_not_readable,
                &[("p", &file.display().to_string())]
            )
        );
    }
    let model = parse_session_fully(&file, locale)?;
    let info = read_session_info(&file);
    let s = locale.strs();

    let title = info.title.as_deref().unwrap_or(s.inspect_untitled);
    println!(
        "{}",
        fill(
            s.inspect_session,
            &[("id", &model.session_id), ("title", title)]
        )
    );
    // Session-level metadata (the `i` overlay's content, headless).
    println!(
        "{}",
        fill(
            s.inspect_mode_perm,
            &[
                ("m", info.mode.as_deref().unwrap_or("—")),
                ("p", info.permission_mode.as_deref().unwrap_or("—")),
            ],
        )
    );
    println!(
        "{}",
        fill(
            s.inspect_summary,
            &[
                ("a", &model.agent_count().to_string()),
                ("t", &model.tool_count().to_string()),
                ("q", &info.queued_ops.to_string()),
                ("f", &info.file_snapshots.to_string()),
            ],
        )
    );
    if let Some(p) = &info.last_prompt {
        println!(
            "{}",
            fill(s.inspect_last_prompt, &[("p", &format!("{p:?}"))])
        );
    }
    println!();

    // Print roots first, then children indented underneath, in spawn order.
    print_agent_tree(&model, None, 0, locale);

    Ok(())
}

/// Build `SessionInfo` from the main file's untimed flat-metadata (mirrors the
/// timeline feeder's extraction, for the headless `inspect` path). Latest-wins by
/// file order; counts accumulate.
fn read_session_info(main_path: &Path) -> zoetrope::state::SessionInfo {
    let mut info = zoetrope::state::SessionInfo::default();
    if let Ok(text) = std::fs::read_to_string(main_path) {
        for line in text.lines() {
            if let Some(entry) = transcript::parse_line(line) {
                info.apply(&entry);
            }
        }
    }
    info
}

/// Recursively print agents whose `parent` equals `parent`, in spawn order.
fn print_agent_tree(model: &SessionModel, parent: Option<&str>, depth: usize, locale: Locale) {
    for id in &model.spawn_order {
        let Some(agent) = model.agent(id) else {
            continue;
        };
        if agent.parent.as_deref() != parent {
            continue;
        }

        let indent = "  ".repeat(depth + 1);
        let s = locale.strs();
        let kind = match agent.kind {
            AgentKind::Main => s.kind_main,
            AgentKind::Subagent => s.kind_subagent,
            AgentKind::WorkflowGroup => s.kind_workflow,
        };
        // Single source: same wording + glyph the cards/panel use.
        let status = agent.status_word(locale);
        let glyph = agent.status.glyph();

        let label = agent
            .agent_type
            .as_deref()
            .or(agent.description.as_deref())
            .unwrap_or(id);

        // Tool tallies.
        let mut ok = 0u32;
        let mut err = 0u32;
        let mut pending = 0u32;
        for t in &agent.tool_calls {
            match t.state {
                ToolState::Ok => ok += 1,
                ToolState::Err => err += 1,
                ToolState::Pending => pending += 1,
            }
        }

        println!("{indent}{glyph} [{kind}] {label}  ({status}) — id={id}");
        if let Some(desc) = &agent.description
            && agent.agent_type.is_some()
        {
            println!("{indent}    {desc}");
        }
        if let Some(model_name) = &agent.model {
            println!("{}", fill(s.inspect_model, &[("m", model_name)]));
        }
        println!(
            "{indent}{}",
            fill(
                s.inspect_tools,
                &[
                    ("n", &agent.tool_calls.len().to_string()),
                    ("ok", &ok.to_string()),
                    ("err", &err.to_string()),
                    ("pend", &pending.to_string()),
                    ("k", &agent.output_tokens.to_string()),
                ],
            )
        );

        // Provenance: what triggered this agent (the panel's `↳ prompt`/`↳ thought`).
        if let Some(ctx) = model.provenance(agent) {
            if let Some(prompt) = model.provenance_prompt(ctx) {
                println!("{}", fill(s.inspect_prompt, &[("p", prompt)]));
            }
            if let Some(reasoning) = &ctx.reasoning {
                println!("{}", fill(s.inspect_thought, &[("p", reasoning)]));
            }
        }

        // Recurse into this agent's children (workflow groups have subagent
        // children, main has direct subagents + workflow groups).
        print_agent_tree(model, Some(id), depth + 1, locale);
    }
}

/// Resolve a `View` invocation into (session id, watch target, mode, feeder,
/// speed), then spawn the tailer and run the TUI.
///
/// A **file** target bulk-loads + tails (`replay` feeder); `--follow` only
/// changes the start position (head vs beginning) via the mode. A **dir** (or no
/// target → the current project) discovers the latest session and live-tails.
async fn run_tui(cli: Cli) -> Result<()> {
    let Cli::View {
        target,
        follow,
        speed,
        locale,
    } = cli
    else {
        unreachable!("inspect handled in main");
    };

    let (session_id, watch_target, mode, replay, speed) = match target {
        // A concrete file → bulk-load + tail. Paced from the start unless
        // `--follow` asks to ride the (possibly still-growing) edge.
        Some(file) if file.is_file() => {
            let session_id = file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("session")
                .to_string();
            let mode = if follow { Mode::Live } else { Mode::Replay };
            (session_id, file, mode, true, speed)
        }
        // A directory (or none → cwd) → live: discover the latest session and
        // follow it. (The dir need not exist yet; the tailer waits.)
        other => {
            if let Some(p) = &other
                && !p.is_dir()
            {
                bail!("not found: {}", p.display());
            }
            let cwd = match other {
                Some(d) => d,
                None => std::env::current_dir().context("resolving current directory")?,
            };
            let proj = transcript::project_dir(&cwd)
                .ok_or_else(|| anyhow!("no Claude projects directory for {}", cwd.display()))?;
            // Best-effort latest session id so stale events filter; the tailer
            // re-discovers and may switch.
            let session_id = transcript::latest_session_file(&proj)
                .as_deref()
                .and_then(Path::file_stem)
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            (session_id, proj, Mode::Live, false, DEFAULT_REPLAY_SPEED)
        }
    };

    // Bounded request/event channels for backpressure.
    let (tail_tx, tail_rx) = mpsc::channel::<TailRequest>(CHANNEL_CAP);
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(CHANNEL_CAP);

    // Kick off the watch before the tailer task starts consuming.
    tail_tx
        .send(TailRequest::Watch(watch_target))
        .await
        .map_err(|_| anyhow!("tailer channel closed before start"))?;

    // Spawn the tailer task — owns all files of the watched session.
    tokio::spawn(async move {
        if let Err(e) = tailer::run(tail_rx, ui_tx.clone(), replay, speed).await {
            let _ = ui_tx.send(UiEvent::Error(e.to_string())).await;
        }
    });

    let mut app = App::new(session_id, mode);
    app.set_locale(locale);
    tui::run(app, tail_tx, ui_rx).await
}

#[tokio::main]
async fn main() -> Result<()> {
    // Answer and exit before the TUI touches the terminal, so this works over a
    // pipe — assets/build.sh asks for it and has no tty to spare. The tape's
    // Sleep has to be at least this long or the recording cuts mid-gesture, and
    // that number used to be copied into the tape by hand.
    if std::env::var("ZOETROPE_DEMO").as_deref() == Ok("duration") {
        println!("{:.2}", zoetrope::autopilot::tour_secs());
        return Ok(());
    }

    let cli = parse_cli(std::env::args(), detect_locale())?;
    match cli {
        Cli::Inspect { file, locale } => run_inspect(file, locale).await,
        other => run_tui(other).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zoetrope::state::session::AgentStatus;

    fn cli(args: &[&str]) -> Result<Cli> {
        // parse_cli skips argv[0], so prepend a fake program name.
        let mut v = vec!["zoe".to_string()];
        v.extend(args.iter().map(|s| s.to_string()));
        parse_cli(v.into_iter(), Locale::En)
    }

    #[test]
    fn bare_invocation_is_live_for_cwd() {
        match cli(&[]).unwrap() {
            Cli::View {
                target: None,
                follow: false,
                ..
            } => {}
            other => panic!("expected View{{target:None}}, got {other:?}"),
        }
    }

    #[test]
    fn positional_path_is_the_target() {
        match cli(&["/tmp/foo"]).unwrap() {
            Cli::View {
                target: Some(p), ..
            } => assert_eq!(p, PathBuf::from("/tmp/foo")),
            other => panic!("got {other:?}"),
        }
        // Works for a session file too (file-vs-dir is resolved at run time).
        match cli(&["s.jsonl"]).unwrap() {
            Cli::View {
                target: Some(p), ..
            } => assert_eq!(p, PathBuf::from("s.jsonl")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn default_speed_and_no_follow() {
        match cli(&["s.jsonl"]).unwrap() {
            Cli::View { speed, follow, .. } => {
                assert_eq!(speed, DEFAULT_REPLAY_SPEED);
                assert!(!follow);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn speed_and_follow_flags_in_any_order() {
        match cli(&["s.jsonl", "--speed", "4", "--follow"]).unwrap() {
            Cli::View {
                target: Some(p),
                follow,
                speed,
                ..
            } => {
                assert_eq!(p, PathBuf::from("s.jsonl"));
                assert_eq!(speed, 4.0);
                assert!(follow);
            }
            other => panic!("got {other:?}"),
        }
        // Flags before the path, too.
        match cli(&["--speed", "2.5", "s.jsonl"]).unwrap() {
            Cli::View {
                target: Some(p),
                speed,
                ..
            } => {
                assert_eq!(p, PathBuf::from("s.jsonl"));
                assert_eq!(speed, 2.5);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_speed() {
        assert!(cli(&["s.jsonl", "--speed", "nope"]).is_err());
        assert!(cli(&["s.jsonl", "--speed", "0"]).is_err());
        assert!(cli(&["s.jsonl", "--speed", "-3"]).is_err());
        assert!(cli(&["s.jsonl", "--speed"]).is_err());
    }

    #[test]
    fn rejects_extra_positional_and_unknown_flags() {
        assert!(cli(&["a.jsonl", "b.jsonl"]).is_err());
        assert!(cli(&["--bogus"]).is_err());
    }

    #[test]
    fn inspect_takes_one_file() {
        match cli(&["inspect", "s.jsonl"]).unwrap() {
            Cli::Inspect { file, .. } => assert_eq!(file, PathBuf::from("s.jsonl")),
            other => panic!("got {other:?}"),
        }
        assert!(cli(&["inspect"]).is_err());
        assert!(cli(&["inspect", "a", "b"]).is_err());
    }

    #[test]
    fn parse_session_fully_marks_quiet_main_idle() {
        // Inspect is a point-in-time view: a transcript whose last activity is
        // far in the past reports main as Idle (interactive agents never claim
        // completion — the format has no end marker to prove it).
        let tmp = std::env::temp_dir().join(format!(
            "zoetrope-fullparse-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(
            &tmp,
            b"{\"type\":\"user\",\"uuid\":\"u\",\"parentUuid\":null,\"timestamp\":\"2026-06-05T13:51:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();

        let model = parse_session_fully(&tmp, Locale::En).expect("parses");
        assert_eq!(
            model
                .agent(zoetrope::state::session::MAIN_ID)
                .unwrap()
                .status,
            AgentStatus::Idle
        );

        let _ = std::fs::remove_file(&tmp);
    }
}
