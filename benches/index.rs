//! Benchmarks for the skeleton index — the fast half of the parse pipeline.
//!
//! Read these against `timeline.rs`'s `parse` and `load` groups: the same
//! synthetic sessions go through both, so the numbers are directly comparable.
//! `parse_line` there is what the model costs today; `skeleton_scan` here is
//! what it costs to learn the same timeline facts without the bodies.

mod common;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{Session, Spec};
use zoetrope::index::{self, Index};
use zoetrope::sessions;

/// A scratch projects root holding written-out synthetic sessions, plus its own
/// index cache. Removed on drop.
struct Corpus {
    root: PathBuf,
    sessions: Vec<(&'static str, Session, PathBuf)>,
}

impl Corpus {
    fn new() -> Corpus {
        let root =
            std::env::temp_dir().join(format!("zoetrope-index-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let projects = root.join("projects");

        // Point the index cache at the scratch tree: a benchmark must not read
        // or write the developer's real `~/.cache`.
        // SAFETY: single-threaded bench setup, before criterion starts any
        // measurement threads.
        unsafe { std::env::set_var("XDG_CACHE_HOME", root.join("cache")) };

        let specs: [(&'static str, Spec); 3] = [
            ("small", Spec::SMALL),
            ("medium", Spec::MEDIUM),
            ("large", Spec::LARGE),
        ];
        let sessions = specs
            .into_iter()
            .enumerate()
            .map(|(i, (name, spec))| {
                let s = common::session(spec);
                let uuid = format!("{:08x}-1111-2222-3333-444444444444", i + 1);
                let main = common::write_to_disk(&s, &projects.join(name), &uuid);
                (name, s, main)
            })
            .collect();

        Corpus { root, sessions }
    }

    /// Drop every cached index, so the next `open` is a genuine cold scan.
    fn clear_cache(&self) {
        let _ = std::fs::remove_dir_all(self.root.join("cache"));
    }
}

impl Drop for Corpus {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Every transcript file of the session rooted at `main`.
fn session_files(main: &Path) -> Vec<PathBuf> {
    let root = main.parent().unwrap().parent().unwrap();
    sessions::discover(root, SystemTime::UNIX_EPOCH)
        .iter()
        .find(|s| s.main_path == main)
        .expect("session discovered")
        .files()
}

/// Every transcript file of a session, as raw bytes.
fn all_bytes(main: &Path) -> Vec<Vec<u8>> {
    session_files(main)
        .iter()
        .filter_map(|p| std::fs::read(p).ok())
        .collect()
}

fn bench_scan(c: &mut Criterion) {
    let corpus = Corpus::new();
    let mut g = c.benchmark_group("index");
    g.sample_size(20);
    g.warm_up_time(Duration::from_millis(500));
    g.measurement_time(Duration::from_secs(8));

    for (name, session, main) in &corpus.sessions {
        let files = all_bytes(main);
        let bytes: usize = files.iter().map(Vec::len).sum();
        // `open_*` must cover the SAME files as `skeleton_scan`, or the
        // throughput divisor describes bytes the benchmark never touched.
        let paths = session_files(main);
        g.throughput(Throughput::Bytes(bytes as u64));

        // The skeleton scan itself: bytes → records, no file IO, no bodies.
        g.bench_with_input(
            BenchmarkId::new("skeleton_scan", name),
            &files,
            |b, files| {
                b.iter(|| {
                    let mut n = 0usize;
                    for f in files {
                        let mut idx = Index::default();
                        idx.extend(black_box(f));
                        n += idx.recs.len();
                    }
                    n
                })
            },
        );

        // The real entry point over a whole session, cache cold: mmap + scan +
        // write the cache, for every file.
        g.bench_with_input(BenchmarkId::new("open_cold", name), &paths, |b, paths| {
            b.iter_batched(
                || corpus.clear_cache(),
                |()| {
                    paths
                        .iter()
                        .filter_map(|p| index::open(black_box(p)).ok())
                        .map(|(idx, _map)| idx.recs.len())
                        .sum::<usize>()
                },
                criterion::BatchSize::PerIteration,
            )
        });

        // Cache warm: stat, read a header, mmap. What every run after the first
        // one costs.
        for p in &paths {
            let _ = index::open(p);
        }
        g.bench_with_input(BenchmarkId::new("open_warm", name), &paths, |b, paths| {
            b.iter(|| {
                paths
                    .iter()
                    .filter_map(|p| index::open(black_box(p)).ok())
                    .map(|(idx, _map)| idx.recs.len())
                    .sum::<usize>()
            })
        });

        black_box(session);
    }
    g.finish();
}

/// A corpus shaped like a real `~/.claude/projects`: many projects, several
/// sessions each. Discovery cost scales with the number of FILES, not their
/// size, so the three-session corpus above says nothing useful about it.
struct WideCorpus {
    root: PathBuf,
}

impl WideCorpus {
    /// 40 projects × 2 sessions, with sidecars under each — the order of
    /// magnitude a long-lived `~/.claude/projects` reaches.
    fn new() -> WideCorpus {
        let root = std::env::temp_dir().join(format!("zoetrope-wide-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let projects = root.join("projects");
        // SAFETY: single-threaded bench setup, before measurement threads start.
        unsafe { std::env::set_var("XDG_CACHE_HOME", root.join("cache")) };

        let session = common::session(Spec::SMALL);
        for p in 0..40 {
            for s in 0..2 {
                let uuid = format!("{p:08x}-1111-2222-3333-{s:012x}");
                common::write_to_disk(&session, &projects.join(format!("proj-{p}")), &uuid);
            }
        }
        WideCorpus { root }
    }

    fn projects(&self) -> PathBuf {
        self.root.join("projects")
    }
}

impl Drop for WideCorpus {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn bench_sessions(c: &mut Criterion) {
    let corpus = WideCorpus::new();
    let root = corpus.projects();
    // Warm every cache once: the summary bench measures steady state, and the
    // cold cost is already covered by `open_cold`.
    for s in sessions::discover(&root, SystemTime::UNIX_EPOCH) {
        let _ = sessions::Summary::of(&s);
    }

    let mut g = c.benchmark_group("sessions");
    g.sample_size(20);
    g.warm_up_time(Duration::from_millis(500));
    g.measurement_time(Duration::from_secs(8));

    let n = sessions::discover(&root, SystemTime::UNIX_EPOCH).len();
    let files: usize = sessions::discover(&root, SystemTime::UNIX_EPOCH)
        .iter()
        .map(|s| s.sidecar_count() + 1)
        .sum();
    g.throughput(Throughput::Elements(files as u64));
    assert_eq!(n, 80, "the wide corpus is 40 projects x 2 sessions");

    // Phase 1: the stat sweep over every project and every sidecar.
    g.bench_function("discover_80_sessions", |b| {
        b.iter(|| sessions::discover(black_box(&root), SystemTime::UNIX_EPOCH).len())
    });

    // Phase 2: summarize every discovered session from its index.
    g.bench_function("summarize_80_sessions", |b| {
        b.iter_batched(
            || sessions::discover(&root, SystemTime::UNIX_EPOCH),
            |found| {
                found
                    .iter()
                    .map(|s| sessions::Summary::of(s).lines)
                    .sum::<usize>()
            },
            criterion::BatchSize::SmallInput,
        )
    });
    g.finish();
}

criterion_group!(benches, bench_scan, bench_sessions);
criterion_main!(benches);
