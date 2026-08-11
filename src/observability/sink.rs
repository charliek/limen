//! The mismatch diff sink: durable, redacted JSONL records of every comparison
//! mismatch (spec §10.4).
//!
//! [`MetricsObserver`][super::MetricsObserver] counts mismatches and logs one
//! line per mismatch, which is enough to *notice* a divergence but not enough to
//! investigate one after the log buffer has rolled. [`SinkObserver`] adds the
//! durable half: when `diff_sink` is configured, every mismatch is appended as
//! one JSON object to `<dir>/mismatches-<YYYY-MM-DD>.jsonl` (UTC date), and
//! [`read_report`] aggregates those files for `limen report`.
//!
//! Safety notes:
//!
//! - **Redaction (invariant 5).** Every value written comes from
//!   [`ComparisonResult`], which the comparison engine already redacted at
//!   render time — cookie values, sensitive headers, sensitive query params, and
//!   redacted JSON paths never reach this module in the clear. The sink adds no
//!   values of its own beyond the request's own method/path and ids.
//! - **Off the client path, and off every Tokio worker (invariant 2).** The
//!   observer runs inside the detached shadow task, already off the client's
//!   response path — but the file write is *synchronous*, so doing it inline
//!   would park a Tokio worker per stalled write (a full or unmounted volume
//!   under concurrent mismatches). Instead [`SinkObserver::comparison`] only
//!   serializes the record and hands it to a bounded channel; a single dedicated
//!   OS thread ([`run_writer`]) owns the file handle, date rotation, and the
//!   blocking IO. The channel is bounded and non-blocking to the producer: a full
//!   queue drops-and-counts (warn-once), exactly like an IO failure — diagnostics
//!   must never degrade the proxy, so the shadow task is never blocked. On
//!   shutdown the observer is simply dropped; the channel closes and the writer
//!   thread exits (best-effort flush — a diagnostic sink needn't guarantee its
//!   last line).

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};
use tracing::warn;

use crate::compare::result::{
    ComparisonResult, CookieMismatch, Difference, HeaderMismatch, LocationMismatch,
};
use crate::observability::metrics::{ShadowFailure, ShadowMeta, ShadowObserver, SkipReason};
use crate::observability::prometheus::{self, SinkDropReason};

/// The file-name prefix of a daily sink file (`mismatches-2026-07-28.jsonl`).
const FILE_PREFIX: &str = "mismatches-";
/// The file-name suffix of a daily sink file.
const FILE_SUFFIX: &str = ".jsonl";
/// How many recent examples `limen report` shows per route.
pub const REPORT_EXAMPLES_PER_ROUTE: usize = 3;

/// Depth of the bounded channel feeding the writer thread. Deep enough to absorb
/// a burst of concurrent mismatches while the writer flushes one line at a time,
/// shallow enough to bound memory: over this many queued records, new mismatches
/// are dropped-and-counted rather than blocking the shadow task (invariant 2).
const WRITER_QUEUE_DEPTH: usize = 1024;

/// One mismatch record, as written to the sink.
///
/// Borrowed from the [`ShadowMeta`] and [`ComparisonResult`] it describes so a
/// write allocates nothing but the serialized line. The reader side is the
/// deliberately looser [`ReportRecord`].
#[derive(Debug, Serialize)]
struct MismatchRecord<'a> {
    timestamp: &'a str,
    route_id: &'a str,
    request_id: &'a str,
    method: &'a str,
    path: &'a str,
    legacy_status: u16,
    new_status: u16,
    status_match: bool,
    body_match: bool,
    mismatch_kinds: Vec<String>,
    differences: &'a [Difference],
    header_mismatches: &'a [HeaderMismatch],
    cookie_mismatches: &'a [CookieMismatch],
    location_mismatches: &'a [LocationMismatch],
    diff_truncated: bool,
}

/// A serialized, newline-terminated record plus the UTC date that selects its
/// daily file. The record is serialized on the shadow task (borrowing from the
/// [`ComparisonResult`]) so the channel only ever carries owned bytes.
struct QueuedRecord {
    date: Date,
    line: String,
}

/// A message to the writer thread.
enum WriterMsg {
    /// Append this record to its daily file.
    Record(QueuedRecord),
    /// Test-only rendezvous: the writer acks once it has drained every record
    /// enqueued before this message (the channel is FIFO and single-consumer),
    /// so a test can read the file without racing the writer.
    #[cfg(test)]
    Flush(SyncSender<()>),
}

