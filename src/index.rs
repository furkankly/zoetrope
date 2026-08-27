//! Skeleton index over transcript files — the cheap half of parsing.
//!
//! A transcript line is mostly payload, of which the timeline needs very
//! little: when it happened, what kind of entry it is, which agent it belongs
//! to, and how much tool activity it carries. Parsing every line in full to
//! learn that is slower than searching for it, and leaves the whole payload
//! resident afterwards. This module extracts the same facts with SIMD
//! substring probes into a fixed 32-byte [`Rec`] per line, and leaves the body
//! on disk until something actually renders it.
//!
//! `benches/index.rs` measures the difference against `parse_line`; no figure
//! is quoted here, because a number in a comment is a number from whichever
//! machine happened to write it.
//!
//! Two properties make the whole design work:
//!
//! - **Transcripts are append-only.** An index is therefore extendable: a file
//!   that grew is scanned from [`Index::scanned_len`], never from the start, and
//!   the result can be cached on disk across runs ([`Index::load`] / [`save`]).
//! - **The index never guesses.** Every probe either reads a fact out of the
//!   bytes or records that it could not, via [`flags::AMBIGUOUS_PROMPT`]. Where
//!   the format is ambiguous the caller parses that one line properly. A
//!   differential test (`index::tests::skeleton_agrees_with_parser`) asserts
//!   record-for-record agreement with [`crate::transcript::parse_line`], so the
//!   fast path can never silently drift from the real parser.
//!
//! [`save`]: Index::save

use std::path::{Path, PathBuf};

use memchr::memmem;

/// Serialized size of one [`Rec`] — so an index costs exactly 32 bytes per
/// line of transcript, whatever the lines contain.
pub const REC_SIZE: usize = 32;

/// Magic + version stamp at the head of a cached index. Bump the trailing digits
/// whenever [`Rec`]'s layout or any probe's meaning changes — a stale cache is
/// then rejected wholesale instead of being read as garbage.
const MAGIC: [u8; 8] = *b"ZOEIDX03";

/// Size of the cache header: magic, scanned length, head hash + its covered
/// length, record count.
const HEADER_SIZE: usize = 40;

/// Bytes of a file's head that the rotation guard hashes.
const HEAD_HASH_LEN: usize = 4096;

/// Sentinel `ts_micros` for a line with no parsable `timestamp`.
pub const UNDATED: i64 = i64::MIN;

/// Which entry a line is, mirroring [`crate::transcript::Entry`]'s variants.
///
/// Read from the `"type"` field alone, so it is exactly as reliable as serde's
/// tag dispatch — and [`Kind::Unknown`] covers anything new, the same way
/// `Entry::Unknown` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    User = 0,
    Assistant = 1,
    System = 2,
    Attachment = 3,
    AiTitle = 4,
    LastPrompt = 5,
    Mode = 6,
    PermissionMode = 7,
    FileHistorySnapshot = 8,
    QueueOperation = 9,
    Started = 10,
    Result = 11,
    Unknown = 12,
}

impl Kind {
    /// Map a raw `"type"` value to a kind. Unrecognized types are
    /// [`Kind::Unknown`], never an error.
    fn from_tag(tag: &[u8]) -> Kind {
        match tag {
            b"user" => Kind::User,
            b"assistant" => Kind::Assistant,
            b"system" => Kind::System,
            b"attachment" => Kind::Attachment,
            b"ai-title" => Kind::AiTitle,
            b"last-prompt" => Kind::LastPrompt,
            b"mode" => Kind::Mode,
            b"permission-mode" => Kind::PermissionMode,
            b"file-history-snapshot" => Kind::FileHistorySnapshot,
            b"queue-operation" => Kind::QueueOperation,
            b"started" => Kind::Started,
            b"result" => Kind::Result,
            _ => Kind::Unknown,
        }
    }

    fn from_u8(b: u8) -> Kind {
        match b {
            0 => Kind::User,
            1 => Kind::Assistant,
            2 => Kind::System,
            3 => Kind::Attachment,
            4 => Kind::AiTitle,
            5 => Kind::LastPrompt,
            6 => Kind::Mode,
            7 => Kind::PermissionMode,
            8 => Kind::FileHistorySnapshot,
            9 => Kind::QueueOperation,
            10 => Kind::Started,
            11 => Kind::Result,
            _ => Kind::Unknown,
        }
    }

    /// Whether this kind's entry carries the shared envelope (`uuid`,
    /// `parentUuid`, `timestamp`).
    ///
    /// Flat metadata and ledger lines do not — and some of them carry a
    /// `timestamp` field in the file that the parser's types have nowhere to
    /// put, so it never reaches the model. The index reports what the parser
    /// reports, so those lines are [`UNDATED`] here too.
    pub fn has_envelope(self) -> bool {
        matches!(
            self,
            Kind::User | Kind::Assistant | Kind::System | Kind::Attachment
        )
    }

    /// Whether the model folds this kind onto the timeline at all. Mirrors
    /// [`crate::transcript::Entry::is_timeline_noise`], inverted.
    pub fn is_timeline_item(self) -> bool {
        !matches!(
            self,
            Kind::AiTitle
                | Kind::LastPrompt
                | Kind::Mode
                | Kind::PermissionMode
                | Kind::FileHistorySnapshot
                | Kind::QueueOperation
        )
    }
}

/// Bit meanings for [`Rec::flags`].
pub mod flags {
    /// The line carries `origin.kind == "human"` — an authoritative prompt.
    pub const HUMAN_ORIGIN: u8 = 1 << 0;
    /// The line carries `origin.kind == "task-notification"` — authoritatively
    /// NOT a prompt.
    pub const TASK_NOTIFICATION: u8 = 1 << 1;
    /// `isSidechain: true` — the line belongs to a subagent's own thread.
    pub const SIDECHAIN: u8 = 1 << 2;
    /// A `user` line with no `origin` field at all: a legacy transcript, where
    /// "is this a prompt" is decided by a text heuristic the skeleton cannot
    /// replicate. The caller must parse this one line to decide. The index
    /// records that it does not know rather than guessing.
    pub const AMBIGUOUS_PROMPT: u8 = 1 << 3;
}

