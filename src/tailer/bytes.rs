//! Incremental byte reading.
//!
//! Stat a file, read bytes appended past the last offset, split on `\n`, hand
//! back complete lines, and buffer the trailing partial. Knows nothing about
//! any format: parsing is the provider's job. [`consume_bytes`] is pure over the
//! byte stream (no filesystem), so it is unit-testable directly.

use std::path::Path;

/// A single line longer than this (no newline yet) is treated as pathological:
/// the buffered prefix is dropped and parsing resyncs at the next newline, so a
/// newline-less or runaway line can never grow `partial` unbounded.
const MAX_PARTIAL: usize = 8 * 1024 * 1024;

/// Per-file tail position state.
///
/// `offset` is the byte position consumed so far; `partial` buffers a trailing
/// incomplete line until its newline arrives.
#[derive(Debug, Default)]
pub struct TailState {
    pub offset: u64,
    pub partial: Vec<u8>,
    /// Set when `partial` blew past [`MAX_PARTIAL`]: skip bytes until the next
    /// newline, then resync.
    overflowed: bool,
    /// `(dev, ino)` of the file last read, to catch replacement rotation where
    /// the new file is not shorter than the old offset (`None` off unix, or
    /// before the first read).
    identity: Option<(u64, u64)>,
}

/// Outcome of reading appended bytes from a file.
pub(crate) enum ReadResult {
    /// File missing or unreadable.
    Missing,
    /// No new bytes since last read.
    NoChange,
    /// File shrank (truncation/rotation) — state was reset to zero.
    Reset,
    /// Newly completed lines from appended bytes (CRLF-trimmed, blank lines
    /// dropped).
    Lines(Vec<String>),
}

/// Stat `path`, read any bytes appended past `state.offset`, and feed them
/// through [`consume_bytes`]. Detects truncation (`len < offset`) as well as
/// replacement by a different file (inode change, even to an equal-or-longer
/// one) and resets the state, returning [`ReadResult::Reset`].
pub(crate) fn read_appended(path: &Path, state: &mut TailState) -> ReadResult {
    use std::io::{Read, Seek, SeekFrom};

    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return ReadResult::Missing,
    };
    let len = metadata.len();
    let identity = file_identity(&metadata);
    let replaced = matches!((identity, state.identity), (Some(new), Some(old)) if new != old);

    if len < state.offset || replaced {
        // Truncation / rotation: reset everything.
        state.offset = 0;
        state.partial.clear();
        state.overflowed = false;
        state.identity = identity;
        return ReadResult::Reset;
    }
    if len == state.offset {
        return ReadResult::NoChange;
    }
    state.identity = identity;

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return ReadResult::Missing,
    };
    if file.seek(SeekFrom::Start(state.offset)).is_err() {
        return ReadResult::Missing;
    }
    let to_read = (len - state.offset) as usize;
    let mut buf = vec![0u8; to_read];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return ReadResult::Missing,
    };
    buf.truncate(n);
    state.offset += n as u64;

    ReadResult::Lines(consume_bytes(state, &buf))
}

/// Apply newly appended bytes to a [`TailState`], returning the parsed entries
/// from now-complete lines and buffering any trailing partial line.
///
/// Pure over the byte stream so it can be unit-tested without a filesystem.
pub(crate) fn consume_bytes(state: &mut TailState, appended: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut start = 0;

    for (i, &byte) in appended.iter().enumerate() {
        if byte == b'\n' {
            if state.overflowed {
                // End of the oversized line we were skipping — resync here.
                state.overflowed = false;
                state.partial.clear();
                start = i + 1;
                continue;
            }
            // Complete line = buffered partial + bytes up to (not incl.) '\n'.
            let line_bytes = &appended[start..i];
            if state.partial.is_empty() {
                if let Some(line) = line_of(line_bytes) {
                    lines.push(line);
                }
            } else {
                state.partial.extend_from_slice(line_bytes);
                if let Some(line) = line_of(&state.partial) {
                    lines.push(line);
                }
                state.partial.clear();
            }
            start = i + 1;
        }
    }

    // Buffer the trailing partial (no terminating newline yet) — unless we're
    // skipping a runaway line, or it would blow past the cap (drop + resync).
    if !state.overflowed && start < appended.len() {
        state.partial.extend_from_slice(&appended[start..]);
        if state.partial.len() > MAX_PARTIAL {
            state.partial.clear();
            state.overflowed = true;
        }
    }

    lines
}