/// Drop counters shared between the producing shadow tasks and the writer thread.
///
/// Diagnostics-only: every increment marks a mismatch record that was *dropped*
/// rather than persisted, so the proxy is never blocked or brought down by the
/// sink (invariant 2). Exposed to the writer and to tests, never to the client
/// path's control flow.
#[derive(Default)]
struct SinkStats {
    /// Records dropped because the daily file could not be opened or written
    /// (full/unmounted volume, a path whose parent is a file, …).
    io_failures: AtomicU64,
    /// Records dropped because the writer queue was full — the writer could not
    /// keep up (a stalled volume) and the shadow task refused to block on it.
    queue_overflows: AtomicU64,
}

/// A [`ShadowObserver`] that appends every mismatch to a daily JSONL file.
///
/// Matching comparisons and the non-comparison callbacks are no-ops: the sink is
/// a mismatch archive, not a request log. The observer never touches the
/// filesystem itself — it serializes each mismatch and hands it to the dedicated
/// [`run_writer`] thread over a bounded channel.
pub struct SinkObserver {
    /// Kept only for log context (the writer thread owns the actual directory).
    dir: PathBuf,
    /// Bounded, non-blocking producer end. A full channel drops-and-counts.
    tx: SyncSender<WriterMsg>,
    /// Shared drop counters (also read by the writer thread and by tests).
    stats: Arc<SinkStats>,
    /// Warn-once latch for a full writer queue: the first overflow warns, the
    /// rest are counted in `stats.queue_overflows`, not re-logged.
    overflow_warned: AtomicBool,
}

impl SinkObserver {
    /// Create a sink writing under `dir` and spawn its dedicated writer thread.
    /// The directory is created lazily on the first mismatch, so configuring a
    /// sink never touches the filesystem at startup (and a proxy that never
    /// mismatches leaves no trace).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let stats = Arc::new(SinkStats::default());
        let (tx, rx) = sync_channel(WRITER_QUEUE_DEPTH);
        // A dedicated OS thread — not a Tokio task — owns the file handle and all
        // blocking IO, so a stalled volume parks only this thread, never a Tokio
        // worker (invariant 2). It exits when the last `tx` is dropped (the
        // channel closes), i.e. at shutdown.
        let writer_dir = dir.clone();
        let writer_stats = Arc::clone(&stats);
        thread::Builder::new()
            .name("limen-diff-sink".to_string())
            .spawn(move || run_writer(&writer_dir, &rx, &writer_stats))
            .expect("spawn diff-sink writer thread");
        Self {
            dir,
            tx,
            stats,
            overflow_warned: AtomicBool::new(false),
        }
    }

    /// Record that a mismatch was dropped because the writer channel refused it.
    /// Warn once, then only count — a stalled sink must not drown the logs.
    ///
    /// Both refusals share the `queue_overflows` stat and the warn latch: from
    /// the shadow task's side a refused record is a refused record, and the
    /// existing counter's meaning ("the producer could not hand this off") is
    /// unchanged. The metric's `reason` label is what tells a full queue from a
    /// dead writer. `event` comes from the call site so each refusal keeps its
    /// own log event name without this function claiming to handle the IO
    /// failures the writer thread reports itself (`record_failure`).
    fn note_refused(&self, reason: SinkDropReason, event: &'static str) {
        self.stats.queue_overflows.fetch_add(1, Ordering::Relaxed);
        prometheus::diff_sink_dropped(reason);
        if !self.overflow_warned.swap(true, Ordering::Relaxed) {
            warn!(
                event,
                reason = reason.as_str(),
                dir = %self.dir.display(),
                "mismatch record dropped; further drops are counted, not logged"
            );
        }
    }
}

/// The writer thread's loop: drain the channel, appending each record to its
/// daily file, until every producer has hung up. Owns all blocking IO and the
/// rotation + warn-once failure latch. Never panics: an IO failure is warned
/// about (once per run of consecutive failures) and the record dropped.
fn run_writer(dir: &Path, rx: &Receiver<WriterMsg>, stats: &SinkStats) {
    let mut state = WriterState::default();
    while let Ok(msg) = rx.recv() {
        match msg {
            WriterMsg::Record(record) => write_record(dir, &mut state, stats, &record),
            #[cfg(test)]
            WriterMsg::Flush(ack) => {
                // Every earlier `Record` has already been handled (FIFO), so the
                // file reflects them; the receiver can now read it.
                let _ = ack.send(());
            }
        }
    }
}