/// One indexed line: where it is, when it happened, and the handful of counts
/// the timeline geometry needs — without the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rec {
    /// Byte offset of the line's first byte within its file.
    pub offset: u64,
    /// Microseconds since the Unix epoch, or [`UNDATED`].
    pub ts_micros: i64,
    /// Length of the line in bytes, excluding the newline.
    pub len: u32,
    /// FNV-1a hash of the line's `agentId`, or 0 when absent. A stable,
    /// process-independent hash — the cache on disk outlives the run that
    /// wrote it, so [`std::collections::hash_map::DefaultHasher`] (randomly
    /// seeded per process) would be wrong here.
    pub agent_key: u32,
    /// Which entry this is.
    pub kind: Kind,
    /// `tool_use` blocks in the line.
    pub tool_uses: u8,
    /// Agent/workflow spawn calls in the line (`Agent` / `Task` / `Workflow`).
    pub spawns: u8,
    /// `tool_result` blocks flagged `is_error: true`.
    pub failures: u8,
    /// See [`flags`].
    pub flags: u8,
}

impl Rec {
    /// The line's timestamp, or `None` when undated.
    pub fn ts(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        if self.ts_micros == UNDATED {
            return None;
        }
        chrono::DateTime::from_timestamp_micros(self.ts_micros)
    }

    /// Slice this line out of the file's bytes, for the caller to parse fully.
    pub fn slice<'a>(&self, file: &'a [u8]) -> &'a [u8] {
        let start = self.offset as usize;
        let end = (start + self.len as usize).min(file.len());
        &file[start.min(file.len())..end]
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.ts_micros.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.agent_key.to_le_bytes());
        out.push(self.kind as u8);
        out.push(self.tool_uses);
        out.push(self.spawns);
        out.push(self.failures);
        out.push(self.flags);
        out.extend_from_slice(&[0u8; 3]);
    }

    fn decode(b: &[u8]) -> Rec {
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let i64_at = |i: usize| i64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        Rec {
            offset: u64_at(0),
            ts_micros: i64_at(8),
            len: u32_at(16),
            agent_key: u32_at(20),
            kind: Kind::from_u8(b[24]),
            tool_uses: b[25],
            spawns: b[26],
            failures: b[27],
            flags: b[28],
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level field scan
// ---------------------------------------------------------------------------
//
// The envelope fields must be read from the line's TOP LEVEL, not from wherever
// the name first appears. Claude Code does not emit envelope keys first, and a
// nested object can carry the same names — an `attachment` entry serializes
// `"attachment":{"type":…}` BEFORE its own `"type":"attachment"`, so taking the
// first `"type"` in the line reads the attachment's type as the entry's kind.
// (Found by the real-corpus differential test; the synthetic fixtures all
// happened to put `type` first.) The same trap applies to `timestamp` and
// `agentId`, which appear inside attachment payloads and tool results.
//
// So: one structural pass that walks depth-1 keys and skips nested values
// wholesale. It stays fast by using memchr to jump between structural bytes
// rather than stepping through string contents.

/// Index just past the string starting at `at` (which must be its opening
/// quote), honouring backslash escapes.
fn skip_string(line: &[u8], at: usize) -> Option<usize> {
    let mut i = at + 1;
    loop {
        let q = i + memchr::memchr(b'"', line.get(i..)?)?;
        // A quote is the terminator only if preceded by an EVEN number of
        // backslashes (`\\"` ends the string, `\"` does not).
        let mut backslashes = 0;
        let mut j = q;
        while j > at + 1 && line[j - 1] == b'\\' {
            backslashes += 1;
            j -= 1;
        }
        if backslashes % 2 == 0 {
            return Some(q + 1);
        }
        i = q + 1;
    }
}

/// Index just past the `{…}` or `[…]` container starting at `at`.
fn skip_container(line: &[u8], at: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = at;
    while i < line.len() {
        let pos = i + memchr::memchr3(b'"', open, close, line.get(i..)?)?;
        let c = line[pos];
        if c == b'"' {
            i = skip_string(line, pos)?;
        } else if c == open {
            depth += 1;
            i = pos + 1;
        } else {
            depth -= 1;
            if depth == 0 {
                return Some(pos + 1);
            }
            i = pos + 1;
        }
    }
    None
}

/// Index just past the JSON value starting at `at`.
fn skip_value(line: &[u8], at: usize) -> Option<usize> {
    match *line.get(at)? {
        b'"' => skip_string(line, at),
        b'{' => skip_container(line, at, b'{', b'}'),
        b'[' => skip_container(line, at, b'[', b']'),
        // A literal (number, `true`, `false`, `null`) ends at the next
        // separator; none of them can contain one.
        _ => {
            let rel = line[at..]
                .iter()
                .position(|&c| c == b',' || c == b'}' || c == b']')?;
            Some(at + rel)
        }
    }
}

/// A JSON string's contents, without its quotes. `None` when the value is not a
/// string, or contains an escape.
///
/// Escaped values are refused rather than mis-read: every envelope field read
/// here (`type`, `agentId`, `timestamp`) is a machine-generated identifier that
/// never contains one, and refusing degrades to "unknown", never to a wrong
/// answer.
fn unquote(value: &[u8]) -> Option<&[u8]> {
    let inner = value.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    (!inner.contains(&b'\\')).then_some(inner)
}

/// Iterator over the top-level `(key, value)` pairs of a JSON object slice.
///
/// Nested values are skipped wholesale, so a key at depth 2 is never mistaken
/// for one at depth 1. Malformed input ends the iteration rather than erroring:
/// this is a fast path over data the real parser validates, so it degrades to
/// "less known", never to a wrong answer.
struct ObjectFields<'a> {
    buf: &'a [u8],
    i: usize,
}

/// Walk the top-level keys of the object beginning at the first `{` in `buf`.
fn object_fields(buf: &[u8]) -> ObjectFields<'_> {
    let i = memchr::memchr(b'{', buf).map_or(buf.len(), |o| o + 1);
    ObjectFields { buf, i }
}

