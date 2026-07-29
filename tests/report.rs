//! `limen report` over a diff-sink directory (spec §10.4).
//!
//! The reading half is exercised as a library function
//! ([`limen::observability::sink::read_report`]) rather than by spawning the
//! binary — the thin-binary convention: `cli.rs` only parses flags and renders
//! what this returns.

use std::path::Path;

use limen::observability::sink::{read_report, Report, ReportFilter};
use tempfile::TempDir;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// How many examples per route the tests ask for unless they are exercising the
/// cap itself.
const EXAMPLES: usize = 3;

/// One sink line. Written by hand (not via `SinkObserver`) so the fixture pins
/// the on-disk format independently of the writer.
fn line(timestamp: &str, route: &str, request_id: &str, kinds: &[&str]) -> String {
    serde_json::json!({
        "timestamp": timestamp,
        "route_id": route,
        "request_id": request_id,
        "method": "GET",
        "path": format!("/{route}/1"),
        "legacy_status": 200,
        "new_status": 200,
        "status_match": true,
        "body_match": !kinds.contains(&"body"),
        "mismatch_kinds": kinds,
        "differences": [],
        "header_mismatches": [],
        "cookie_mismatches": [],
        "location_mismatches": [],
        "diff_truncated": false,
    })
    .to_string()
}

/// A sink directory with two routes, mixed mismatch kinds spread over two daily
/// files, one malformed line, one blank line, and one unrelated file.
fn fixture(dir: &Path) {
    let day_one = [
        line("2026-07-27T09:00:00Z", "get-device", "req-1", &["body"]),
        line(
            "2026-07-27T09:00:01Z",
            "get-device",
            "req-2",
            &["body", "set_cookie.value"],
        ),
        // Malformed: not JSON at all (a torn write, or a stray log line).
        "{not json".to_string(),
        String::new(),
        line("2026-07-27T09:00:02Z", "list-devices", "req-3", &["status"]),
    ];
    let day_two = [
        line("2026-07-28T10:00:00Z", "get-device", "req-4", &["body"]),
        line(
            "2026-07-28T10:00:05Z",
            "get-device",
            "req-5",
            &["location.query"],
        ),
        // Malformed: valid JSON, but no `route_id` to group it under.
        r#"{"timestamp":"2026-07-28T10:00:06Z","method":"GET"}"#.to_string(),
        // Malformed: unparseable timestamp — it could be neither ordered nor
        // filtered honestly, so it is reported rather than silently included.
        r#"{"timestamp":"not-a-time","route_id":"get-device"}"#.to_string(),
        line("2026-07-28T10:00:07Z", "list-devices", "req-6", &["status"]),
    ];
    std::fs::write(dir.join("mismatches-2026-07-27.jsonl"), day_one.join("\n")).unwrap();
    std::fs::write(
        dir.join("mismatches-2026-07-28.jsonl"),
        format!("{}\n", day_two.join("\n")),
    )
    .unwrap();
    // None of these is a sink file, so none is read. The decoys carry *valid*
    // records, so a filter loose enough to admit them would inflate every count
    // below rather than fail loudly.
    std::fs::write(dir.join("README.md"), "not a sink file").unwrap();
    std::fs::write(dir.join("mismatches-2026-07-29.jsonl.gz"), "compressed").unwrap();
    for decoy in DECOY_FILES {
        std::fs::write(
            dir.join(decoy),
            format!(
                "{}\n",
                line("2026-07-28T11:00:00Z", "get-device", "decoy", &["body"])
            ),
        )
        .unwrap();
    }
}

/// File names an operator's `cp`/`gzip`/editor might leave in a sink directory.
/// A `mismatches-*.jsonl` glob would read the first three of these.
const DECOY_FILES: &[&str] = &[
    "mismatches-backup.jsonl",
    "mismatches-2026-07-28-copy.jsonl",
    "mismatches-.jsonl",
    "mismatches-2026-7-8.jsonl",
    "mismatches-2026-07-28.json",
    "old-mismatches-2026-07-28.jsonl",
];

/// A temp directory holding the [`fixture`] sink files.
fn fixture_dir() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    dir
}

