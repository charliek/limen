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
//! - **Off the client path (invariant 2).** The observer runs inside the
//!   detached shadow task, so the blocking file write here is already off the
//!   client's response path. It still must never panic: an IO failure is warned
//!   about once and dropped.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};
use tracing::warn;

use crate::compare::result::{
    ComparisonResult, CookieMismatch, Difference, HeaderMismatch, LocationMismatch,
};
use crate::observability::metrics::{ShadowFailure, ShadowMeta, ShadowObserver, SkipReason};

/// The file-name prefix of a daily sink file (`mismatches-2026-07-28.jsonl`).
const FILE_PREFIX: &str = "mismatches-";
/// The file-name suffix of a daily sink file.
const FILE_SUFFIX: &str = ".jsonl";
/// How many recent examples `limen report` shows per route.
pub const REPORT_EXAMPLES_PER_ROUTE: usize = 3;

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

/// The currently open daily file plus the warn-once latch for IO failures.
#[derive(Default)]
struct SinkState {
    /// The open file and the UTC date it holds, if any.
    open: Option<(Date, File)>,
    /// Whether the last write failed (so repeated failures warn once, not once
    /// per mismatch).
    failing: bool,
    /// Failures suppressed by the latch since the last successful write.
    suppressed: u64,
}

/// A [`ShadowObserver`] that appends every mismatch to a daily JSONL file.
///
/// Matching comparisons and the non-comparison callbacks are no-ops: the sink is
/// a mismatch archive, not a request log.
pub struct SinkObserver {
    dir: PathBuf,
    state: Mutex<SinkState>,
}

impl SinkObserver {
    /// Create a sink writing under `dir`. The directory is created lazily on the
    /// first mismatch, so configuring a sink never touches the filesystem at
    /// startup (and a proxy that never mismatches leaves no trace).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            state: Mutex::new(SinkState::default()),
        }
    }

    /// Append one serialized, newline-terminated record, rotating the open file
    /// when the UTC date changes. Never panics: an IO failure is warned about
    /// (once per run of consecutive failures) and dropped.
    fn append(&self, date: Date, line: &str) {
        // The lock is only poisoned if a previous holder panicked; nothing here
        // can, but recovering the guard keeps a poisoned mutex from taking the
        // shadow task down with it.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.open.as_ref().is_none_or(|(open, _)| *open != date) {
            match self.open_file(date) {
                Ok(file) => state.open = Some((date, file)),
                Err(e) => {
                    self.record_failure(&mut state, date, &e);
                    return;
                }
            }
        }
        let Some((_, file)) = state.open.as_mut() else {
            return;
        };
        // A single `write_all` of the newline-terminated record on an O_APPEND
        // file keeps concurrent shadow tasks (and other limen processes sharing
        // the directory) from interleaving partial records.
        if let Err(e) = file.write_all(line.as_bytes()) {
            // Drop the handle so the next mismatch reopens rather than retrying
            // a file descriptor that may be gone (rotated away by an external
            // log rotator, unmounted volume, …).
            state.open = None;
            self.record_failure(&mut state, date, &e);
        } else if state.failing {
            warn!(
                suppressed_failures = state.suppressed,
                dir = %self.dir.display(),
                "limen.diff_sink_recovered"
            );
            state.failing = false;
            state.suppressed = 0;
        }
    }

    /// Open (creating the directory and file as needed) the daily file.
    fn open_file(&self, date: Date) -> std::io::Result<File> {
        std::fs::create_dir_all(&self.dir)?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(file_name(date)))
    }

    /// Warn about an IO failure the first time, then only count it until a write
    /// succeeds again — a sink on a full disk must not drown the logs.
    fn record_failure(&self, state: &mut SinkState, date: Date, error: &std::io::Error) {
        if state.failing {
            state.suppressed = state.suppressed.saturating_add(1);
            return;
        }
        state.failing = true;
        state.suppressed = 0;
        warn!(
            event = "limen.diff_sink_write_failed",
            path = %self.dir.join(file_name(date)).display(),
            error = %error,
            "mismatch record dropped; further failures are counted, not logged"
        );
    }
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
        match serde_json::to_string(&record) {
            Ok(mut line) => {
                line.push('\n');
                self.append(now.date(), &line);
            }
            // Unreachable: every field is a plain serializable type.
            Err(e) => warn!(
                event = "limen.diff_sink_serialize_failed",
                route_id = %meta.route_id,
                error = %e,
            ),
        }
    }

    fn shadow_skipped(&self, _meta: &ShadowMeta, _reason: SkipReason) {}
    fn shadow_failed(&self, _meta: &ShadowMeta, _failure: ShadowFailure) {}
    fn comparison_skipped(&self, _meta: &ShadowMeta, _reason: SkipReason) {}
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
/// `mismatches-YYYY-MM-DD.jsonl`, digits checked.
///
/// Deliberately strict rather than a `mismatches-*.jsonl` glob. A sink directory
/// tends to accumulate operator copies — `mismatches-backup.jsonl`,
/// `mismatches-2026-07-28-copy.jsonl` — and counting those would double-report
/// the very mismatches an operator is trying to size up.
fn is_sink_file_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(FILE_PREFIX) else {
        return false;
    };
    let Some(date) = rest.strip_suffix(FILE_SUFFIX) else {
        return false;
    };
    let date = date.as_bytes();
    date.len() == 10
        && date[4] == b'-'
        && date[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|i| date[*i].is_ascii_digit())
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
        // Second failure was counted, not re-logged.
        let state = sink.state.lock().unwrap();
        assert!(state.failing);
        assert_eq!(state.suppressed, 1);
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
}