impl<'a> Iterator for ObjectFields<'a> {
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let rel = memchr::memchr2(b'"', b'}', self.buf.get(self.i..)?)?;
        let key_start = self.i + rel;
        if self.buf[key_start] == b'}' {
            return None;
        }
        let key_end = skip_string(self.buf, key_start)?;
        let key = unquote(&self.buf[key_start..key_end])?;

        // `"key" : value` — whitespace may follow the colon.
        let colon = key_end + memchr::memchr(b':', self.buf.get(key_end..)?)?;
        let mut value_start = colon + 1;
        while self
            .buf
            .get(value_start)
            .is_some_and(u8::is_ascii_whitespace)
        {
            value_start += 1;
        }
        let value_end = skip_value(self.buf, value_start)?;

        self.i = value_end;
        Some((key, &self.buf[value_start..value_end]))
    }
}

/// Iterator over the top-level elements of a JSON array slice.
struct ArrayElements<'a> {
    buf: &'a [u8],
    i: usize,
}

/// Walk the elements of the array beginning at the first `[` in `buf`.
fn array_elements(buf: &[u8]) -> ArrayElements<'_> {
    let i = memchr::memchr(b'[', buf).map_or(buf.len(), |o| o + 1);
    ArrayElements { buf, i }
}

impl<'a> Iterator for ArrayElements<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        while self
            .buf
            .get(self.i)
            .is_some_and(|c| c.is_ascii_whitespace() || *c == b',')
        {
            self.i += 1;
        }
        if *self.buf.get(self.i)? == b']' {
            return None;
        }
        let end = skip_value(self.buf, self.i)?;
        let start = self.i;
        self.i = end;
        Some(&self.buf[start..end])
    }
}

/// The envelope fields of one line, read from its top level only.
#[derive(Default)]
struct EnvelopeFields<'a> {
    ty: Option<&'a [u8]>,
    timestamp: Option<&'a [u8]>,
    agent_id: Option<&'a [u8]>,
    is_sidechain: bool,
    /// The raw `origin` value, and whether the key was present at all — an
    /// absent `origin` is what makes a legacy `user` line ambiguous.
    origin: Option<&'a [u8]>,
    saw_origin: bool,
    /// The raw `message` value, captured here so [`count_blocks`] does not walk
    /// the line a second time to re-find it. It is the largest value on the
    /// line, and the envelope walk has already paid to skip past it.
    message: Option<&'a [u8]>,
}