/// Report over `dir` with the standard example cap and the given filters
/// (`since` as an RFC 3339 string, as the CLI takes it).
fn report_for(dir: &Path, route: Option<&str>, since: Option<&str>) -> Report {
    let filter = ReportFilter {
        route: route.map(str::to_string),
        since: since.map(|ts| OffsetDateTime::parse(ts, &Rfc3339).unwrap()),
    };
    read_report(dir, &filter, EXAMPLES).unwrap()
}

#[test]
fn aggregates_counts_and_kinds_per_route() {
    let dir = fixture_dir();
    let report = report_for(dir.path(), None, None);

    assert_eq!(
        report.files_read, 2,
        "only mismatches-<date>.jsonl files are read"
    );
    assert_eq!(report.total, 6);
    // Routes are ordered by mismatch count, descending.
    let ids: Vec<&str> = report.routes.iter().map(|r| r.route_id.as_str()).collect();
    assert_eq!(ids, ["get-device", "list-devices"]);

    let get_device = &report.routes[0];
    assert_eq!(get_device.count, 4);
    assert_eq!(get_device.kinds["body"], 3);
    assert_eq!(get_device.kinds["set_cookie.value"], 1);
    assert_eq!(get_device.kinds["location.query"], 1);
    assert_eq!(report.routes[1].count, 2);
    assert_eq!(report.routes[1].kinds["status"], 2);
}

#[test]
fn malformed_lines_are_counted_not_fatal() {
    let dir = fixture_dir();
    let report = report_for(dir.path(), None, None);
    // Non-JSON, missing route_id, unparseable timestamp — the blank line is
    // skipped silently (it is not a damaged record).
    assert_eq!(report.malformed_lines, 3);
    // …and every well-formed record around them still made it in.
    assert_eq!(report.total, 6);
}

#[test]
fn examples_are_the_most_recent_and_bounded() {
    let dir = fixture_dir();
    let report = report_for(dir.path(), None, None);
    let get_device = &report.routes[0];
    assert_eq!(
        get_device.examples.len(),
        EXAMPLES,
        "capped at the requested count"
    );
    let ids: Vec<&str> = get_device
        .examples
        .iter()
        .map(|e| e.request_id.as_str())
        .collect();
    assert_eq!(ids, ["req-5", "req-4", "req-2"], "newest first");
    assert_eq!(get_device.examples[0].method, "GET");
    assert_eq!(get_device.examples[0].path, "/get-device/1");
    assert_eq!(get_device.examples[0].mismatch_kinds, ["location.query"]);

    // A smaller cap truncates; a larger one is not padded.
    let one = read_report(dir.path(), &ReportFilter::default(), 1).unwrap();
    assert_eq!(one.routes[0].examples.len(), 1);
    let many = read_report(dir.path(), &ReportFilter::default(), 50).unwrap();
    assert_eq!(many.routes[0].examples.len(), 4);
}

#[test]
fn route_filter_selects_one_route() {
    let dir = fixture_dir();

    let report = report_for(dir.path(), Some("list-devices"), None);
    assert_eq!(report.routes.len(), 1);
    assert_eq!(report.routes[0].route_id, "list-devices");
    assert_eq!(report.total, 2);
    // Filtering records does not hide damaged lines.
    assert_eq!(report.malformed_lines, 3);

    let unknown = report_for(dir.path(), Some("nope"), None);
    assert_eq!(unknown.total, 0);
    assert!(unknown.routes.is_empty());
}

#[test]
fn since_filter_is_inclusive_of_its_instant() {
    let dir = fixture_dir();

    // Everything from the second day.
    let report = report_for(dir.path(), None, Some("2026-07-28T00:00:00Z"));
    assert_eq!(report.total, 3);

    // Exactly on a record's timestamp: that record is included.
    let boundary = report_for(dir.path(), None, Some("2026-07-28T10:00:05Z"));
    assert_eq!(boundary.total, 2);
    assert!(boundary
        .routes
        .iter()
        .flat_map(|r| &r.examples)
        .any(|e| e.request_id == "req-5"));

    // Offsets are honored, not assumed to be UTC: 10:00:05Z == 12:00:05+02:00.
    let offset = report_for(dir.path(), None, Some("2026-07-28T12:00:05+02:00"));
    assert_eq!(offset.total, 2);
}

