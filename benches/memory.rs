//! Memory baseline: how much resident memory one loaded session costs.
//!
//! Not a criterion bench — it prints a table. Timing lives in `timeline.rs`;
//! this answers the other half of the multi-session question, since the whole
//! point of tracking N sessions is that N × this has to fit.
//!
//! RSS is sampled from `/proc/self/statm`, so the deltas are approximate: the
//! allocator does not return freed pages to the OS, which makes each stage's
//! delta an upper bound on what that stage retains and the total a fair figure
//! for "what one open session costs". Non-Linux prints the shape only.

mod common;

use common::{Session, Spec};
use zoetrope::state::{App, Mode};
use zoetrope::tailer::{UiEvent, replay_from_session};

/// Resident set size in bytes, or `None` where `/proc` is not available.
fn rss() -> Option<usize> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: usize = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * page_size())
}

fn page_size() -> usize {
    // 4 KiB everywhere this runs; reading it out of `getconf` is not worth a dep.
    4096
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Load one scale end to end, sampling RSS at each stage.
fn measure(name: &str, spec: Spec) {
    let base = rss();

    let s: Session = common::session(spec);
    let after_gen = rss();

    let (items, info) = replay_from_session(&s.main, &s.demo_subagents());
    let item_count = items.len();
    let after_items = rss();

    let mut app = App::new("bench".to_string(), Mode::Live);
    app.handle_ui_event(UiEvent::ReplayLoaded {
        session_id: "bench".to_string(),
        items,
        speed: 8.0,
        info,
    });
    app.go_live();
    let after_app = rss();

    let delta = |a: Option<usize>, b: Option<usize>| match (a, b) {
        (Some(a), Some(b)) => format!("{:>8.1}", mb(b.saturating_sub(a))),
        _ => "     n/a".to_string(),
    };

    println!(
        "{name:<7} {:>7} {:>9.1} {:>9} {:>8} {:>8} {:>8} {:>8} {:>7} {:>7}",
        s.lines(),
        mb(s.bytes()),
        item_count,
        delta(base, after_gen),
        delta(after_gen, after_items),
        delta(after_items, after_app),
        delta(base, after_app),
        app.session.agent_count(),
        app.flow.nodes().count(),
    );

    // Keep everything alive to the end of the measurement.
    std::hint::black_box(&s);
    std::hint::black_box(&app);
}

fn main() {
    println!("synthetic session memory baseline (RSS deltas, MB)\n");
    println!(
        "{:<7} {:>7} {:>9} {:>9} {:>8} {:>8} {:>8} {:>8} {:>7} {:>7}",
        "scale", "lines", "text MB", "items", "gen", "items", "app", "total", "agents", "nodes"
    );
    println!("{}", "-".repeat(88));
    // Each scale runs in its own process-lifetime segment; the allocator's
    // retained pages make later rows read high, so run them smallest-first and
    // read each row's `total` as the headline.
    measure("small", Spec::SMALL);
    measure("medium", Spec::MEDIUM);
    measure("large", Spec::LARGE);
}