/// Walk the top-level keys of `line` and collect the envelope fields.
///
/// Malformed input simply yields whatever was read before the scan gave up:
/// this is a fast path over data the real parser validates, so it degrades to
/// "less known", never to an error.
fn scan_envelope(line: &[u8]) -> EnvelopeFields<'_> {
    let mut out = EnvelopeFields::default();
    for (key, value) in object_fields(line) {
        match key {
            b"type" => out.ty = unquote(value),
            b"timestamp" => out.timestamp = unquote(value),
            b"agentId" => out.agent_id = unquote(value),
            b"isSidechain" => out.is_sidechain = value == b"true",
            b"origin" => {
                out.saw_origin = true;
                out.origin = Some(value);
            }
            b"message" => out.message = Some(value),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

/// Per-line activity counts, taken from the actual `message.content` blocks.
///
/// These cannot be byte-counted across the whole line, for two reasons the
/// real-corpus differential test surfaced. The parser scopes each count to an
/// entry kind — `tool_use_count` and `spawn_count` read assistant content,
/// `tool_failure_count` reads user content — and a line's *text* can contain
/// any of these literals: a `user` line quoting a tool result that mentioned
/// `"name":"Agent"` six times counted six spawns against the parser's zero.
#[derive(Default)]
struct BlockCounts {
    tool_uses: u8,
    spawns: u8,
    failures: u8,
}

/// Count the content blocks of a line's `message` value, scoped exactly as the
/// parser scopes them.
///
/// `message.content` is string-or-array; only the array form holds blocks, and
/// a bare string yields zero of everything — which is what the parser reports.
fn count_blocks(message: Option<&[u8]>, kind: Kind) -> BlockCounts {
    let mut out = BlockCounts::default();
    // Only these two kinds carry countable content, and each contributes only
    // its own counts — mirroring `Entry::{tool_use_count, spawn_count,
    // tool_failure_count}`, which match on the variant before counting.
    if !matches!(kind, Kind::Assistant | Kind::User) {
        return out;
    }
    let Some(message) = message else {
        return out;
    };
    let Some((_, content)) = object_fields(message).find(|(k, _)| *k == b"content") else {
        return out;
    };
    if !content.starts_with(b"[") {
        return out;
    }

    // The block type this kind counts; anything else is skipped without reading
    // the rest of its keys (a `tool_use`'s `input` is the biggest value on the
    // line, and there is no reason to walk it).
    let wanted: &[u8] = match kind {
        Kind::Assistant => b"tool_use",
        _ => b"tool_result",
    };

    for block in array_elements(content) {
        let (mut ty, mut name, mut is_error) = (None, None, false);
        for (key, value) in object_fields(block) {
            match key {
                b"type" => {
                    ty = unquote(value);
                    if ty != Some(wanted) {
                        break;
                    }
                }
                b"name" => name = unquote(value),
                b"is_error" => is_error = value == b"true",
                _ => {}
            }
        }
        match (kind, ty) {
            (Kind::Assistant, Some(b"tool_use")) => {
                out.tool_uses = out.tool_uses.saturating_add(1);
                // Single source for the spawn rule, shared with the parser.
                if name
                    .and_then(|n| std::str::from_utf8(n).ok())
                    .is_some_and(crate::transcript::is_spawn_tool)
                {
                    out.spawns = out.spawns.saturating_add(1);
                }
            }
            (Kind::User, Some(b"tool_result")) if is_error => {
                out.failures = out.failures.saturating_add(1);
            }
            _ => {}
        }
    }
    out
}

/// FNV-1a (32-bit). Chosen for being fully specified and seed-free, so a hash
/// written into a cache file means the same thing when read back next week.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// FNV-1a (64-bit), for the file-head rotation guard.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff...]Z` into microseconds since the epoch.
///
/// The fast path handles the canonical shape Claude Code writes, digit by
/// digit, and hands the civil-date arithmetic to chrono (leap years and epoch
/// offsets are not arithmetic worth re-deriving). Anything else — an offset
/// other than `Z`, a different precision, a format we have not seen — falls
/// back to chrono's full RFC-3339 parser, so an unusual stamp is read
/// correctly rather than dropped.
fn parse_ts(s: &[u8]) -> Option<i64> {
    fn digits(b: &[u8]) -> Option<u32> {
        let mut n: u32 = 0;
        for &c in b {
            if !c.is_ascii_digit() {
                return None;
            }
            n = n * 10 + (c - b'0') as u32;
        }
        Some(n)
    }

    let canonical = s.len() >= 20
        && s[4] == b'-'
        && s[7] == b'-'
        && s[10] == b'T'
        && s[13] == b':'
        && s[16] == b':';

    if canonical {
        let micros = match s.len() {
            // `...SSZ`
            20 if s[19] == b'Z' => Some(0),
            // `...SS.fffZ` — the shape every observed transcript uses.
            24 if s[19] == b'.' && s[23] == b'Z' => digits(&s[20..23]).map(|ms| ms * 1000),
            _ => None,
        };
        if let Some(micro) = micros {
            let (y, mo, d) = (digits(&s[0..4])?, digits(&s[5..7])?, digits(&s[8..10])?);
            let (h, mi, sec) = (
                digits(&s[11..13])?,
                digits(&s[14..16])?,
                digits(&s[17..19])?,
            );
            let date = chrono::NaiveDate::from_ymd_opt(y as i32, mo, d)?;
            let dt = date.and_hms_micro_opt(h, mi, sec, micro)?;
            return Some(dt.and_utc().timestamp_micros());
        }
    }

    // Slow path: anything non-canonical.
    let text = std::str::from_utf8(s).ok()?;
    Some(
        chrono::DateTime::parse_from_rfc3339(text)
            .ok()?
            .timestamp_micros(),
    )
}

/// Extract one line's [`Rec`]. `offset` is the line's position in its file.
///
/// Blank lines yield `None` — they are not entries and must not become records
/// (the parser skips them too).
fn index_line(line: &[u8], offset: u64) -> Option<Rec> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }

    let env = scan_envelope(line);
    let kind = env.ty.map_or(Kind::Unknown, Kind::from_tag);

    let ts_micros = if kind.has_envelope() {
        env.timestamp.and_then(parse_ts).unwrap_or(UNDATED)
    } else {
        UNDATED
    };

    let agent_key = env.agent_id.map_or(0, fnv1a);

    let mut flag_bits = 0u8;
    if env.is_sidechain {
        flag_bits |= flags::SIDECHAIN;
    }
    // Prompt provenance. `origin.kind` is authoritative where present; a `user`
    // line without it is a legacy transcript whose prompt-ness only the text
    // heuristic in `UserEntry::is_human_prompt` can settle, so say so. The
    // search is scoped to the `origin` value — `"kind"` is a common enough key
    // that a line-wide search would read someone else's.
    match env.origin {
        Some(origin) => {
            if memmem::find(origin, b"\"human\"").is_some() {
                flag_bits |= flags::HUMAN_ORIGIN;
            } else if memmem::find(origin, b"\"task-notification\"").is_some() {
                flag_bits |= flags::TASK_NOTIFICATION;
            }
        }
        None if kind == Kind::User => flag_bits |= flags::AMBIGUOUS_PROMPT,
        None => {}
    }
    debug_assert!(
        env.saw_origin == env.origin.is_some(),
        "origin presence and value must agree"
    );

    let counts = count_blocks(env.message, kind);

    Some(Rec {
        offset,
        ts_micros,
        len: line.len() as u32,
        agent_key,
        kind,
        tool_uses: counts.tool_uses,
        spawns: counts.spawns,
        failures: counts.failures,
        flags: flag_bits,
    })
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

/// The skeleton index of one transcript file.
#[derive(Debug, Default, Clone)]
pub struct Index {
    /// One record per non-blank line, in file order.
    pub recs: Vec<Rec>,
    /// Bytes consumed so far, always ending at a newline. A later append is
    /// scanned from exactly here — never from the start of the file.
    pub scanned_len: u64,
    /// FNV-1a of the file's first [`Index::head_len`] bytes. Transcripts are
    /// append-only, so a changed head means the file was rotated or rewritten
    /// and the index must be discarded.
    pub head_hash: u64,
    /// How many bytes [`head_hash`](Index::head_hash) covers. Recorded rather
    /// than assumed: a file indexed while it was still shorter than
    /// [`HEAD_HASH_LEN`] must be re-hashed over that same short prefix, or it
    /// would appear rotated on its very next append.
    pub head_len: u64,
}

impl Index {
    /// Scan `bytes` — the whole file — building or extending this index.
    ///
    /// Only the region past [`scanned_len`](Index::scanned_len) is read, and a
    /// trailing partial line (no newline yet: the writer is mid-append) is left
    /// unconsumed for the next call.
    pub fn extend(&mut self, bytes: &[u8]) {
        // Grow the guard as the file grows, up to the full window: a session
        // indexed at 200 bytes gets a 200-byte guard now and a 4 KiB one once
        // there are 4 KiB to hash.
        let want = bytes.len().min(HEAD_HASH_LEN) as u64;
        if want > self.head_len {
            self.head_len = want;
            self.head_hash = fnv1a64(&bytes[..want as usize]);
        }
        let start = self.scanned_len as usize;
        if start >= bytes.len() {
            return;
        }
        let tail = &bytes[start..];
        let mut line_start = 0usize;
        for nl in memchr::memchr_iter(b'\n', tail) {
            let line = &tail[line_start..nl];
            if let Some(rec) = index_line(line, (start + line_start) as u64) {
                self.recs.push(rec);
            }
            line_start = nl + 1;
        }
        self.scanned_len = (start + line_start) as u64;
    }

    /// Whether `bytes` is a valid continuation of what this index already
    /// covers: same head, and no shorter than what was scanned. A `false` here
    /// means rotation or truncation — rebuild from scratch.
    pub fn continues(&self, bytes: &[u8]) -> bool {
        (bytes.len() as u64) >= self.scanned_len
            && (bytes.len() as u64) >= self.head_len
            && self.head_hash == fnv1a64(&bytes[..self.head_len as usize])
    }

    /// Serialize to the on-disk cache format.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_SIZE + self.recs.len() * REC_SIZE);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.scanned_len.to_le_bytes());
        out.extend_from_slice(&self.head_hash.to_le_bytes());
        out.extend_from_slice(&self.head_len.to_le_bytes());
        out.extend_from_slice(&(self.recs.len() as u64).to_le_bytes());
        for r in &self.recs {
            r.encode(&mut out);
        }
        out
    }

    /// Parse the on-disk cache format. `None` for a foreign, truncated, or
    /// version-mismatched file — every one of which means "rebuild", never an
    /// error worth surfacing.
    fn decode(bytes: &[u8]) -> Option<Index> {
        if bytes.len() < HEADER_SIZE || bytes[..8] != MAGIC {
            return None;
        }
        let u64_at = |i: usize| u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
        let scanned_len = u64_at(8);
        let head_hash = u64_at(16);
        let head_len = u64_at(24);
        let count = u64_at(32) as usize;
        if bytes.len() < HEADER_SIZE + count * REC_SIZE {
            return None;
        }
        let recs = (0..count)
            .map(|i| {
                let at = HEADER_SIZE + i * REC_SIZE;
                Rec::decode(&bytes[at..at + REC_SIZE])
            })
            .collect();
        Some(Index {
            recs,
            scanned_len,
            head_hash,
            head_len,
        })
    }

    /// Read the cached index for `transcript`, if one exists and parses.
    pub fn load(transcript: &Path) -> Option<Index> {
        let bytes = std::fs::read(cache_path(transcript)?).ok()?;
        Index::decode(&bytes)
    }

    /// Write this index to the cache. Best-effort: a full disk or an unwritable
    /// cache directory costs a re-scan next run, nothing more, so failures are
    /// swallowed rather than propagated into the UI.
    pub fn save(&self, transcript: &Path) {
        let Some(path) = cache_path(transcript) else {
            return;
        };
        let Some(dir) = path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        // Write-then-rename so a killed process can never leave a half-written
        // index that the next run would read as valid.
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, self.encode()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Cache location for a transcript's index: `~/.cache/zoetrope/idx/<hash>.idx`.
///
/// Keyed by a hash of the absolute path rather than the session uuid — two
/// projects can hold transcripts with the same stem only by accident, but the
/// cache must be correct even then.
fn cache_path(transcript: &Path) -> Option<PathBuf> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            #[allow(deprecated)]
            std::env::home_dir()
                .filter(|h| !h.as_os_str().is_empty())
                .map(|h| h.join(".cache"))
        })?;
    let key = fnv1a64(transcript.as_os_str().as_encoded_bytes());
    Some(
        root.join("zoetrope")
            .join("idx")
            .join(format!("{key:016x}.idx")),
    )
}