/// The currently open daily file plus the warn-once latch for IO failures.
/// Lives entirely inside the single writer thread, so it needs no locking.
#[derive(Default)]
struct WriterState {
    /// The open file and the UTC date it holds, if any.
    open: Option<(Date, File)>,
    /// Whether the last write failed (so repeated failures warn once, not once
    /// per mismatch).
    failing: bool,
    /// Failures suppressed by the latch since the last successful write.
    suppressed: u64,
}

/// Append one serialized, newline-terminated record, rotating the open file when
/// the UTC date changes.
fn write_record(dir: &Path, state: &mut WriterState, stats: &SinkStats, record: &QueuedRecord) {
    let date = record.date;
    if state.open.as_ref().is_none_or(|(open, _)| *open != date) {
        match open_file(dir, date) {
            Ok(file) => state.open = Some((date, file)),
            Err(e) => {
                record_failure(dir, state, stats, date, &e);
                return;
            }
        }
    }
    let Some((_, file)) = state.open.as_mut() else {
        return;
    };
    // A single `write_all` of the newline-terminated record on an O_APPEND file
    // keeps other limen processes sharing the directory from interleaving partial
    // records. (Within one process only this thread writes.)
    match file.write_all(record.line.as_bytes()) {
        Ok(()) => {
            prometheus::diff_sink_written();
            if state.failing {
                warn!(
                    suppressed_failures = state.suppressed,
                    dir = %dir.display(),
                    "limen.diff_sink_recovered"
                );
                state.failing = false;
                state.suppressed = 0;
            }
        }
        // Drop the handle so the next record reopens rather than retrying a file
        // descriptor that may be gone (rotated away by an external log rotator,
        // unmounted volume, …).
        Err(e) => {
            state.open = None;
            record_failure(dir, state, stats, date, &e);
        }
    }
}

/// Open (creating the directory and file as needed) the daily file.
fn open_file(dir: &Path, date: Date) -> std::io::Result<File> {
    std::fs::create_dir_all(dir)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(file_name(date)))
}

/// Count a dropped record and warn about the IO failure the first time, then
/// only count it until a write succeeds again — a sink on a full disk must not
/// drown the logs.
fn record_failure(
    dir: &Path,
    state: &mut WriterState,
    stats: &SinkStats,
    date: Date,
    error: &std::io::Error,
) {
    stats.io_failures.fetch_add(1, Ordering::Relaxed);
    prometheus::diff_sink_dropped(SinkDropReason::IoError);
    if state.failing {
        state.suppressed = state.suppressed.saturating_add(1);
        return;
    }
    state.failing = true;
    state.suppressed = 0;
    warn!(
        event = "limen.diff_sink_write_failed",
        path = %dir.join(file_name(date)).display(),
        error = %error,
        "mismatch record dropped; further failures are counted, not logged"
    );
}

