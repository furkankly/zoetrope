//! The conformance check every provider runs: whatever its files state, the
//! model must reach one state however those files interleave
//! (ARCHITECTURE.md §1.1). Test-only.

use std::path::PathBuf;

use crate::fact::{Fact, Statement};
use crate::state::session::SessionModel;
use crate::tailer::ReplayItem;

/// A provider's fixture directory, `assets/<provider>/`, if this checkout has
/// it. The published crate ships only `src/`, so fixture-driven tests skip
/// (loudly) rather than fail there.
pub(crate) fn fixture_dir(provider: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join(provider);
    if dir.is_dir() {
        Some(dir)
    } else {
        eprintln!(
            "skipping: no fixtures at {} (not a git checkout)",
            dir.display()
        );
        None
    }
}

/// The whole conformance check for one fixture of one provider:
///
/// 1. order invariance over every fact the files state (§1.1);
/// 2. the folded model matches `assets/<provider>/<fixture>.model.txt`;
/// 3. the dated, sorted timeline matches `assets/<provider>/<fixture>.timeline.txt`.
///
/// The two golden files are what the provider extracts from that session, in
/// the form a person reads to check it. Regenerate with `UPDATE_GOLDEN=1`.
pub(crate) fn conform(provider: &str, fixture: &str, streams: impl Fn() -> Vec<Vec<Statement>>) {
    let Some(dir) = fixture_dir(provider) else {
        return;
    };
    let facts = || -> Vec<Vec<Fact>> {
        streams()
            .into_iter()
            .map(|s| s.into_iter().flat_map(|st| st.facts).collect())
            .collect()
    };
    assert_order_invariant(facts);

    // The model, folded in file order and settled as a finished replay would be.
    let mut model = SessionModel::new(fixture.to_string());
    for stream in streams() {
        for statement in stream {
            for fact in &statement.facts {
                model.apply_fact(fact);
            }
        }
    }
    model.recompute_group_status();
    model.end_of_stream();
    golden(
        &dir.join(format!("{fixture}.model.txt")),
        &crate::state::render::agents(&model),
    );

    // The timeline: every statement dated and sorted, one line per item.
    let mut items: Vec<ReplayItem> = streams()
        .into_iter()
        .flatten()
        .filter_map(|mut st| {
            st.take_session_meta();
            (!st.facts.is_empty()).then(|| ReplayItem::new(st))
        })
        .collect();
    crate::tailer::date_and_sort(&mut items);
    let mut text = String::new();
    for item in &items {
        let at = item.ts().map_or("-".to_string(), |t| t.to_rfc3339());
        let said: Vec<String> = item
            .facts
            .iter()
            .map(|f| format!("{}@{}", f.kind.name(), f.agent.as_deref().unwrap_or("-")))
            .collect();
        text.push_str(&format!("{at}  {}\n", said.join(" ")));
    }
    golden(&dir.join(format!("{fixture}.timeline.txt")), &text);
}

/// Compare `actual` to the file at `path`, or rewrite it under `UPDATE_GOLDEN=1`.
fn golden(path: &std::path::Path, actual: &str) {
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(path, actual).unwrap();
        eprintln!("wrote {}", path.display());
        return;
    }
    let expected = std::fs::read_to_string(path)
        .unwrap_or_else(|_| panic!("no golden at {}; run with UPDATE_GOLDEN=1", path.display()))
        // A Windows checkout may carry CRLF; the golden is compared by content, not bytes.
        .replace("\r\n", "\n");
    if expected != actual {
        let first = expected
            .lines()
            .zip(actual.lines())
            .position(|(e, a)| e != a)
            .unwrap_or(expected.lines().count().min(actual.lines().count()));
        panic!(
            "{} differs from the folded result (first difference at line {}):\n  expected: {:?}\n  actual:   {:?}\nRun with UPDATE_GOLDEN=1 if the change is intended.",
            path.display(),
            first + 1,
            expected.lines().nth(first).unwrap_or(""),
            actual.lines().nth(first).unwrap_or("")
        );
    }
}

/// Fold `streams` (one per source file, each in its own order) in forty
/// deterministic interleavings and assert the final model is identical every
/// time. Returns the baseline snapshot so a caller can assert semantic anchors
/// on it ("this agent ended `Done`").
pub(crate) fn assert_order_invariant(streams: impl Fn() -> Vec<Vec<Fact>>) -> Vec<String> {
    // Deterministic LCG so failures are reproducible.
    let interleave = |mut seed: u64| -> SessionModel {
        let mut queues = streams();
        let mut m = SessionModel::new("s".into());
        while queues.iter().any(|q| !q.is_empty()) {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let nonempty: Vec<usize> = (0..queues.len())
                .filter(|&i| !queues[i].is_empty())
                .collect();
            let pick = nonempty[(seed >> 33) as usize % nonempty.len()];
            let fact = queues[pick].remove(0);
            m.apply_fact(&fact);
        }
        m.recompute_group_status();
        m
    };
    let baseline = snapshot(&interleave(0));
    for seed in 1..40u64 {
        let got = snapshot(&interleave(seed));
        assert_eq!(got, baseline, "order-dependent state at seed {seed}");
    }
    baseline
}

/// Comparable snapshot of the model (spawn order is presentation — creation
/// order legitimately varies — so agents are compared by sorted id).
fn snapshot(m: &SessionModel) -> Vec<String> {
    m.agents
        .iter()
        .map(|(id, a)| {
            let tools: Vec<String> = a
                .tool_calls
                .iter()
                .map(|t| format!("{}:{:?}", t.id, t.state))
                .collect();
            let prov = m
                .provenance(a)
                .map(|c| format!("{:?}/{:?}", m.provenance_prompt(c), c.reasoning))
                .unwrap_or_default();
            format!(
                "{id}|{:?}|{:?}|{:?}|{:?}|{}|{prov}",
                a.kind,
                a.status,
                a.parent,
                a.spawned_by,
                tools.join(",")
            )
        })
        .collect()
}
