//! Baseline benchmarks for the parse → timeline → fold → seek pipeline.
//!
//! Every input is synthesized (`common::session`) — the benches never read a
//! real transcript, so a run is reproducible on any machine. Three scales
//! bracket what the format produces in practice: a short session, a long
//! working session, and a pathologically large one.
//!
//! What each group answers:
//! - `parse`   — raw `parse_line` throughput, the floor on any cold open.
//! - `load`    — assembling the ts-ordered timeline, and folding it into a model.
//! - `seek`    — the playhead moves. `seek_back_*` is the O(k) rebuild path
//!   (`App::rebuild_to`), the number that decides whether scrubbing is usable.

mod common;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;

use common::{Session, Spec};
use zoetrope::state::session::SessionModel;
use zoetrope::state::{App, Mode};
use zoetrope::tailer::{ReplayItem, UiEvent, replay_from_session};
use zoetrope::transcript;

/// The scales every group runs over.
fn scales() -> Vec<(&'static str, Session)> {
    vec![
        ("small", common::session(Spec::SMALL)),
        ("medium", common::session(Spec::MEDIUM)),
        ("large", common::session(Spec::LARGE)),
    ]
}

/// Assemble the ts-ordered timeline for a session (the untimed setup step
/// shared by the fold and seek benches).
fn items_of(s: &Session) -> Vec<ReplayItem> {
    replay_from_session(&s.main, &s.demo_subagents()).0
}

/// An App with the whole session loaded and the playhead at the live edge —
/// the state a user is in when they grab the scrubber.
fn app_at_edge(s: &Session) -> App {
    let (items, info) = replay_from_session(&s.main, &s.demo_subagents());
    let mut app = App::new("bench".to_string(), Mode::Live);
    app.handle_ui_event(UiEvent::ReplayLoaded {
        session_id: "bench".to_string(),
        items,
        speed: 8.0,
        info,
    });
    app.go_live();
    app
}

fn bench_parse(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse");
    for (name, s) in scales() {
        // Every line of every file — what a cold open of one session reads.
        let lines: Vec<&str> = s
            .main
            .lines()
            .chain(s.sidecars.iter().flat_map(|sc| sc.transcript.lines()))
            .collect();
        let bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
        g.throughput(Throughput::Bytes(bytes as u64));
        g.bench_with_input(BenchmarkId::new("parse_line", name), &lines, |b, lines| {
            b.iter(|| {
                let mut n = 0usize;
                for l in lines {
                    if transcript::parse_line(black_box(l)).is_some() {
                        n += 1;
                    }
                }
                n
            })
        });
    }
    g.finish();
}

fn bench_load(c: &mut Criterion) {
    let mut g = c.benchmark_group("load");
    // A cold open at the large scale is on the order of a second, so the
    // criterion defaults (100 samples, 3 s warm-up) would run for many minutes
    // to sharpen a number whose interesting digit is the first one.
    g.sample_size(10);
    g.warm_up_time(Duration::from_millis(500));
    g.measurement_time(Duration::from_secs(10));
    for (name, s) in scales() {
        g.throughput(Throughput::Bytes(s.bytes() as u64));

        // Text → dated, sorted timeline items.
        g.bench_with_input(BenchmarkId::new("replay_assemble", name), &s, |b, s| {
            b.iter(|| black_box(items_of(s).len()))
        });

        // Items → model. The pure fold, no graph projection.
        g.bench_with_input(BenchmarkId::new("fold_cold", name), &s, |b, s| {
            b.iter_batched(
                || items_of(s),
                |items| {
                    let mut m = SessionModel::new("bench".to_string());
                    for it in &items {
                        m.apply_update(&it.update);
                    }
                    black_box(m.agents.len())
                },
                BatchSize::LargeInput,
            )
        });

        // The real cold open: assemble + fold + project onto the flow graph.
        g.bench_with_input(BenchmarkId::new("app_cold_open", name), &s, |b, s| {
            b.iter(|| black_box(app_at_edge(s).flow.nodes().count()))
        });
    }
    g.finish();
}

fn bench_seek(c: &mut Criterion) {
    let mut g = c.benchmark_group("seek");
    // Each sample re-loads the session in untimed setup (a seek mutates the
    // App, so it cannot be reused) — same reasoning as `load` above.
    g.sample_size(10);
    g.warm_up_time(Duration::from_millis(500));
    g.measurement_time(Duration::from_secs(10));
    for (name, s) in scales() {
        let n = items_of(&s).len();
        g.throughput(Throughput::Elements(n as u64));

        // Backward seek to the middle — rebuilds the model from items[0..n/2].
        g.bench_with_input(BenchmarkId::new("back_to_50pct", name), &s, |b, s| {
            b.iter_batched_ref(
                || app_at_edge(s),
                |app| app.seek_to_fraction(black_box(0.5)),
                BatchSize::PerIteration,
            )
        });

        // Backward seek to the start — the cheapest rebuild (folds nothing) but
        // still pays the full model + flow teardown.
        g.bench_with_input(BenchmarkId::new("back_to_start", name), &s, |b, s| {
            b.iter_batched_ref(
                || app_at_edge(s),
                |app| app.seek_to_fraction(black_box(0.0)),
                BatchSize::PerIteration,
            )
        });

        // Forward seek across the whole timeline — the in-place fold path, for
        // contrast with the rebuild above.
        g.bench_with_input(BenchmarkId::new("fwd_start_to_edge", name), &s, |b, s| {
            b.iter_batched_ref(
                || {
                    let mut app = app_at_edge(s);
                    app.seek_to_fraction(0.0);
                    app
                },
                |app| app.seek_to_fraction(black_box(1.0)),
                BatchSize::PerIteration,
            )
        });

        // One SMALL backward hop — what a scrubber drag actually delivers once
        // `pending_seek` coalesces a burst to one seek per frame. The big jumps
        // above are the worst case for patching the canvas; this is the case
        // it exists for.
        g.bench_with_input(BenchmarkId::new("back_one_hop", name), &s, |b, s| {
            b.iter_batched_ref(
                || app_at_edge(s),
                |app| app.seek_to_fraction(black_box(0.95)),
                BatchSize::PerIteration,
            )
        });

        // A scrub drag: ten backward hops. This is what holding the mouse down
        // on the scrubber actually costs.
        g.bench_with_input(BenchmarkId::new("drag_10_back", name), &s, |b, s| {
            b.iter_batched_ref(
                || app_at_edge(s),
                |app| {
                    for i in (0..10).rev() {
                        app.seek_to_fraction(black_box(i as f64 / 10.0));
                    }
                },
                BatchSize::PerIteration,
            )
        });
    }
    g.finish();
}

criterion_group!(benches, bench_parse, bench_load, bench_seek);
criterion_main!(benches);
