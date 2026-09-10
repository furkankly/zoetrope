//! zoetrope — visualize coding-agent sessions as a live flow graph.
//!
//! CLI (hand-rolled over `std::env::args`, no clap):
//!
//! ```text
//! zoe                       follow the current project's live session
//! zoe <file.jsonl>          replay a recording, played from the start
//! zoe <id>                  replay a session by id (or a unique prefix)
//! zoe <dir>                 follow another project's live session
//! zoe <file> --follow       follow a file's live edge instead of replaying
//! zoe <file> --speed N      playback speed multiplier (default 8.0)
//! zoe --provider <name> ... force the transcript format instead of detecting it
//! zoe inspect <file|id|dir> headless: print the session tree + info
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc;

use zoetrope::provider::{Provider, ReadMode, Target, open};
use zoetrope::state::session::SessionModel;
use zoetrope::state::{App, Mode};
use zoetrope::tailer::{TailRequest, UiEvent};
use zoetrope::{tailer, tui};

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
    /// start), a project dir (follow its live session), a session id, or
    /// `None` (the current project). `follow` starts at the live edge instead
    /// of replaying. `provider` forces the format instead of detecting it.
    View {
        target: Option<String>,
        follow: bool,
        speed: f64,
        provider: Option<Provider>,
    },
    /// Headless: parse and print the session tree + info; no TUI. The target
    /// resolves like `View`'s: a file, a session id, or a project directory.
    Inspect {
        target: String,
        provider: Option<Provider>,
    },
}

/// Default replay speed multiplier.
const DEFAULT_REPLAY_SPEED: f64 = 8.0;

const USAGE: &str = "\
zoetrope — visualize coding-agent sessions as a flow graph

USAGE:
    zoe                     follow the current project's live session
    zoe <file.jsonl>        replay a recording, played from the start
    zoe <id>                replay a session by id, or a unique prefix of one
    zoe <dir>               follow another project's live session
    zoe <file> --follow     follow a file's live edge instead of replaying
    zoe <file> --speed N    playback speed (default 8.0)
    zoe --provider <name>   force the format (claude, codex) instead of detecting it
    zoe inspect <file|id>   headless: print the session tree + info
    zoe --version           print the version and exit

Once open, scrub/follow/pause/go-live are available no matter how you launched.";

/// Parse `std::env::args` into a [`Cli`]. Returns a usage error on bad input.
fn parse_cli(args: impl Iterator<Item = String>) -> Result<Cli> {
    // Skip argv[0].
    let mut args = args.skip(1).peekable();

    let provider_flag = |args: &mut std::iter::Peekable<_>| -> Result<Option<Provider>> {
        let v: String = args
            .next()
            .ok_or_else(|| anyhow!("--provider requires a name\n\n{USAGE}"))?;
        Provider::parse(&v)
            .map(Some)
            .ok_or_else(|| anyhow!("unknown provider {v:?}; known: claude, codex"))
    };

    // `inspect <file>` is the one distinct (headless) subcommand.
    if args.peek().map(String::as_str) == Some("inspect") {
        args.next();
        let mut target: Option<String> = None;
        let mut provider = None;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--provider" => provider = provider_flag(&mut args)?,
                other if other.starts_with('-') => bail!("unknown flag {other:?}\n\n{USAGE}"),
                _ => {
                    if target.is_some() {
                        bail!("inspect takes a single target argument\n\n{USAGE}");
                    }
                    target = Some(arg);
                }
            }
        }
        let target = target.ok_or_else(|| {
            anyhow!("inspect requires a file, a session id or a directory\n\n{USAGE}")
        })?;
        return Ok(Cli::Inspect { target, provider });
    }

    // Otherwise: an optional positional target + flags.
    let mut target: Option<String> = None;
    let mut follow = false;
    let mut speed = DEFAULT_REPLAY_SPEED;
    let mut provider = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            // Packaging depends on this: the Homebrew formula's `test do`
            // block runs `zoe --version`, and it has to exit 0.
            "-V" | "--version" => {
                println!("zoe {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--follow" => follow = true,
            "--speed" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--speed requires a number\n\n{USAGE}"))?;
                speed = v
                    .parse::<f64>()
                    .with_context(|| format!("invalid --speed value: {v:?}"))?;
                if !(speed.is_finite() && speed > 0.0) {
                    bail!("--speed must be a positive number, got {v:?}");
                }
            }
            "--provider" => provider = provider_flag(&mut args)?,
            other if other.starts_with('-') => {
                bail!("unknown flag {other:?}\n\n{USAGE}");
            }
            _ => {
                if target.is_some() {
                    bail!("expected a single target argument\n\n{USAGE}");
                }
                target = Some(arg);
            }
        }
    }

    Ok(Cli::View {
        target,
        follow,
        speed,
        provider,
    })
}