/// The daily file name for a UTC date.
fn file_name(date: Date) -> String {
    format!(
        "{FILE_PREFIX}{:04}-{:02}-{:02}{FILE_SUFFIX}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

impl ShadowObserver for SinkObserver {
    fn shadow_dispatched(&self, _meta: &ShadowMeta) {}

    fn comparison(&self, meta: &ShadowMeta, result: &ComparisonResult) {
        if result.is_match() {
            return;
        }
        let now = OffsetDateTime::now_utc();
        // RFC 3339 formatting of a real `OffsetDateTime` cannot fail; fall back
        // to an empty timestamp rather than dropping the record if it somehow
        // does (the reader treats that line as malformed, which is honest).
        let timestamp = now.format(&Rfc3339).unwrap_or_default();
        let record = MismatchRecord {
            timestamp: &timestamp,
            route_id: &meta.route_id,
            request_id: &meta.request_id,
            method: meta.method.as_str(),
            path: &meta.path,
            legacy_status: result.legacy_status,
            new_status: result.new_status,
            status_match: result.status_match,
            body_match: result.body_match,
            mismatch_kinds: result.mismatch_kinds(),
            differences: &result.differences,
            header_mismatches: &result.header_mismatches,
            cookie_mismatches: &result.cookie_mismatches,
            location_mismatches: &result.location_mismatches,
            diff_truncated: result.diff_truncated,
        };
        let mut line = match serde_json::to_string(&record) {
            Ok(line) => line,
            // Unreachable: every field is a plain serializable type. Note this
            // returns *before* the record is offered to the queue, so it is
            // neither enqueued nor dropped — the record never entered the
            // pipeline the drain equation describes.
            Err(e) => {
                warn!(
                    event = "limen.diff_sink_serialize_failed",
                    route_id = %meta.route_id,
                    error = %e,
                );
                return;
            }
        };
        line.push('\n');
        // Counted at the offer, before `try_send` can refuse it, so every record
        // that entered the pipeline is accounted for exactly once as written or
        // dropped (`enqueued == written + dropped`).
        prometheus::diff_sink_enqueued();
        // Hand the record to the writer thread without ever blocking the shadow
        // task: a full queue (a stalled volume the writer can't drain) drops-and-
        // counts, exactly like an IO failure. `Disconnected` means the writer
        // thread is gone (only possible if it panicked, which it is written not
        // to) — count that dropped record too, under its own reason, rather than
        // resurrecting the thread.
        match self.tx.try_send(WriterMsg::Record(QueuedRecord {
            date: now.date(),
            line,
        })) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.note_refused(SinkDropReason::QueueFull, "limen.diff_sink_queue_full")
            }
            Err(TrySendError::Disconnected(_)) => {
                self.note_refused(SinkDropReason::WriterGone, "limen.diff_sink_writer_gone")
            }
        }
    }

    fn shadow_skipped(&self, _meta: &ShadowMeta, _reason: SkipReason) {}
    fn shadow_failed(&self, _meta: &ShadowMeta, _failure: ShadowFailure) {}
    fn comparison_skipped(&self, _meta: &ShadowMeta, _reason: SkipReason) {}
}

#[cfg(test)]
impl SinkObserver {
    /// Block until the writer thread has processed every record enqueued so far.
    /// Test-only: lets a test read the sink file without racing the writer.
    fn flush(&self) {
        let (ack_tx, ack_rx) = sync_channel(0);
        if self.tx.send(WriterMsg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }
}

// ---------------------------------------------------------------------------
// Reading side: `limen report`
// ---------------------------------------------------------------------------

/// One record as read back from a sink file.
///
/// Deliberately looser than the writer's [`MismatchRecord`]: unknown fields are
/// ignored and everything but the fields the report keys on defaults, so a sink
/// written by a newer Limen still reports cleanly against an older binary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportRecord {
    /// RFC 3339 timestamp (UTC) the mismatch was recorded at.
    pub timestamp: String,
    /// The matched route id.
    pub route_id: String,
    /// The originating request's `x-request-id`.
    #[serde(default)]
    pub request_id: String,
    /// The request method.
    #[serde(default)]
    pub method: String,
    /// The concrete request path.
    #[serde(default)]
    pub path: String,
    /// The neutral mismatch-kind vocabulary
    /// ([`ComparisonResult::mismatch_kinds`]).
    #[serde(default)]
    pub mismatch_kinds: Vec<String>,
}

/// Which records a report covers.
#[derive(Debug, Clone, Default)]
pub struct ReportFilter {
    /// Only this route id, if set.
    pub route: Option<String>,
    /// Only records at or after this instant, if set.
    pub since: Option<OffsetDateTime>,
}

impl ReportFilter {
    /// Whether a record, at its parsed timestamp, belongs in the report. An
    /// unset filter accepts everything; `since` is inclusive of its instant.
    fn accepts(&self, at: OffsetDateTime, record: &ReportRecord) -> bool {
        self.route.as_ref().is_none_or(|r| *r == record.route_id)
            && self.since.is_none_or(|since| at >= since)
    }
}

/// Per-route aggregation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteReport {
    /// The route id.
    pub route_id: String,
    /// How many mismatches the route has (after filtering).
    pub count: usize,
    /// Mismatch counts by kind. One record contributes to every kind it
    /// carries, so these sum to at least `count`.
    pub kinds: BTreeMap<String, usize>,
    /// The most recent records, newest first.
    pub examples: Vec<ReportRecord>,
}