/// `(dev, ino)` for rotation detection; `None` on platforms without inodes.
#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// A complete line from raw bytes, trimming a trailing `\r` (CRLF tolerance).
/// Invalid UTF-8 and blank lines are dropped.
fn line_of(bytes: &[u8]) -> Option<String> {
    let bytes = match bytes.last() {
        Some(b'\r') => &bytes[..bytes.len() - 1],
        _ => bytes,
    };
    let line = std::str::from_utf8(bytes).ok()?;
    (!line.trim().is_empty()).then(|| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_user(line: &str) -> bool {
        line.contains("\"type\":\"user\"")
    }

    #[test]
    fn consume_complete_lines() {
        let mut state = TailState::default();
        let data = b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n";
        let entries = consume_bytes(&mut state, data);
        assert_eq!(entries.len(), 2);
        assert!(state.partial.is_empty());
    }

    #[test]
    fn consume_partial_then_complete() {
        let mut state = TailState::default();
        // First chunk ends mid-line (no trailing newline).
        let first = consume_bytes(&mut state, b"{\"type\":\"us");
        assert!(first.is_empty());
        assert!(!state.partial.is_empty());
        // Second chunk completes the line.
        let second = consume_bytes(&mut state, b"er\"}\n");
        assert_eq!(second.len(), 1);
        assert!(is_user(&second[0]));
        assert!(state.partial.is_empty());
    }

    #[test]
    fn consume_partial_carried_across_three_chunks() {
        let mut state = TailState::default();
        assert!(consume_bytes(&mut state, b"{\"ty").is_empty());
        assert!(consume_bytes(&mut state, b"pe\":\"system").is_empty());
        let out = consume_bytes(&mut state, b"\"}\n");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("\"type\":\"system\""));
    }

    #[test]
    fn consume_multiple_with_trailing_partial() {
        let mut state = TailState::default();
        let data = b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n{\"type\":\"sys";
        let out = consume_bytes(&mut state, data);
        assert_eq!(out.len(), 2);
        assert_eq!(state.partial, b"{\"type\":\"sys");
    }

    #[test]
    fn consume_caps_a_runaway_partial_line() {
        let mut state = TailState::default();
        // A newline-less chunk past the cap is dropped, not buffered unbounded.
        let huge = vec![b'x'; super::MAX_PARTIAL + 1];
        let out = consume_bytes(&mut state, &huge);
        assert!(out.is_empty());
        assert!(state.partial.is_empty(), "oversized partial is dropped");

        // More of the runaway line (still no newline) stays dropped.
        consume_bytes(&mut state, b"more-garbage-no-newline");
        assert!(state.partial.is_empty());

        // The next newline resyncs; the following line comes through normally.
        let out = consume_bytes(&mut state, b"tail-of-garbage\n{\"type\":\"user\"}\n");
        assert_eq!(out.len(), 1, "resynced after the runaway line ended");
        assert!(is_user(&out[0]));
    }

    #[test]
    fn consume_returns_every_line_and_leaves_parsing_to_the_adapter() {
        let mut state = TailState::default();
        // The reader knows no format: a non-JSON line is a line like any other,
        // and dropping it is the provider's call.
        let out = consume_bytes(&mut state, b"not json at all\n{\"type\":\"user\"}\n");
        assert_eq!(out, ["not json at all", "{\"type\":\"user\"}"]);
    }

    #[test]
    fn consume_tolerates_crlf() {
        let mut state = TailState::default();
        let out = consume_bytes(&mut state, b"{\"type\":\"user\"}\r\n");
        assert_eq!(out.len(), 1);
        assert!(is_user(&out[0]));
    }

    #[test]
    fn read_appended_partial_then_complete_over_tempfile() {
        use std::io::Write;

        let mut tmp = std::env::temp_dir();
        tmp.push(format!("zoetrope_tail_test_{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        let mut state = TailState::default();

        // Write a partial line (no newline).
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"{\"type\":\"us").unwrap();
            f.flush().unwrap();
        }
        let r = read_appended(&tmp, &mut state);
        assert!(matches!(r, ReadResult::Lines(ref v) if v.is_empty()));
        assert!(!state.partial.is_empty());

        // Append the completion.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
            f.write_all(b"er\"}\n").unwrap();
            f.flush().unwrap();
        }
        let r = read_appended(&tmp, &mut state);
        match r {
            ReadResult::Lines(v) => {
                assert_eq!(v.len(), 1);
                assert!(is_user(&v[0]));
            }
            _ => panic!("expected entries"),
        }
        assert!(state.partial.is_empty());

        // No change → NoChange.
        assert!(matches!(
            read_appended(&tmp, &mut state),
            ReadResult::NoChange
        ));

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn read_appended_truncation_resets() {
        use std::io::Write;

        let mut tmp = std::env::temp_dir();
        tmp.push(format!("zoetrope_trunc_test_{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        let mut state = TailState::default();
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n")
                .unwrap();
        }
        let r = read_appended(&tmp, &mut state);
        assert!(matches!(r, ReadResult::Lines(ref v) if v.len() == 2));
        assert!(state.offset > 0);

        // Truncate to a shorter file → reset.
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"{\"type\":\"user\"}\n").unwrap();
        }
        let r = read_appended(&tmp, &mut state);
        assert!(matches!(r, ReadResult::Reset));
        assert_eq!(state.offset, 0);
        assert!(state.partial.is_empty());

        // Next read picks up from the start of the new (shorter) file.
        let r = read_appended(&tmp, &mut state);
        assert!(matches!(r, ReadResult::Lines(ref v) if v.len() == 1));

        let _ = std::fs::remove_file(&tmp);
    }

    // Rotation is detected by file identity, which only exists on unix:
    // `file_identity` returns None elsewhere, so a rotated-in longer file is
    // indistinguishable from an append on Windows. Truncation (`len < offset`)
    // is still caught everywhere, which the test above covers.
    #[cfg(unix)]
    #[test]
    fn read_appended_detects_rotation_to_longer_file() {
        use std::io::Write;

        let mut tmp = std::env::temp_dir();
        tmp.push(format!("zoetrope_rotate_test_{}.jsonl", std::process::id()));
        let mut incoming = std::env::temp_dir();
        incoming.push(format!("zoetrope_rotate_new_{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&incoming);

        let mut state = TailState::default();
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"{\"type\":\"user\"}\n").unwrap();
        }
        let r = read_appended(&tmp, &mut state);
        assert!(matches!(r, ReadResult::Lines(ref v) if v.len() == 1));

        // Replace with a DIFFERENT file (new inode) that is longer than the
        // old offset — the old `len < offset` check alone would read garbage
        // from mid-file. Renaming a second file over the path is what actually
        // rotates a log, and it is the only way to guarantee a new inode:
        // unlink-then-create lets the filesystem hand back the one just freed,
        // which ext4 routinely does.
        {
            let mut f = std::fs::File::create(&incoming).unwrap();
            f.write_all(b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n")
                .unwrap();
        }
        std::fs::rename(&incoming, &tmp).unwrap();
        let r = read_appended(&tmp, &mut state);
        assert!(matches!(r, ReadResult::Reset));
        assert_eq!(state.offset, 0);

        // Next read emits the WHOLE new file, not a mid-file suffix.
        let r = read_appended(&tmp, &mut state);
        assert!(matches!(r, ReadResult::Lines(ref v) if v.len() == 2));

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn truncation_reset_clears_runaway_line_skip() {
        use std::io::Write;

        let mut tmp = std::env::temp_dir();
        tmp.push(format!(
            "zoetrope_overflow_test_{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);

        // A runaway (newline-less, over-cap) line puts the state into
        // skip-until-newline mode.
        let mut state = TailState::default();
        consume_bytes(&mut state, &vec![b'x'; MAX_PARTIAL + 1]);
        state.offset = (MAX_PARTIAL + 1) as u64;

        // The file is then truncated and rewritten shorter.
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"{\"type\":\"user\"}\n").unwrap();
        }
        assert!(matches!(read_appended(&tmp, &mut state), ReadResult::Reset));

        // The new file's FIRST line must not be swallowed by the stale
        // overflow skip.
        let r = read_appended(&tmp, &mut state);
        assert!(matches!(r, ReadResult::Lines(ref v) if v.len() == 1));

        let _ = std::fs::remove_file(&tmp);
    }
}