/// Fully parse a session, every file of it, into a [`SessionModel`] and its
/// [`SessionInfo`](zoetrope::state::SessionInfo). Shared by `inspect`; the
/// live/replay path uses the tailer instead.
fn parse_session_fully(
    target: &Target,
    only: Option<Provider>,
) -> Result<(SessionModel, zoetrope::state::SessionInfo)> {
    let session = open(target, only)?;
    let mut model = SessionModel::new(session.id.clone());
    let mut info = zoetrope::state::SessionInfo::default();
    let p = session.provider;
    let mut apply = |mut statement: zoetrope::fact::Statement| {
        for f in statement.take_session_meta() {
            info.apply(&f);
        }
        for f in &statement.facts {
            model.apply_fact(f);
        }
    };
    // Order is not critical — the model is fold-order independent — so files
    // go in as the session lists them, root first.
    for f in session.every_file() {
        let text = std::fs::read_to_string(&f.path)
            .with_context(|| format!("reading {}", f.path.display()))?;
        match f.read {
            ReadMode::Tail => {
                let mut stream = p.stream_for(f);
                for statement in text.lines().filter_map(|l| stream.push(l)) {
                    apply(statement);
                }
            }
            ReadMode::Whole => {
                if let Some(statement) = p.sidecar(f, &text) {
                    apply(statement);
                }
            }
        }
    }

    // Group nodes have no direct completion signal — roll them up from their
    // children once everything is folded.
    model.recompute_group_status();

    // Interactive agents (main, forks) have no completion signal: derive
    // their liveness against the wall clock — `inspect` is a point-in-time
    // view, so a recently active session shows `running`, a long-quiet one `idle`.
    model.recompute_liveness(Some(chrono::Utc::now()));

    Ok((model, info))
}

/// Run the `inspect` subcommand: fully parse the session and print a tree to
/// stdout. Returns an error (non-zero exit) on an unreadable file. This is the
/// headless smoke test — no TTY required.
async fn run_inspect(target: String, provider: Option<Provider>) -> Result<()> {
    let target = resolve_target(target)?;
    let (model, info) = parse_session_fully(&target, provider)?;
    print!("{}", zoetrope::state::render::report(&model, &info));
    Ok(())
}

/// Resolve a `View` invocation into (session id, target, mode, feeder, speed),
/// then spawn the tailer and run the TUI.
///
/// A **file** or an **id** bulk-loads + tails (`replay` feeder); `--follow`
/// only changes the start position (head vs beginning) via the mode. A
/// **dir** (or no target → the current project) discovers the newest session
/// there, any provider, and live-tails it.
async fn run_tui(cli: Cli) -> Result<()> {
    let Cli::View {
        target,
        follow,
        speed,
        provider,
    } = cli
    else {
        unreachable!("inspect handled in main");
    };

    let target = match target {
        Some(t) => resolve_target(t)?,
        None => Target::Here(std::env::current_dir().context("resolving current directory")?),
    };
    let (session_id, mode, replay, speed) = match &target {
        // A concrete file, or a stored session by id → bulk-load + tail. Paced
        // from the start unless `--follow` asks to ride the (possibly still
        // growing) edge.
        Target::Path(_) | Target::Id(_) => {
            let session_id = open(&target, provider)?.id;
            let mode = if follow { Mode::Live } else { Mode::Replay };
            (session_id, mode, true, speed)
        }
        // A directory (or none → cwd) → live: discover the newest session of
        // that project and follow it. (It need not exist yet; the tailer
        // waits.) The id is best-effort so stale events filter; the tailer
        // re-discovers and may switch.
        Target::Here(_) => {
            let session_id = open(&target, provider).map(|s| s.id).unwrap_or_default();
            (session_id, Mode::Live, false, DEFAULT_REPLAY_SPEED)
        }
    };

    // Bounded request/event channels for backpressure.
    let (tail_tx, tail_rx) = mpsc::channel::<TailRequest>(CHANNEL_CAP);
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(CHANNEL_CAP);

    // Kick off the watch before the tailer task starts consuming.
    tail_tx
        .send(TailRequest::Watch(target))
        .await
        .map_err(|_| anyhow!("tailer channel closed before start"))?;

    // Spawn the tailer task — owns all files of the watched session.
    tokio::spawn(async move {
        if let Err(e) = tailer::run(tail_rx, ui_tx.clone(), replay, speed, provider).await {
            let _ = ui_tx.send(UiEvent::Error(e.to_string())).await;
        }
    });

    let app = App::new(session_id, mode);
    tui::run(app, tail_tx, ui_rx).await
}

