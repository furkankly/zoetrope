//! The OMP / Pi session wire format: one JSON object per line, each with a
//! `type`. Records are read as loose JSON because the format is open-ended
//! (`custom` entries, per-version fields); every accessor returns `None` or an
//! empty slice instead of failing, so an unknown or malformed record is
//! skippable and never a panic.
//!
//! Shapes this provider reads (all others are ignored):
//!
//! - `title` / `title_change`: `{title}`. May precede the `session` header.
//! - `session`: `{id, timestamp, cwd, parentSession?}`, the header.
//! - `model_change`: `{model}`.
//! - `message`: `{message: {role, content[], usage?, model?, ...}}` with role
//!   `user`, `assistant` (content `text` / `thinking` / `toolCall`) or
//!   `toolResult` (`toolCallId`, `toolName`, `isError`, `details`).
//! - `custom_message` with `customType: "async-result"`: a delivered
//!   background job, `<task-result id=".." status="..">` in its text.

use chrono::{DateTime, Utc};
use serde_json::Value;

/// One parsed line.
#[derive(Debug, Clone)]
pub struct Record {
    pub v: Value,
}

/// Parse a line into a record. `None` for blank lines, invalid JSON, or
/// anything that is not an object with a string `type`.
pub fn parse_line(line: &str) -> Option<Record> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    v.get("type")?.as_str()?;
    Some(Record { v })
}

/// Whether a first record is an OMP/Pi one: the title sidecar line or the
/// session header. Content, never a path.
pub fn looks_like_omp(line: &str) -> bool {
    let Some(r) = parse_line(line) else {
        return false;
    };
    match r.kind() {
        "title" => r.v.get("v").is_some() || r.v.get("source").is_some(),
        "session" => r.v.get("version").is_some() && r.v.get("id").is_some(),
        _ => false,
    }
}

impl Record {
    pub fn kind(&self) -> &str {
        self.v.get("type").and_then(Value::as_str).unwrap_or("")
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.v.get(key).and_then(Value::as_str)
    }

    /// The record's own time: the entry `timestamp`, else the message's
    /// epoch-millisecond one.
    pub fn time(&self) -> Option<DateTime<Utc>> {
        if let Some(s) = self.str("timestamp")
            && let Ok(t) = DateTime::parse_from_rfc3339(s)
        {
            return Some(t.with_timezone(&Utc));
        }
        let ms = self.message()?.get("timestamp")?.as_i64()?;
        DateTime::from_timestamp_millis(ms)
    }

    pub fn message(&self) -> Option<&Value> {
        self.v.get("message").filter(|m| m.is_object())
    }

    pub fn role(&self) -> Option<&str> {
        self.message()?.get("role")?.as_str()
    }

    /// Message content parts. A bare string content becomes no parts; use
    /// [`Record::plain_text`] for it.
    pub fn parts(&self) -> &[Value] {
        self.message()
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// All `text` content of a message (string content or `text` parts).
    pub fn plain_text(&self) -> String {
        let Some(c) = self.message().and_then(|m| m.get("content")) else {
            return String::new();
        };
        if let Some(s) = c.as_str() {
            return s.to_string();
        }
        c.as_array()
            .map(|a| {
                a.iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    pub fn output_tokens(&self) -> Option<u64> {
        self.message()?.get("usage")?.get("output")?.as_u64()
    }
}

/// One `<task-result id=".." ... status="..">` from a delivered background
/// job's notice text: `(job id, status)`.
pub fn task_results(text: &str) -> Vec<(String, String)> {
    const OPEN: &str = "<task-result ";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(OPEN) {
        rest = &rest[at + OPEN.len()..];
        let tag_end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..tag_end];
        if let (Some(id), Some(status)) = (attr(tag, "id"), attr(tag, "status")) {
            out.push((id.to_string(), status.to_string()));
        }
    }
    out
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let pat = format!("{name}=\"");
    let mut from = 0;
    while let Some(i) = tag[from..].find(&pat) {
        let start = from + i;
        // Whole attribute name only (`id`, not `jobid`).
        if start == 0 || tag.as_bytes()[start - 1] == b' ' {
            let v = &tag[start + pat.len()..];
            return Some(&v[..v.find('"')?]);
        }
        from = start + pat.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_lines_are_skippable() {
        assert!(parse_line("").is_none());
        assert!(parse_line("{not json").is_none());
        assert!(parse_line("[1]").is_none());
        assert!(parse_line("{\"type\":7}").is_none());
        let r = parse_line("{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":5}}")
            .unwrap();
        assert!(r.parts().is_empty());
        assert_eq!(r.plain_text(), "");
    }

    #[test]
    fn detects_title_before_header() {
        assert!(looks_like_omp(
            r#"{"type":"title","v":1,"title":"x","source":"auto"}"#
        ));
        assert!(looks_like_omp(
            r#"{"type":"session","version":3,"id":"a","cwd":"/x"}"#
        ));
        assert!(!looks_like_omp(r#"{"type":"user","message":{}}"#));
        assert!(!looks_like_omp(r#"{"type":"session_meta","payload":{}}"#));
    }

    #[test]
    fn task_results_read_id_and_status() {
        let t = "x <task-result id=\"A\" agent=\"scout\" status=\"completed\" duration=\"1m\"> y <task-result id=\"B\" status=\"failed\">";
        assert_eq!(
            task_results(t),
            vec![
                ("A".into(), "completed".into()),
                ("B".into(), "failed".into())
            ]
        );
    }
}