#[test]
fn filters_compose() {
    let dir = fixture_dir();

    let report = report_for(dir.path(), Some("get-device"), Some("2026-07-28T00:00:00Z"));
    assert_eq!(report.total, 2);
    assert_eq!(report.routes[0].kinds["body"], 1);
    assert_eq!(report.routes[0].kinds["location.query"], 1);
}

#[test]
fn an_empty_directory_reports_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let report = report_for(dir.path(), None, None);
    assert_eq!(report.total, 0);
    assert_eq!(report.files_read, 0);
    assert_eq!(report.malformed_lines, 0);
    assert!(report.routes.is_empty());
}

#[test]
fn a_missing_directory_is_an_error_not_an_empty_report() {
    let dir = tempfile::tempdir().unwrap();
    let err =
        read_report(&dir.path().join("nope"), &ReportFilter::default(), EXAMPLES).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

/// A torn write can split a multi-byte character, leaving a line that is not
/// valid UTF-8. That must cost the line, not the file: decoding per line keeps
/// the records on either side of the damage readable.
#[test]
fn invalid_utf8_costs_only_its_own_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut contents: Vec<u8> = Vec::new();
    contents.extend_from_slice(
        line("2026-07-28T10:00:00Z", "get-device", "before", &["body"]).as_bytes(),
    );
    contents.push(b'\n');
    // A record cut mid-character: the leading byte of a 3-byte sequence (the
    // `…` in a truncated path) with its continuation bytes lost to the tear.
    contents.extend_from_slice(
        br#"{"timestamp":"2026-07-28T10:00:01Z","route_id":"get-device","path":"/devices/"#,
    );
    contents.push(0xE2);
    contents.push(b'\n');
    contents.extend_from_slice(
        line("2026-07-28T10:00:02Z", "get-device", "after", &["status"]).as_bytes(),
    );
    contents.push(b'\n');
    std::fs::write(dir.path().join("mismatches-2026-07-28.jsonl"), &contents).unwrap();

    let report = report_for(dir.path(), None, None);
    assert_eq!(report.malformed_lines, 1);
    assert_eq!(report.total, 2, "the records either side survive");
    let ids: Vec<&str> = report.routes[0]
        .examples
        .iter()
        .map(|e| e.request_id.as_str())
        .collect();
    assert_eq!(ids, ["after", "before"]);
}

/// Only the writer's exact `mismatches-YYYY-MM-DD.jsonl` shape is read. Operator
/// copies sitting next to the real files must not double-count the mismatches
/// they contain.
#[test]
fn operator_copies_in_the_sink_directory_are_ignored() {
    let dir = fixture_dir();
    let report = report_for(dir.path(), None, None);

    assert_eq!(report.files_read, 2);
    assert_eq!(report.total, 6);
    // Each decoy holds one valid `get-device` record; none of them counted.
    assert_eq!(report.routes[0].route_id, "get-device");
    assert_eq!(report.routes[0].count, 4);
    assert!(
        !report
            .routes
            .iter()
            .flat_map(|r| &r.examples)
            .any(|e| e.request_id == "decoy"),
        "a decoy file's record reached the report"
    );

    // Sanity check on the fixture itself: the decoys really are on disk, and
    // really do hold readable records — renaming one to the exact shape brings
    // its record in.
    for decoy in DECOY_FILES {
        assert!(dir.path().join(decoy).is_file(), "{decoy} missing");
    }
    std::fs::rename(
        dir.path().join("mismatches-backup.jsonl"),
        dir.path().join("mismatches-2026-07-26.jsonl"),
    )
    .unwrap();
    let after = report_for(dir.path(), None, None);
    assert_eq!(after.files_read, 3);
    assert_eq!(after.total, 7);
}

#[test]
fn unknown_fields_from_a_newer_writer_are_tolerated() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("mismatches-2026-07-28.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({
                "timestamp": "2026-07-28T10:00:00Z",
                "route_id": "get-device",
                "request_id": "req-1",
                "method": "GET",
                "path": "/devices/1",
                "mismatch_kinds": ["body"],
                // A field this binary has never heard of.
                "future_dimension_mismatches": [{"kind": "whatever"}],
            })
        ),
    )
    .unwrap();

    let report = report_for(dir.path(), None, None);
    assert_eq!(report.malformed_lines, 0);
    assert_eq!(report.total, 1);
    assert_eq!(report.routes[0].kinds["body"], 1);
}