/// What a positional argument means: an existing file is a session's file, an
/// existing directory is a project to follow, anything shaped like a path
/// that does not exist is a typo, and the rest is a session id or a prefix
/// of one. Shared by `zoe <target>` and `zoe inspect <target>`.
fn resolve_target(arg: String) -> Result<Target> {
    let path = PathBuf::from(&arg);
    if path.is_file() {
        return Ok(Target::Path(path));
    }
    if path.is_dir() {
        return Ok(Target::Here(path));
    }
    if path.components().count() > 1 || path.extension().is_some() {
        bail!("not found: {}", path.display());
    }
    Ok(Target::Id(arg))
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

    let cli = parse_cli(std::env::args())?;
    match cli {
        Cli::Inspect { target, provider } => run_inspect(target, provider).await,
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
        parse_cli(v.into_iter())
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
    fn positional_argument_is_the_target() {
        match cli(&["/tmp/foo"]).unwrap() {
            Cli::View {
                target: Some(p), ..
            } => assert_eq!(p, "/tmp/foo"),
            other => panic!("got {other:?}"),
        }
        // A session file, a directory or an id: told apart at run time.
        match cli(&["01a03eb1"]).unwrap() {
            Cli::View {
                target: Some(p), ..
            } => assert_eq!(p, "01a03eb1"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn default_speed_and_no_follow() {
        match cli(&["s.jsonl"]).unwrap() {
            Cli::View {
                speed,
                follow,
                provider,
                ..
            } => {
                assert_eq!(speed, DEFAULT_REPLAY_SPEED);
                assert!(!follow);
                assert_eq!(provider, None);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn speed_follow_and_provider_flags_in_any_order() {
        match cli(&["s.jsonl", "--speed", "4", "--follow", "--provider", "codex"]).unwrap() {
            Cli::View {
                target: Some(p),
                follow,
                speed,
                provider,
            } => {
                assert_eq!(p, "s.jsonl");
                assert_eq!(speed, 4.0);
                assert!(follow);
                assert_eq!(provider, Some(Provider::Codex));
            }
            other => panic!("got {other:?}"),
        }
        // Flags before the path, too.
        match cli(&["--provider", "Claude", "--speed", "2.5", "s.jsonl"]).unwrap() {
            Cli::View {
                target: Some(p),
                speed,
                provider,
                ..
            } => {
                assert_eq!(p, "s.jsonl");
                assert_eq!(speed, 2.5);
                assert_eq!(provider, Some(Provider::Claude));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_speed_and_unknown_provider() {
        assert!(cli(&["s.jsonl", "--speed", "nope"]).is_err());
        assert!(cli(&["s.jsonl", "--speed", "0"]).is_err());
        assert!(cli(&["s.jsonl", "--speed", "-3"]).is_err());
        assert!(cli(&["s.jsonl", "--speed"]).is_err());
        assert!(cli(&["s.jsonl", "--provider", "gemini"]).is_err());
        assert!(cli(&["s.jsonl", "--provider"]).is_err());
    }

    #[test]
    fn rejects_extra_positional_and_unknown_flags() {
        assert!(cli(&["a.jsonl", "b.jsonl"]).is_err());
        assert!(cli(&["--bogus"]).is_err());
    }

    #[test]
    fn inspect_takes_one_target_and_a_provider() {
        match cli(&["inspect", "s.jsonl"]).unwrap() {
            Cli::Inspect { target, provider } => {
                assert_eq!(target, "s.jsonl");
                assert_eq!(provider, None);
            }
            other => panic!("got {other:?}"),
        }
        // An id resolves for inspect the way it does for the TUI.
        match cli(&["inspect", "01a03eb1"]).unwrap() {
            Cli::Inspect { target, .. } => assert_eq!(target, "01a03eb1"),
            other => panic!("got {other:?}"),
        }
        match cli(&["inspect", "--provider", "codex", "s.jsonl"]).unwrap() {
            Cli::Inspect { provider, .. } => assert_eq!(provider, Some(Provider::Codex)),
            other => panic!("got {other:?}"),
        }
        assert!(cli(&["inspect"]).is_err());
        assert!(cli(&["inspect", "a", "b"]).is_err());
    }

    #[test]
    fn a_missing_path_is_a_typo_and_a_bare_word_is_an_id() {
        assert!(resolve_target("./nope.jsonl".into()).is_err());
        assert!(resolve_target("nope.jsonl".into()).is_err());
        assert!(
            matches!(resolve_target("01a03eb1".into()), Ok(Target::Id(id)) if id == "01a03eb1")
        );
        assert!(matches!(resolve_target(".".into()), Ok(Target::Here(_))));
    }

    #[test]
    fn parse_session_fully_marks_quiet_main_idle() {
        // Inspect is a point-in-time view: a transcript whose last activity is
        // far in the past reports main as Idle (interactive agents never claim
        // completion — the format has no end marker to prove it).
        let dir = std::env::temp_dir().join(format!("zoetrope-fullparse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("77777777-7777-7777-7777-777777777777.jsonl");
        std::fs::write(
            &tmp,
            b"{\"type\":\"user\",\"uuid\":\"u\",\"parentUuid\":null,\"timestamp\":\"2026-06-05T13:51:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();

        let (model, _info) = parse_session_fully(&Target::Path(tmp.clone()), None).expect("parses");
        let main = model.agent(zoetrope::state::session::MAIN_ID).unwrap();
        assert_eq!(main.status, AgentStatus::Idle);
        // The root's name comes from the provider now, not the model.
        assert_eq!(main.agent_type.as_deref(), Some("claude"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
