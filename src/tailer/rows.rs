//! Incremental row reading: the `bytes` counterpart for a session that lives
//! in a database rather than files.
//!
//! A root read as [`ReadMode::Rows`](crate::provider::ReadMode::Rows) has no
//! bytes to tail. What it has is a provider that can present its rows as
//! lines and remember which it has shown; this is the feeder's handle on
//! that, and the one place the tailer names a provider's storage. Each
//! `poll` is the row analogue of reading appended bytes: the lines for
//! whatever landed since the last call, in the provider's own line format,
//! ready for the same [`Stream`](crate::provider::Stream) that reads an
//! export of the same session.

use crate::provider::bob;
use crate::provider::{Provider, SessionFile};

/// One stored session's cursor over its rows.
#[derive(Debug)]
pub enum RowSource {
    Bob(bob::db::RowSource),
}

impl RowSource {
    /// A cursor at the start of the session `root` names. Only a provider
    /// that stores sessions as rows produces such a root; the others never
    /// reach here, and a new one lists itself by failing to compile.
    pub fn new(root: &SessionFile) -> Self {
        match root.provider {
            Provider::Bob => RowSource::Bob(bob::db::RowSource::new(&root.path, &root.session)),
            Provider::Claude | Provider::Codex => {
                unreachable!("only a root read as rows has a row source")
            }
        }
    }

    /// The lines for what landed since the last call; the whole session on
    /// the first. Empty when nothing did, or the database could not be read
    /// this tick.
    pub fn poll(&mut self) -> Vec<String> {
        match self {
            RowSource::Bob(s) => s.poll(),
        }
    }
}