/// The aggregated report over a sink directory.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// Per-route aggregates, most mismatches first (ties broken by route id).
    pub routes: Vec<RouteReport>,
    /// Total matching records across all routes.
    pub total: usize,
    /// Lines that could not be parsed as a mismatch record. Reported rather
    /// than fatal: a truncated final line (the proxy was killed mid-write) must
    /// not cost you the rest of the report.
    pub malformed_lines: usize,
    /// How many sink files were read.
    pub files_read: usize,
}

/// Read every `mismatches-*.jsonl` file in `dir`, apply `filter`, and aggregate.
///
/// Exposed as a library function (not just CLI code) so the report logic is
/// testable without spawning the binary — the thin-binary convention.
pub fn read_report(
    dir: &Path,
    filter: &ReportFilter,
    examples_per_route: usize,
) -> std::io::Result<Report> {
    let files = sink_files(dir)?;
    let mut malformed_lines = 0usize;
    // Grouping by id needs a map; a BTreeMap also makes the final sort's ties
    // deterministic.
    let mut by_route: BTreeMap<String, Vec<(OffsetDateTime, ReportRecord)>> = BTreeMap::new();

    for file in &files {
        // Read bytes, not a `String`: one torn write splitting a multi-byte
        // character must cost that *line*, not the whole file (and with it the
        // rest of the report). Invalid UTF-8 is localized to its own line below.
        let contents = std::fs::read(file)?;
        for raw in contents.split(|b| *b == b'\n') {
            // Tolerate CRLF, in case a file made a round trip through Windows.
            let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
            // Equivalent to lossy conversion + "did it gain a replacement
            // character?", minus the false positive on a record that legitimately
            // contains U+FFFD.
            let Ok(line) = std::str::from_utf8(raw) else {
                malformed_lines += 1;
                continue;
            };
            if line.trim().is_empty() {
                continue;
            }
            let Some((at, record)) = parse_line(line) else {
                malformed_lines += 1;
                continue;
            };
            if !filter.accepts(at, &record) {
                continue;
            }
            by_route
                .entry(record.route_id.clone())
                .or_default()
                .push((at, record));
        }
    }

    let total = by_route.values().map(Vec::len).sum();
    let mut routes: Vec<RouteReport> = by_route
        .into_iter()
        .map(|(route_id, records)| aggregate(route_id, records, examples_per_route))
        .collect();
    routes.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.route_id.cmp(&b.route_id))
    });

    Ok(Report {
        routes,
        total,
        malformed_lines,
        files_read: files.len(),
    })
}

/// Every sink file in `dir`, sorted by name — which, given the date-based
/// naming, is chronological.
fn sink_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let named_like_a_sink_file = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_sink_file_name);
        if named_like_a_sink_file && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Whether a file name is exactly what [`file_name`] writes:
/// `mismatches-YYYY-MM-DD.jsonl`, over a real calendar date.
///
/// Deliberately strict rather than a `mismatches-*.jsonl` glob. A sink directory
/// tends to accumulate operator copies — `mismatches-backup.jsonl`,
/// `mismatches-2026-07-28-copy.jsonl` — and counting those would double-report
/// the very mismatches an operator is trying to size up.
fn is_sink_file_name(name: &str) -> bool {
    sink_file_date(name).is_some()
}

/// The UTC date a sink file name encodes, or `None` if the name is not one
/// [`file_name`] could have produced.
///
/// The date is *parsed*, not merely shaped: the writer derives the name from a
/// real `Date`, so `mismatches-2026-99-99.jsonl` is a name limen cannot have
/// written. A digits-only check would count it as a sink file, and an empty one
/// dropped into the directory would then turn "no evidence" into "a file was
/// read and held nothing" — absence promoted to a clean bill of health, which
/// is the one direction this reader must never fail in.
fn sink_file_date(name: &str) -> Option<Date> {
    let rest = name.strip_prefix(FILE_PREFIX)?;
    let date = rest.strip_suffix(FILE_SUFFIX)?;
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if ![0, 1, 2, 3, 5, 6, 8, 9]
        .iter()
        .all(|i| bytes[*i].is_ascii_digit())
    {
        return None;
    }
    // Zero-padded fixed widths, digit-checked above, so these parses cannot
    // overflow or sign-flip; the calendar check is what does the real work
    // (month 13, day 31 in November, February 29 in a common year).
    let year: i32 = date[0..4].parse().ok()?;
    let month: u8 = date[5..7].parse().ok()?;
    let day: u8 = date[8..10].parse().ok()?;
    Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()
}