/// Build (or refresh from cache) the index for a transcript on disk.
///
/// The file is mmapped, so an already-cached index costs a stat and a header
/// read rather than a copy of the transcript. Returns the index and the mapped
/// bytes, so the caller can slice bodies out of the same mapping.
pub fn open(transcript: &Path) -> std::io::Result<(Index, memmap2::Mmap)> {
    let file = std::fs::File::open(transcript)?;
    // SAFETY: mmap of a file another process appends to. Appends never move or
    // rewrite existing bytes, and the index only ever reads the prefix it has
    // already measured, so a concurrent append cannot invalidate a slice in use.
    // Truncation would — which is exactly what `continues` checks for.
    let map = unsafe { memmap2::Mmap::map(&file)? };

    let mut index = match Index::load(transcript) {
        Some(cached) if cached.continues(&map) => cached,
        _ => Index::default(),
    };
    let before = index.scanned_len;
    index.extend(&map);
    if index.scanned_len != before {
        index.save(transcript);
    }
    Ok((index, map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{self, Entry};

    /// The entry kind the real parser assigns to a line.
    fn parser_kind(line: &str) -> Option<Kind> {
        Some(match transcript::parse_line(line)? {
            Entry::User(_) => Kind::User,
            Entry::Assistant(_) => Kind::Assistant,
            Entry::System(_) => Kind::System,
            Entry::Attachment(_) => Kind::Attachment,
            Entry::AiTitle(_) => Kind::AiTitle,
            Entry::LastPrompt(_) => Kind::LastPrompt,
            Entry::Mode(_) => Kind::Mode,
            Entry::PermissionMode(_) => Kind::PermissionMode,
            Entry::FileHistorySnapshot(_) => Kind::FileHistorySnapshot,
            Entry::QueueOperation(_) => Kind::QueueOperation,
            Entry::Started(_) => Kind::Started,
            Entry::Result(_) => Kind::Result,
            Entry::Unknown => Kind::Unknown,
        })
    }

    /// Assert that the skeleton index reports exactly what the real parser
    /// would, for every line of `text`. This is the contract the whole module
    /// rests on: the fast path is only allowed to be faster, never different.
    pub(crate) fn assert_agrees(text: &str) {
        let found = check_agrees(text);
        assert!(
            found.is_empty(),
            "{} disagreement(s):\n{}",
            found.len(),
            found.join("\n")
        );
    }

    /// One disagreement between the skeleton and the parser.
    pub(crate) struct Mismatch {
        /// What disagreed (`kind`, `timestamp`, `tool_use count`, …) — used to
        /// group findings so one systematic bug reads as one bug.
        pub(crate) field: &'static str,
        pub(crate) detail: String,
    }

    impl std::fmt::Display for Mismatch {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}: {}", self.field, self.detail)
        }
    }

    /// Every disagreement between the skeleton and the parser over `text`.
    ///
    /// Collects rather than panicking on the first: a corpus sweep that aborts
    /// on line one costs a full re-run per bug found, and systematic format
    /// drift usually shows up as many instances of a few causes.
    pub(crate) fn check_agrees_detailed(text: &str) -> Vec<Mismatch> {
        let mut out = Vec::new();
        let mut index = Index::default();
        index.extend(text.as_bytes());

        let lines: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter(|l| transcript::parse_line(l).is_some())
            .collect();

        // A line the parser rejects outright still gets a record (the skeleton
        // cannot know it is malformed), so compare on the parseable subset.
        let recs: Vec<&Rec> = index
            .recs
            .iter()
            .filter(|r| {
                std::str::from_utf8(r.slice(text.as_bytes()))
                    .ok()
                    .and_then(transcript::parse_line)
                    .is_some()
            })
            .collect();
        if recs.len() != lines.len() {
            out.push(Mismatch {
                field: "record count",
                detail: format!("{} records vs {} parseable lines", recs.len(), lines.len()),
            });
            return out;
        }

        let mut note = |field: &'static str, line: &str, detail: String| {
            out.push(Mismatch {
                field,
                detail: format!("{detail} — {line:.160}"),
            });
        };

        for (rec, line) in recs.iter().zip(&lines) {
            // The slice must reproduce the line exactly — offsets are the
            // foundation everything else sits on.
            let sliced = std::str::from_utf8(rec.slice(text.as_bytes())).unwrap_or("<non-utf8>");
            if sliced != *line {
                note("slice", line, format!("offset {}", rec.offset));
                continue;
            }

            let Some(entry) = transcript::parse_line(line) else {
                continue;
            };
            if Some(rec.kind) != parser_kind(line) {
                note(
                    "kind",
                    line,
                    format!("{:?} vs {:?}", rec.kind, parser_kind(line)),
                );
            }

            // Timestamp: identical to what serde + chrono produced.
            let expect_ts = match &entry {
                Entry::User(e) => e.envelope.timestamp,
                Entry::Assistant(e) => e.envelope.timestamp,
                Entry::System(e) => e.envelope.timestamp,
                Entry::Attachment(e) => e.envelope.timestamp,
                _ => None,
            };
            if rec.ts() != expect_ts {
                note(
                    "timestamp",
                    line,
                    format!("{:?} vs {expect_ts:?}", rec.ts()),
                );
            }

            // Counts drive the scrubber's markers — they must match exactly.
            for (field, got, want) in [
                (
                    "tool_use count",
                    rec.tool_uses as usize,
                    entry.tool_use_count(),
                ),
                ("spawn count", rec.spawns as usize, entry.spawn_count()),
                (
                    "failure count",
                    rec.failures as usize,
                    entry.tool_failure_count(),
                ),
            ] {
                if got != want {
                    note(field, line, format!("{got} vs {want}"));
                }
            }
            if rec.kind.is_timeline_item() == entry.is_timeline_noise() {
                note("timeline-item", line, format!("{:?}", rec.kind));
            }

            // Prompt provenance: where the index claims to know, it must be
            // right; where it flags ambiguity, no claim is made.
            if let Entry::User(u) = &entry {
                let ambiguous = rec.flags & flags::AMBIGUOUS_PROMPT != 0;
                if !ambiguous && rec.flags & flags::HUMAN_ORIGIN != 0 && !u.is_human_prompt() {
                    note("human-origin", line, "claimed human prompt".to_string());
                }
                if rec.flags & flags::TASK_NOTIFICATION != 0 && u.is_human_prompt() {
                    note("task-notification", line, "claimed non-prompt".to_string());
                }
            }
        }
        out
    }

    /// [`check_agrees_detailed`], rendered as strings.
    pub(crate) fn check_agrees(text: &str) -> Vec<String> {
        check_agrees_detailed(text)
            .into_iter()
            .map(|m| m.to_string())
            .collect()
    }

    #[test]
    fn skeleton_agrees_with_parser_on_the_demo_session() {
        assert_agrees(include_str!("../assets/demo.jsonl"));
    }

    /// Differential check against a REAL corpus — opt-in, never run by default.
    ///
    /// The synthetic fixtures only prove agreement on shapes this repository
    /// already knows about. Transcripts in the wild carry format drift no
    /// fixture anticipates, and the fast path must agree there too. Point this
    /// at a directory tree of transcripts to find out:
    ///
    /// ```text
    /// ZOE_DIFF_CORPUS=~/.claude/projects \
    ///   cargo test --lib index -- --ignored --nocapture
    /// ```
    ///
    /// It reads only; it reports only aggregate counts.
    #[test]
    #[ignore = "reads real transcripts; opt in with ZOE_DIFF_CORPUS"]
    fn skeleton_agrees_with_parser_on_a_real_corpus() {
        let Some(root) = std::env::var_os("ZOE_DIFF_CORPUS") else {
            eprintln!("ZOE_DIFF_CORPUS not set — nothing to compare against");
            return;
        };
        let root = PathBuf::from(root);

        // Two levels of `.jsonl` under the root, plus the sidecar trees.
        fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
            if depth > 4 {
                return;
            }
            let Ok(read) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, out, depth + 1);
                } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    out.push(path);
                }
            }
        }

        let mut files = Vec::new();
        collect(&root, &mut files, 0);
        files.sort();
        assert!(
            !files.is_empty(),
            "no .jsonl found under {}",
            root.display()
        );

        let (mut lines, mut bytes, mut skipped) = (0usize, 0usize, 0usize);
        // Group by (field, file): one systematic bug then reads as one finding
        // per file rather than thousands of identical lines.
        let mut by_field: std::collections::BTreeMap<&'static str, (usize, String)> =
            std::collections::BTreeMap::new();

        for path in &files {
            let Ok(text) = std::fs::read_to_string(path) else {
                // Non-UTF-8 is not a disagreement; the byte path handles it and
                // the parser would reject it. Count it and move on.
                skipped += 1;
                continue;
            };
            bytes += text.len();
            lines += text.lines().filter(|l| !l.trim().is_empty()).count();
            for m in check_agrees_detailed(&text) {
                let slot = by_field.entry(m.field).or_insert((0, String::new()));
                slot.0 += 1;
                if slot.1.is_empty() {
                    slot.1 = format!("{}\n      in {}", m.detail, path.display());
                }
            }
        }

        eprintln!(
            "\ncompared {} files, {lines} lines, {:.1} MB ({skipped} unreadable)",
            files.len(),
            bytes as f64 / 1e6,
        );
        if by_field.is_empty() {
            eprintln!("skeleton == parser on every line");
            return;
        }
        eprintln!("\ndisagreements, grouped by field:");
        for (field, (count, example)) in &by_field {
            eprintln!("  {field}: {count}\n      e.g. {example}");
        }
        let total: usize = by_field.values().map(|(n, _)| n).sum();
        panic!("{total} disagreement(s) across {} field(s)", by_field.len());
    }

    /// Envelope keys are NOT emitted first, and nested objects reuse their
    /// names. This is the shape that broke the first implementation: an
    /// `attachment` entry whose payload carries its own `"type"` ahead of the
    /// envelope's. Taking the first match in the line read `"file"` as the
    /// entry kind.
    #[test]
    fn nested_keys_do_not_shadow_the_envelope() {
        let line = "{\"parentUuid\":\"df4e8281-7786-43cd-a51f-994daff568da\",\"isSidechain\":true,             \"agentId\":\"a1974221057027b73\",             \"attachment\":{\"type\":\"file\",\"timestamp\":\"1999-01-01T00:00:00.000Z\",             \"agentId\":\"nested-should-be-ignored\"},             \"type\":\"attachment\",\"uuid\":\"x1\",             \"timestamp\":\"2026-08-26T09:00:00.000Z\"}";

        let rec = index_line(line.as_bytes(), 0).expect("indexed");

        assert_eq!(
            rec.kind,
            Kind::Attachment,
            "the envelope's type, not the payload's"
        );
        assert_eq!(
            rec.ts()
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "2026-08-26T09:00:00.000Z",
            "the envelope's timestamp, not the payload's"
        );
        assert_eq!(rec.agent_key, fnv1a(b"a1974221057027b73"));
        assert_ne!(rec.agent_key, fnv1a(b"nested-should-be-ignored"));
        assert!(rec.flags & flags::SIDECHAIN != 0);

        // And it agrees with the real parser, which is the actual contract.
        assert_agrees(&format!("{line}\n"));
    }

    /// A nested `origin` must not be mistaken for the envelope's, and a `kind`
    /// key elsewhere in the line must not be read as prompt provenance.
    #[test]
    fn prompt_provenance_is_scoped_to_the_envelope_origin() {
        // No top-level `origin` at all, but a nested one that says "human".
        let decoy = "{\"type\":\"user\",\"uuid\":\"u1\",             \"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",             \"tool_use_id\":\"t1\",\"content\":\"origin kind human\"}]}}";
        let rec = index_line(decoy.as_bytes(), 0).unwrap();
        assert_eq!(
            rec.flags & flags::HUMAN_ORIGIN,
            0,
            "no envelope origin to read"
        );
        assert!(
            rec.flags & flags::AMBIGUOUS_PROMPT != 0,
            "a user line without origin is ambiguous, not assumed"
        );

        let real = "{\"type\":\"user\",\"uuid\":\"u1\",\"origin\":{\"kind\":\"human\"},             \"message\":{\"role\":\"user\",\"content\":\"hi\"}}";
        let rec = index_line(real.as_bytes(), 0).unwrap();
        assert!(rec.flags & flags::HUMAN_ORIGIN != 0);
        assert_eq!(rec.flags & flags::AMBIGUOUS_PROMPT, 0);
    }

    /// Escaped quotes inside string values must not end the value early — a
    /// mis-terminated string would desynchronize the whole key walk.
    #[test]
    fn escaped_quotes_do_not_desync_the_scan() {
        let line = "{\"type\":\"user\",\"uuid\":\"u1\",             \"message\":{\"role\":\"user\",\"content\":\"she said \\\"type\\\": \\\"assistant\\\" out loud\"},             \"timestamp\":\"2026-08-26T09:00:00.000Z\"}";
        let rec = index_line(line.as_bytes(), 0).unwrap();
        assert_eq!(rec.kind, Kind::User, "the quoted text is not a type tag");
        assert!(
            rec.ts().is_some(),
            "the key walk reached the trailing timestamp"
        );
        assert_agrees(&format!("{line}\n"));
    }

    /// Activity counts are scoped to the entry kind and read from real content
    /// blocks — never byte-counted across the line. This is the shape the
    /// real-corpus test caught: a `user` line whose quoted tool output mentions
    /// `"name":"Agent"` repeatedly counted six spawns against the parser's zero.
    #[test]
    fn quoted_text_is_not_counted_as_activity() {
        let echoed = "{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,             \"timestamp\":\"2026-08-26T09:00:00.000Z\",             \"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",             \"tool_use_id\":\"t1\",\"content\":\"available tools: \
             {name: Agent}, {name: Task}, {name: Workflow}, type: tool_use, is_error: true\"}]}}";
        let rec = index_line(echoed.as_bytes(), 0).expect("indexed");
        assert_eq!(rec.spawns, 0, "a user line spawns nothing");
        assert_eq!(
            rec.tool_uses, 0,
            "tool_use is counted on assistant lines only"
        );
        assert_eq!(rec.failures, 0, "the result is not flagged is_error");
        assert_agrees(&format!("{echoed}\n"));

        // The assistant side: real blocks are counted, and a spawn name that
        // appears only inside a tool INPUT is not a spawn.
        let assistant = "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":\"u1\",             \"timestamp\":\"2026-08-26T09:00:05.000Z\",             \"message\":{\"role\":\"assistant\",\"content\":[             {\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Read\",              \"input\":{\"prompt\":\"describe the Agent and Workflow tools\"}},             {\"type\":\"tool_use\",\"id\":\"t2\",\"name\":\"Agent\",\"input\":{}}]}}";
        let rec = index_line(assistant.as_bytes(), 0).expect("indexed");
        assert_eq!(rec.tool_uses, 2);
        assert_eq!(rec.spawns, 1, "only the block whose name is a spawn tool");
        assert_agrees(&format!("{assistant}\n"));

        // A genuine failure still counts, on the user side where it lives.
        let failed = "{\"type\":\"user\",\"uuid\":\"u2\",\"parentUuid\":\"a1\",             \"timestamp\":\"2026-08-26T09:00:09.000Z\",             \"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",             \"tool_use_id\":\"t1\",\"is_error\":true,\"content\":\"boom\"}]}}";
        let rec = index_line(failed.as_bytes(), 0).expect("indexed");
        assert_eq!(rec.failures, 1);
        assert_agrees(&format!("{failed}\n"));
    }

    /// String-form `message.content` (a plain prompt) carries no blocks.
    #[test]
    fn string_content_yields_no_counts() {
        let line = "{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,             \"timestamp\":\"2026-08-26T09:00:00.000Z\",\"origin\":{\"kind\":\"human\"},             \"message\":{\"role\":\"user\",\"content\":\"use the Agent tool please\"}}";
        let rec = index_line(line.as_bytes(), 0).expect("indexed");
        assert_eq!((rec.tool_uses, rec.spawns, rec.failures), (0, 0, 0));
        assert!(rec.flags & flags::HUMAN_ORIGIN != 0);
        assert_agrees(&format!("{line}\n"));
    }

    #[test]
    fn timestamps_round_trip() {
        // Canonical millisecond form, the shape transcripts actually use.
        let micros = parse_ts(b"2026-06-21T10:00:09.250Z").unwrap();
        assert_eq!(
            chrono::DateTime::from_timestamp_micros(micros).unwrap(),
            "2026-06-21T10:00:09.250Z"
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap()
        );
        // Second precision, and a non-canonical offset via the slow path.
        assert!(parse_ts(b"2026-06-21T10:00:09Z").is_some());
        assert_eq!(
            parse_ts(b"2026-06-21T12:00:09+02:00"),
            parse_ts(b"2026-06-21T10:00:09Z")
        );
        assert!(parse_ts(b"not a timestamp").is_none());
    }

    #[test]
    fn extend_resumes_at_the_append_point() {
        let a = "{\"type\":\"user\",\"uuid\":\"u1\",\"timestamp\":\"2026-06-21T10:00:00.000Z\"}\n";
        let b =
            "{\"type\":\"assistant\",\"uuid\":\"a1\",\"timestamp\":\"2026-06-21T10:00:05.000Z\"}\n";

        let mut idx = Index::default();
        idx.extend(a.as_bytes());
        assert_eq!(idx.recs.len(), 1);
        assert_eq!(idx.scanned_len, a.len() as u64);

        // Appending re-scans only the new bytes and keeps the old records.
        let both = format!("{a}{b}");
        assert!(idx.continues(both.as_bytes()));
        idx.extend(both.as_bytes());
        assert_eq!(idx.recs.len(), 2);
        assert_eq!(idx.recs[1].offset, a.len() as u64);
        assert_eq!(idx.recs[1].kind, Kind::Assistant);
    }

    #[test]
    fn a_partial_trailing_line_is_left_for_the_next_scan() {
        let complete = "{\"type\":\"user\",\"uuid\":\"u1\"}\n";
        let partial = "{\"type\":\"assis";
        let mut idx = Index::default();
        idx.extend(format!("{complete}{partial}").as_bytes());
        // The half-written line is not indexed, and the next scan starts at it.
        assert_eq!(idx.recs.len(), 1);
        assert_eq!(idx.scanned_len, complete.len() as u64);
    }

    #[test]
    fn truncation_is_detected() {
        let text = "{\"type\":\"user\",\"uuid\":\"u1\"}\n{\"type\":\"user\",\"uuid\":\"u2\"}\n";
        let mut idx = Index::default();
        idx.extend(text.as_bytes());
        assert!(idx.continues(text.as_bytes()));
        // A file that got shorter was rotated, not appended to.
        assert!(!idx.continues(&text.as_bytes()[..10]));
    }

    #[test]
    fn cache_round_trips() {
        let text = include_str!("../assets/demo.jsonl");
        let mut idx = Index::default();
        idx.extend(text.as_bytes());
        let decoded = Index::decode(&idx.encode()).expect("decodes");
        assert_eq!(decoded.recs, idx.recs);
        assert_eq!(decoded.scanned_len, idx.scanned_len);
        assert_eq!(decoded.head_hash, idx.head_hash);
        assert_eq!(decoded.head_len, idx.head_len);
        // A foreign file is rejected, not misread.
        assert!(Index::decode(b"not an index").is_none());
    }
}
