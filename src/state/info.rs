//! Session-level metadata, folded independently of the agent model.
//!
//! These facts carry no timestamp and aren't activity, so they stay OFF the
//! timeline (kept on `App`, surfaced in the `i` overlay and the `inspect`
//! header) rather than cluttering the graph. They arrive in file
//! (chronological) order, so latest-wins for the "current" values; tallies
//! accumulate. What the rows *are* is the provider's business: the provider
//! labels them, and this store only keeps them in first-seen order.

use crate::fact::{Fact, FactKind};

/// Session-level metadata that carries no timestamp and isn't activity.
#[derive(Debug, Default, Clone)]
pub struct SessionInfo {
    /// Header title. Session identity, not an event.
    pub title: Option<String>,
    /// Labelled values, in the order first seen; the latest value per label
    /// wins.
    pub fields: Vec<(String, String)>,
    /// Counters, in the order first seen.
    pub tallies: Vec<(String, u32)>,
}

impl SessionInfo {
    /// Fold one session-level fact. Anything else is ignored.
    pub fn apply(&mut self, fact: &Fact) {
        match &fact.kind {
            FactKind::Title(t) => self.title = Some(t.clone()),
            FactKind::Session { label, value } => {
                match self.fields.iter_mut().find(|(l, _)| l == label) {
                    Some((_, v)) => *v = value.clone(),
                    None => self.fields.push((label.clone(), value.clone())),
                }
            }
            FactKind::Tally(label) => match self.tallies.iter_mut().find(|(l, _)| l == label) {
                Some((_, n)) => *n += 1,
                None => self.tallies.push((label.clone(), 1)),
            },
            _ => {}
        }
    }

    /// The current value of a labelled field.
    pub fn field(&self, label: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, v)| v.as_str())
    }

    /// A tally's count (zero if never seen).
    pub fn tally(&self, label: &str) -> u32 {
        self.tallies
            .iter()
            .find(|(l, _)| l == label)
            .map_or(0, |(_, n)| *n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::claude::{Source, Stream};

    #[test]
    fn session_info_extracts_metadata_latest_wins() {
        let mut info = SessionInfo::default();
        let mut stream = Stream::new(Source::Main);
        let mut p = |s: &str| {
            if let Some(st) = stream.push(s) {
                st.facts.iter().for_each(|f| info.apply(f));
            }
        };
        p(r#"{"type":"ai-title","aiTitle":"Build the thing"}"#);
        p(r#"{"type":"mode","mode":"normal"}"#);
        p(r#"{"type":"permission-mode","permissionMode":"default"}"#);
        p(r#"{"type":"permission-mode","permissionMode":"acceptEdits"}"#);
        p(r#"{"type":"last-prompt","lastPrompt":"hey"}"#);
        p(r#"{"type":"queue-operation","operation":"enqueue"}"#);
        p(r#"{"type":"queue-operation","operation":"dequeue"}"#);
        p(r#"{"type":"file-history-snapshot","messageId":"x"}"#);
        p(r#"{"type":"file-history-snapshot","messageId":"y"}"#);

        assert_eq!(info.title.as_deref(), Some("Build the thing"));
        assert_eq!(info.field("mode"), Some("normal"));
        assert_eq!(info.field("permission"), Some("acceptEdits"), "latest wins");
        assert_eq!(info.field("last prompt"), Some("hey"));
        assert_eq!(info.tally("queued"), 1, "only enqueues counted");
        assert_eq!(info.tally("file edits"), 2);
        // Order is first-seen, so the overlay is stable across sessions.
        let labels: Vec<&str> = info.fields.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["mode", "permission", "last prompt"]);
    }
}
