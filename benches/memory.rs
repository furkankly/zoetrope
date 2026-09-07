//! Memory baseline: how much heap one loaded session costs.
//!
//! Not a criterion bench — it prints a table. Timing lives in `timeline.rs`;
//! this answers the other half: a snapshot ladder trades memory for seek time,
//! so the rungs have to be worth what they retain.
//!
//! Measured with a counting global allocator rather than RSS, which reports
//! LIVE heap: exact, allocator-independent, and identical on every platform.
//! RSS could only ever be an upper bound, since the allocator does not return
//! freed pages to the OS, and it read as zero wherever `/proc` is absent.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Session, Spec};
use zoetrope::state::{App, Mode};
use zoetrope::tailer::UiEvent;

/// Bytes currently allocated and not yet freed.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// `System`, plus a running total of live bytes.
///
/// Relaxed ordering throughout: the counter is a statistic, and no other memory
/// is published through it.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_add(new_size, Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Load one scale end to end, sampling live heap at each stage.
fn measure(name: &str, spec: Spec) {
    let base = live();

    let s: Session = common::session(spec);
    let after_gen = live();

    let (items, info) = s.load();
    let item_count = items.len();
    let after_items = live();

    let mut app = App::new("bench".to_string(), Mode::Live);
    app.handle_ui_event(UiEvent::ReplayLoaded {
        session_id: "bench".to_string(),
        items,
        speed: 8.0,
        info,
    });
    app.go_live();
    let after_app = live();

    // The ladder is the part that trades memory for seek time, so price it on
    // its own. The rungs were taken during the fold to the edge above, and they
    // share structure with the live model, so their cost is only what nothing
    // else keeps alive: read the heap with them, drop them, read it again.
    // (Seeking away and back does NOT measure this — rungs already exist at
    // every stride, so a seek takes none; it only builds a second model.)
    let with_ladder = live();
    app.drop_snapshot_ladder();
    let ladder = with_ladder.saturating_sub(live());

    let d = |a: usize, b: usize| mb(b.saturating_sub(a));

    println!(
        "{name:<7} {:>7} {:>9.1} {:>9} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>7} {:>7}",
        s.lines(),
        mb(s.bytes()),
        item_count,
        d(base, after_gen),
        d(after_gen, after_items),
        d(after_items, after_app),
        d(base, after_app),
        mb(ladder),
        app.session.agent_count(),
        app.flow.nodes().count(),
    );

    // Keep everything alive to the end of the measurement.
    std::hint::black_box(&s);
    std::hint::black_box(&app);
}

fn main() {
    println!("synthetic session memory baseline (live heap, MB)\n");
    println!(
        "{:<7} {:>7} {:>9} {:>9} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7} {:>7}",
        "scale",
        "lines",
        "text MB",
        "items",
        "gen",
        "items",
        "app",
        "total",
        "ladder",
        "agents",
        "nodes"
    );
    println!("{}", "-".repeat(97));
    measure("small", Spec::SMALL);
    measure("medium", Spec::MEDIUM);
    measure("large", Spec::LARGE);
}
