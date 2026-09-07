//! Providers: one module per transcript format, each turning its own
//! records into the shared [`Fact`](crate::fact::Fact) vocabulary. Everything
//! a format is *for*, what its records mean and how they join, lives here and
//! nowhere else; the model never learns a field name.
//!
//! # Vocabulary
//!
//! - **Core**: [`fact`](crate::fact), the model, the timeline, the UI. Consumes
//!   facts; knows no format.
//! - **Provider**: one per transcript format, `provider/<name>/`. Speaks the
//!   format and presents one uniform surface to everything else: a per-file
//!   stream whose `push(line)` yields a [`Statement`](crate::fact::Statement),
//!   a way to state a sidecar, and `discovery` for the files. (The issue
//!   thread calls this an adapter; same thing.)
//! - **Feeder**: one per way of getting bytes. The live tailer (polls files),
//!   the replay assembler (reads files up front), `inspect` (reads and prints),
//!   the browser's append (bytes from JS). Bytes in, a provider's stream in the
//!   middle, statements out to the core. Feeders know where bytes come from and
//!   nothing about what they mean. Today they name the Claude provider
//!   directly; a second provider makes that a choice.
//!
//! # Adding a provider
//!
//! A provider is one directory, `provider/<name>/`, with three halves:
//!
//! - `wire.rs` — the serde model for the format's records. Defensive: unknown
//!   record types, missing fields and malformed lines parse to something
//!   skippable, never a panic.
//! - `discovery.rs` — where the format keeps a session on disk and how one
//!   session's files are found. Pure path logic plus directory scans.
//! - `mod.rs` — the provider: a per-file `Stream` whose `push(line)` yields a
//!   [`Statement`](crate::fact::Statement) (the record's own time plus the
//!   facts it stated), and a way to state any sidecar.
//!
//! Its fixtures live in `assets/<name>/`, shaped like the real thing so
//! `discovery` finds them the way it finds a live session. Its conformance
//! test is one call, `harness::conform(name, fixture, streams)`, which checks
//! three things: the model reaches the same state however the files interleave
//! (§1.1), the folded model matches `assets/<name>/<fixture>.model.txt`, and
//! the dated timeline matches `assets/<name>/<fixture>.timeline.txt`. The two
//! goldens are generated with `UPDATE_GOLDEN=1` and are the human-readable
//! record of what the provider extracts. That is the whole contract; nothing
//! above `provider/` changes when one is added.
//!
//! Only the output is fixed. `Statement` and `Fact` are the contract, enforced
//! by the compiler; the rule below and the goldens enforce what a fact may
//! honestly claim. The input side — how bytes become records, and how a
//! session's files are found — is deliberately unconstrained: only the feeders
//! consume it, and they name the Claude provider directly. When a second
//! provider exists, the feeders become generic and the input contract is
//! written from what the two share, not guessed from one.
//!
//! A provider's own shape follows its format: the
//! Claude provider is a pure function per record because every Claude line is
//! self-contained, but a format whose records join across lines (Codex pairs a
//! tool call with the execution items between it and its output) will need a
//! struct that carries state between lines. That is expected, not a deviation.
//!
//! The one rule that decides what belongs here versus in the model:
//!
//! > A provider states what its records contain. The core decides what it
//! > means when nothing was recorded.

pub mod claude;

#[cfg(test)]
pub(crate) mod harness;