/// Roll one route's records up into its [`RouteReport`], keeping the newest
/// `examples_per_route` as evidence.
fn aggregate(
    route_id: String,
    mut records: Vec<(OffsetDateTime, ReportRecord)>,
    examples_per_route: usize,
) -> RouteReport {
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for (_, record) in &records {
        for kind in &record.mismatch_kinds {
            *kinds.entry(kind.clone()).or_insert(0) += 1;
        }
    }
    // Newest first, so the examples are the latest evidence.
    records.sort_by_key(|(at, _)| Reverse(*at));
    RouteReport {
        route_id,
        count: records.len(),
        kinds,
        examples: records
            .into_iter()
            .take(examples_per_route)
            .map(|(_, record)| record)
            .collect(),
    }
}

/// Parse one JSONL line into a record plus its parsed timestamp. `None` means
/// malformed: not JSON, missing a keying field, or an untimestamped record we
/// could neither order nor filter honestly.
fn parse_line(line: &str) -> Option<(OffsetDateTime, ReportRecord)> {
    let record: ReportRecord = serde_json::from_str(line).ok()?;
    let at = OffsetDateTime::parse(&record.timestamp, &Rfc3339).ok()?;
    Some((at, record))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::diff::DiffLimits;
    use crate::compare::result::{ChangeKind, ComparisonResult, Difference};
    use crate::compare::{compare, Captured};
    use crate::contract::model::{
        ComparisonRules, CookieValueMode, OriginMode, ResolvedLocationRules, ResolvedSetCookieRules,
    };
    use axum::http::{HeaderMap, Method};
    use bytes::Bytes;
    use serde_json::Value;

    fn meta(request_id: &str) -> ShadowMeta {
        ShadowMeta {
            route_id: "get-device".to_string(),
            request_id: request_id.to_string(),
            method: Method::GET,
            path: "/devices/1".to_string(),
        }
    }

    fn matching() -> ComparisonResult {
        ComparisonResult {
            status_match: true,
            legacy_status: 200,
            new_status: 200,
            body_match: true,
            diff_kind: None,
            differences: vec![],
            diff_truncated: false,
            header_mismatches: vec![],
            cookie_mismatches: vec![],
            location_mismatches: vec![],
        }
    }

    fn mismatching() -> ComparisonResult {
        ComparisonResult {
            body_match: false,
            new_status: 500,
            status_match: false,
            differences: vec![Difference {
                path: "$.name".to_string(),
                kind: ChangeKind::Changed,
                legacy: Some(Value::String("A".into())),
                new: Some(Value::String("B".into())),
            }],
            ..matching()
        }
    }

    /// The single file the sink wrote, as (name, contents).
    fn sole_file(dir: &Path) -> (String, String) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one sink file: {entries:?}"
        );
        let name = entries[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        (name, std::fs::read_to_string(&entries[0]).unwrap())
    }

    #[test]
    fn mismatch_writes_one_dated_jsonl_line() {
        let dir = tempfile::tempdir().unwrap();
        let sink = SinkObserver::new(dir.path().join("diffs"));
        sink.comparison(&meta("req-1"), &mismatching());
        sink.flush();

        let sink_dir = dir.path().join("diffs");
        let (name, contents) = sole_file(&sink_dir);
        let today = OffsetDateTime::now_utc().date();
        assert_eq!(name, file_name(today));
        assert!(name.starts_with("mismatches-") && name.ends_with(".jsonl"));

        assert_eq!(contents.lines().count(), 1);
        let record: Value = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(record["route_id"], "get-device");
        assert_eq!(record["request_id"], "req-1");
        assert_eq!(record["method"], "GET");
        assert_eq!(record["path"], "/devices/1");
        assert_eq!(record["legacy_status"], 200);
        assert_eq!(record["new_status"], 500);
        assert_eq!(record["status_match"], false);
        assert_eq!(record["body_match"], false);
        assert_eq!(record["diff_truncated"], false);
        assert_eq!(
            record["mismatch_kinds"],
            serde_json::json!(["body", "status"])
        );
        assert_eq!(record["differences"][0]["path"], "$.name");
        assert!(record["header_mismatches"].as_array().unwrap().is_empty());
        assert!(record["cookie_mismatches"].as_array().unwrap().is_empty());
        assert!(record["location_mismatches"].as_array().unwrap().is_empty());
        // The timestamp round-trips as RFC 3339.
        OffsetDateTime::parse(record["timestamp"].as_str().unwrap(), &Rfc3339).unwrap();
    }

    #[test]
    fn matching_comparisons_and_other_callbacks_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sink_dir = dir.path().join("diffs");
        let sink = SinkObserver::new(&sink_dir);

        sink.comparison(&meta("req-1"), &matching());
        sink.shadow_dispatched(&meta("req-1"));
        sink.shadow_skipped(&meta("req-1"), SkipReason::ConcurrencyLimit);
        sink.shadow_failed(&meta("req-1"), ShadowFailure::Timeout);
        sink.comparison_skipped(&meta("req-1"), SkipReason::ResponseTooLarge);
        sink.flush();

        // Not even the directory is created: a clean run leaves no trace.
        assert!(!sink_dir.exists());
    }

    #[test]
    fn repeated_mismatches_append_to_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let sink = SinkObserver::new(dir.path());
        for i in 0..3 {
            sink.comparison(&meta(&format!("req-{i}")), &mismatching());
        }
        sink.flush();
        let (_, contents) = sole_file(dir.path());
        assert_eq!(contents.lines().count(), 3);
        assert!(contents.contains("req-0") && contents.contains("req-2"));
    }

    #[test]
    fn an_unwritable_directory_never_panics() {
        // A path whose parent is a *file* can never be created; the sink must
        // swallow the failure (invariant: the shadow path never dies).
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let sink = SinkObserver::new(blocker.join("sink"));
        sink.comparison(&meta("req-1"), &mismatching());
        sink.comparison(&meta("req-2"), &mismatching());
        sink.flush();
        // Both records were dropped by the writer thread (the first warns, the
        // second is counted, not re-logged) — and nothing panicked.
        assert_eq!(sink.stats.io_failures.load(Ordering::Relaxed), 2);
        assert_eq!(sink.stats.queue_overflows.load(Ordering::Relaxed), 0);
    }

    /// Safety invariant 5, end to end through the sink: a cookie/Location
    /// mismatch is persisted with names and attributes but never a raw cookie
    /// value or a sensitive query value (the L3 fixture shape).
    #[test]
    fn sink_lines_never_contain_a_raw_cookie_or_sensitive_query_value() {
        let rules = ComparisonRules {
            set_cookie: Some(ResolvedSetCookieRules {
                compare: true,
                ignore_cookies: Vec::new(),
                ignore_attributes: Vec::new(),
                compare_values: CookieValueMode::Exact,
            }),
            location: Some(ResolvedLocationRules {
                compare: true,
                ignore_query_params: Vec::new(),
                origin: OriginMode::Exact,
            }),
            ..Default::default()
        };

        let captured = |cookie: &str, orphan: &str, location: &str| {
            let mut headers = HeaderMap::new();
            headers.append("set-cookie", cookie.parse().unwrap());
            headers.append("set-cookie", orphan.parse().unwrap());
            headers.insert("location", location.parse().unwrap());
            Captured {
                status: 302,
                headers,
                body: Bytes::from_static(b"{}"),
                request_url: None,
            }
        };
        let legacy = captured(
            "session=legacy-secret-value; Path=/api; SameSite=Lax",
            "not-a-cookie-legacy-secret",
            "https://app.example/cb?code=legacy-auth-code&next=/home",
        );
        let new = captured(
            "session=new-secret-value; Path=/api; SameSite=None",
            "still-not-a-cookie-new-secret",
            "https://app.example/cb?code=new-auth-code&next=/home",
        );
        let result = compare(&rules, &DiffLimits::default(), &legacy, &new);
        assert!(!result.is_match());

        let dir = tempfile::tempdir().unwrap();
        let sink = SinkObserver::new(dir.path());
        sink.comparison(&meta("req-1"), &result);
        sink.flush();
        let (_, contents) = sole_file(dir.path());

        for secret in [
            "legacy-secret-value",
            "new-secret-value",
            "not-a-cookie-legacy-secret",
            "still-not-a-cookie-new-secret",
            "legacy-auth-code",
            "new-auth-code",
        ] {
            assert!(
                !contents.contains(secret),
                "sink leaked {secret}: {contents}"
            );
        }
        // …while still saying which cookie, attribute, and query param differed.
        assert!(contents.contains("session"));
        assert!(contents.contains("SameSite"));
        assert!(contents.contains("\"param\":\"code\""));
        assert!(contents.contains("set_cookie.value"));
        assert!(contents.contains("location.query"));
    }

    #[test]
    fn report_reads_back_what_the_sink_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let sink = SinkObserver::new(dir.path());
        sink.comparison(&meta("req-1"), &mismatching());
        sink.comparison(&meta("req-2"), &mismatching());
        sink.flush();

        let report = read_report(dir.path(), &ReportFilter::default(), 3).unwrap();
        assert_eq!(report.total, 2);
        assert_eq!(report.malformed_lines, 0);
        assert_eq!(report.files_read, 1);
        assert_eq!(report.routes.len(), 1);
        assert_eq!(report.routes[0].route_id, "get-device");
        assert_eq!(report.routes[0].count, 2);
        assert_eq!(report.routes[0].kinds["body"], 2);
        assert_eq!(report.routes[0].kinds["status"], 2);
        assert_eq!(report.routes[0].examples.len(), 2);
    }

    #[test]
    fn file_name_is_the_utc_date() {
        let date = Date::from_calendar_date(2026, time::Month::July, 5).unwrap();
        assert_eq!(file_name(date), "mismatches-2026-07-05.jsonl");
    }

    /// The reader accepts exactly what the writer produces — nothing an
    /// operator's `cp` might leave lying around next to it.
    #[test]
    fn only_the_writers_exact_file_name_shape_is_a_sink_file() {
        let today = file_name(OffsetDateTime::now_utc().date());
        assert!(is_sink_file_name(&today));
        assert!(is_sink_file_name("mismatches-2026-07-05.jsonl"));

        for decoy in [
            "mismatches-backup.jsonl",
            "mismatches-2026-07-05-copy.jsonl",
            "mismatches-2026-07-05.jsonl.bak",
            "mismatches-2026-07-05.jsonl.gz",
            "mismatches-2026-7-5.jsonl",
            "mismatches-2026-07-05T00.jsonl",
            "mismatches-.jsonl",
            "mismatches-yyyy-mm-dd.jsonl",
            "old-mismatches-2026-07-05.jsonl",
            "mismatches-2026-07-05.json",
            "README.md",
        ] {
            assert!(
                !is_sink_file_name(decoy),
                "{decoy} should not be a sink file"
            );
        }
    }

    /// A name the writer could never have produced is not a sink file, however
    /// well-shaped it looks. The digits are a calendar date or they are
    /// nothing: an empty `mismatches-2026-99-99.jsonl` dropped into a sink
    /// directory would otherwise read as a file that *was* read and held no
    /// mismatches — absence promoted to a clean bill of health.
    #[test]
    fn an_impossible_date_is_not_a_sink_file() {
        for impossible in [
            "mismatches-2026-99-99.jsonl",
            "mismatches-2026-13-01.jsonl",
            "mismatches-2026-00-01.jsonl",
            "mismatches-2026-01-00.jsonl",
            "mismatches-2026-01-32.jsonl",
            "mismatches-2026-11-31.jsonl",
            // 2026 is not a leap year.
            "mismatches-2026-02-29.jsonl",
        ] {
            assert!(
                !is_sink_file_name(impossible),
                "{impossible} should not be a sink file"
            );
        }
        // …and the real dates on either side of those still are.
        for real in [
            "mismatches-2026-02-28.jsonl",
            "mismatches-2024-02-29.jsonl",
            "mismatches-2026-11-30.jsonl",
            "mismatches-2026-12-31.jsonl",
            "mismatches-0001-01-01.jsonl",
        ] {
            assert!(is_sink_file_name(real), "{real} should be a sink file");
        }
    }

    /// Whatever the writer names a file, the reader accepts — checked over a
    /// year of dates rather than the one or two a hand-written list covers.
    #[test]
    fn every_name_the_writer_produces_is_read_back() {
        let mut date = Date::from_calendar_date(2024, Month::January, 1).unwrap();
        let end = Date::from_calendar_date(2025, Month::January, 1).unwrap();
        while date < end {
            let name = file_name(date);
            assert!(is_sink_file_name(&name), "{name} was written but not read");
            assert_eq!(sink_file_date(&name), Some(date));
            date = date.next_day().unwrap();
        }
    }
}
