//! Safety invariant 5 as a property of the CLI's output, not of any one message.
//!
//! The specific redactions (`redact_url`, `serde_message`, ...) make particular
//! messages safe and readable. This is the test of the layer under them: every
//! top-level place the binary prints — the error printer, the validation
//! errors, the routing table, the exit-50 reports, the verdict, the draft and its
//! warnings, the report in all three formats — runs its text through
//! `redact_urls_in_text`, so a URL that reaches any of them by a route nobody
//! thought of is cut anyway.
//!
//! The vehicle is a route id (or a path) that IS a credential-bearing URL: ids
//! are free text the operator wrote, nothing validates their shape, and every
//! command echoes them. No message redacts an id specifically, so the only thing
//! standing between the canary and the terminal is the backstop. Each test also
//! asserts the redacted host is still there: the value reached the output and
//! was cut, rather than never arriving.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use limen::observability::observe::{ObserveProfile, RouteProfile};

/// Where a credential and a token would sit, with an unescaped `@` and a query
/// and fragment as well.
const URL: &str = "https://user:CANARY-PW@canary.example/p?token=CANARY-Q#CANARY-F";
/// What is left of [`URL`] once cut.
const CUT_HOST: &str = "canary.example";

const BASE: &str = r#"
server:
  listen_addr: "127.0.0.1:18080"
  graceful_shutdown_timeout_ms: 1000
  request_body_limit_bytes: 1000
metrics:
  listen_addr: "127.0.0.1:18090"
  path: "/metrics"
observe: {}
"#;

/// A config with one comparing route per id given.
fn config(ids: &[&str]) -> String {
    let mut yaml = String::from(BASE);
    yaml.push_str("routes:\n");
    for (i, id) in ids.iter().enumerate() {
        yaml.push_str(&format!(
            "  - id: \"{id}\"\n    match: {{ methods: [\"GET\"], path_prefix: \"/r{i}\" }}\n    \
             legacy_upstream: \"https://l\"\n    new_upstream: \"https://n\"\n    \
             mode: shadow_legacy_primary\n    comparison: {{ enabled: true, sample_rate: 1.0 }}\n"
        ));
    }
    yaml
}

fn workspace(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("limen.config.yaml"), config).expect("config");
    std::fs::create_dir(dir.path().join("sink")).expect("sink");
    dir
}

fn limen(dir: &Path, args: &[&str]) -> Output {
    limen_command(dir, args).output().expect("run limen")
}

fn limen_command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_limen"));
    cmd.current_dir(dir).args(args);
    for var in [
        "LIMEN_CONFIG",
        "LIMEN_LISTEN_ADDR",
        "LIMEN_METRICS_ADDR",
        "LIMEN_FLAGS_PROVIDER",
        "LIMEN_REDIS_URL",
        "LIMEN_FAIL_SAFE_MODE",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Neither stream holds the canary.
fn assert_no_canary(what: &str, output: &Output) {
    let (out, err) = (stdout(output), stderr(output));
    for (name, text) in [("stdout", &out), ("stderr", &err)] {
        assert!(
            !text.contains("CANARY"),
            "{what}: canary in {name}:\n{text}"
        );
    }
}

/// …and the host that survived the cut is in `stream` (`stdout` or `stderr`):
/// the value reached this output point and was redacted there, rather than
/// never arriving.
fn assert_cut(what: &str, output: &Output, stream: fn(&Output) -> String) {
    assert_no_canary(what, output);
    assert!(
        stream(output).contains(CUT_HOST),
        "{what}: the cut URL should still name its host:\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

/// One sink record for `route_id`, as `limen report` and `verdict` read it.
fn mismatch_record(route_id: &str) -> String {
    mismatch_record_at(route_id, "/x?token=CANARY-Q&page=2")
}

fn mismatch_record_at(route_id: &str, path: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "timestamp": "2026-07-27T09:00:00Z",
            "route_id": route_id,
            "request_id": "req-1",
            "method": "GET",
            "path": path,
            "legacy_status": 200,
            "new_status": 200,
            "status_match": true,
            "body_match": false,
            "mismatch_kinds": ["body"],
            "differences": [],
            "header_mismatches": [],
            "cookie_mismatches": [],
            "location_mismatches": [],
            "diff_truncated": false,
        })
    )
}

#[test]
fn the_error_printer_redacts() {
    let ws = workspace(&config(&["r"]));
    // The file does not exist, and the path is the URL.
    let out = limen(ws.path(), &["validate-config", "-c", URL]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).starts_with("Error:"), "{}", stderr(&out));
    assert_cut("main's error printer", &out, stderr);
}

#[test]
fn the_usage_error_printer_redacts() {
    let ws = workspace(&config(&["r"]));
    // clap rejects the value and quotes it: it is not a number.
    let out = limen(ws.path(), &["suggest-routes", "--min-samples", URL]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("invalid value"), "{}", stderr(&out));
    assert_cut("clap's usage error", &out, stderr);

    // `--help` is not an error and is not touched.
    let help = limen(ws.path(), &["--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(stdout(&help).contains("Usage:"), "{}", stdout(&help));
}

#[test]
fn validation_errors_are_redacted() {
    // Two routes with the same id: "duplicate route id <the id>".
    let ws = workspace(&config(&[URL, URL]));
    let out = limen(ws.path(), &["validate-config", "-c", "limen.config.yaml"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("duplicate route id"),
        "{}",
        stderr(&out)
    );
    assert_cut("validate-config", &out, stderr);
}

#[test]
fn the_routing_table_is_redacted() {
    let ws = workspace(&config(&[URL]));
    let out = limen(ws.path(), &["print-routes", "-c", "limen.config.yaml"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_cut("print-routes", &out, stdout);
}

#[test]
fn contract_diagnostics_are_redacted() {
    let ws = workspace(&config(&["r"]));
    let route =
        format!("  - id: \"{URL}\"\n    match: {{ methods: [\"GET\"], path_template: \"/x\" }}\n");
    std::fs::write(
        ws.path().join("svc.contract.yaml"),
        format!("version: 1\nservice: svc\nroutes:\n{route}{route}"),
    )
    .expect("contract");
    let out = limen(ws.path(), &["check-contract", "svc.contract.yaml"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert_cut("check-contract", &out, stderr);
}

#[test]
fn the_exit_fifty_reports_are_redacted_in_both_formats() {
    let ws = workspace(&config(&["r"]));
    // `--dir` is read first offline, and does not exist: the error names it.
    for format in ["human", "json"] {
        let out = limen(
            ws.path(),
            &[
                "verdict",
                "-c",
                "limen.config.yaml",
                "--offline",
                "--dir",
                URL,
                "--format",
                format,
            ],
        );
        assert_eq!(out.status.code(), Some(50), "{format}: {}", stderr(&out));
        assert_cut(
            &format!("verdict --format {format} (unavailable)"),
            &out,
            stdout,
        );
    }
    for format in ["yaml", "json"] {
        let out = limen(
            ws.path(),
            &[
                "suggest-routes",
                "-c",
                "limen.config.yaml",
                "--profile",
                URL,
                "--format",
                format,
            ],
        );
        assert_eq!(out.status.code(), Some(50), "{format}: {}", stderr(&out));
        let stream = if format == "json" { stdout } else { stderr };
        assert_cut(
            &format!("suggest-routes --format {format} (failed)"),
            &out,
            stream,
        );
    }
}

#[test]
fn the_verdict_report_is_redacted_in_both_formats() {
    let ws = workspace(&config(&[URL]));
    std::fs::write(
        ws.path().join("sink/mismatches-2026-07-27.jsonl"),
        mismatch_record(URL),
    )
    .expect("sink record");
    for format in ["human", "json"] {
        let out = limen(
            ws.path(),
            &[
                "verdict",
                "-c",
                "limen.config.yaml",
                "--offline",
                "--dir",
                "sink",
                "--format",
                format,
            ],
        );
        // A recorded mismatch: a verdict (exit 10), not an unavailable input.
        assert_eq!(
            out.status.code(),
            Some(10),
            "{format}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        let what = format!("verdict --format {format}");
        if format == "json" {
            // The JSON report is the one that names routes: by the id it was
            // recorded under.
            assert_cut(&what, &out, stdout);
        } else {
            assert_no_canary(&what, &out);
        }
    }
}

/// A profile that observed `ids`, each a clean read.
fn profile(ids: &[&str]) -> String {
    let routes: BTreeMap<String, RouteProfile> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                id.to_string(),
                RouteProfile {
                    match_basis: format!("prefix:/r{i}"),
                    observations: 20,
                    reads: 20,
                    distinct_read_paths: 1,
                    status_classes: BTreeMap::from([("2xx".to_string(), 20)]),
                    content_types: BTreeSet::from(["application/json".to_string()]),
                    length_repeats: 10,
                    ..RouteProfile::default()
                },
            )
        })
        .collect();
    serde_json::to_string(&ObserveProfile {
        sample_rate: 1.0,
        routes,
    })
    .expect("profile")
}

#[test]
fn the_draft_and_its_warnings_are_redacted() {
    // The configured route and one the config does not define, both URL-shaped:
    // the second is quoted in a stderr warning, the first rides the draft.
    let other = "https://other:CANARY-OTHER@canary.example/o?token=CANARY-Q";
    let ws = workspace(&config(&[URL]));
    std::fs::write(ws.path().join("profile.json"), profile(&[URL, other])).expect("profile");
    for format in ["yaml", "json"] {
        let out = limen(
            ws.path(),
            &[
                "suggest-routes",
                "-c",
                "limen.config.yaml",
                "--profile",
                "profile.json",
                "--format",
                format,
            ],
        );
        assert_eq!(out.status.code(), Some(0), "{format}: {}", stderr(&out));
        assert_cut(&format!("suggest-routes --format {format}"), &out, stdout);
        // The warning about the foreign route id is on stderr, redacted too.
        assert!(
            stderr(&out).contains("route id(s) this config does not define"),
            "{format}: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains(CUT_HOST),
            "{format}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn the_report_is_redacted_in_every_format() {
    let ws = workspace(&config(&["r"]));
    std::fs::write(
        ws.path().join("sink/mismatches-2026-07-27.jsonl"),
        mismatch_record(URL),
    )
    .expect("sink record");

    for format in ["human", "json", "html"] {
        let out = limen(ws.path(), &["report", "--dir", "sink", "--format", format]);
        assert_eq!(out.status.code(), Some(0), "{format}: {}", stderr(&out));
        assert_cut(&format!("report --format {format}"), &out, stdout);
    }

    // `--out` writes a file, which the print path never sees. The HTML page
    // redacts each piece of text it inserts, so it holds without that.
    for format in ["human", "json", "html"] {
        let written = ws.path().join(format!("report.{format}"));
        let out = limen(
            ws.path(),
            &[
                "report",
                "--dir",
                "sink",
                "--format",
                format,
                "--out",
                written.to_str().unwrap(),
            ],
        );
        assert_eq!(out.status.code(), Some(0), "{format}: {}", stderr(&out));
        let file = std::fs::read_to_string(&written).expect("report file");
        assert!(!file.contains("CANARY"), "{format} file:\n{file}");
        assert!(file.contains(CUT_HOST), "{format} file:\n{file}");
    }
}

/// The report is cut where it is built (`--out` never reaches the print) and
/// where it is printed, and never twice over the same text. A second pass used
/// to find the first one's `token=<redacted>`, read the empty value before the
/// `<` as something to hide, and stack a second marker on it: on stdout, and in
/// the HTML page, where the escaped marker turned into a raw `<redacted>` tag.
#[test]
fn the_report_is_redacted_exactly_once() {
    let ws = workspace(&config(&["r"]));
    std::fs::write(
        ws.path().join("sink/mismatches-2026-07-27.jsonl"),
        mismatch_record("r"),
    )
    .expect("sink record");

    // (format, the marker as that format holds it, the expected path text)
    for (format, marker, path) in [
        ("human", "<redacted>", "/x?token=<redacted>&page=2"),
        ("json", "<redacted>", "/x?token=<redacted>&page=2"),
        (
            "html",
            "&lt;redacted&gt;",
            "/x?token=&lt;redacted&gt;&amp;page=2",
        ),
    ] {
        let on_stdout = limen(ws.path(), &["report", "--dir", "sink", "--format", format]);
        assert_eq!(
            on_stdout.status.code(),
            Some(0),
            "{format}: {}",
            stderr(&on_stdout)
        );
        assert_no_canary(&format!("report --format {format}"), &on_stdout);

        let written = ws.path().join(format!("once.{format}"));
        let to_file = limen(
            ws.path(),
            &[
                "report",
                "--dir",
                "sink",
                "--format",
                format,
                "--out",
                written.to_str().unwrap(),
            ],
        );
        assert_eq!(
            to_file.status.code(),
            Some(0),
            "{format}: {}",
            stderr(&to_file)
        );
        let file = std::fs::read_to_string(&written).expect("report file");
        assert!(!file.contains("CANARY"), "{format} file:\n{file}");

        for (where_, text) in [("stdout", stdout(&on_stdout)), ("--out file", file)] {
            assert!(
                text.contains(path),
                "{format} {where_}: no `{path}` in:\n{text}"
            );
            assert_eq!(
                text.matches(marker).count(),
                1,
                "{format} {where_}: exactly one marker:\n{text}"
            );
            // The HTML page's only `<redacted>` would be a raw one, which is not
            // the escaped marker it holds, but a tag the page never wrote.
            if format == "html" {
                assert!(!text.contains("<redacted>"), "{format} {where_}:\n{text}");
            }
        }
    }
}

/// A write that fails for any reason but a closed pipe must not be mistaken for
/// success: a `suggest-routes > draft.yaml` that ran out of disk holds a
/// truncated draft. `/dev/full` fails every write with ENOSPC, which is what a
/// full disk does. Linux-only because that device exists only there (macOS has
/// no equivalent), and CI is Linux.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_write_is_an_error_and_names_only_its_kind() {
    let ws = workspace(&config(&[URL]));
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("open /dev/full");
    let out = limen_command(ws.path(), &["print-routes", "-c", "limen.config.yaml"])
        .stdout(Stdio::from(full))
        .output()
        .expect("run limen");
    // Exit 1 is the unexpected-tooling-error code; 0 is the silent success this
    // guards against, and 101 would be a panic.
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "limen: cannot write to stdout: StorageFull\n",
        "the error's kind, and nothing it was printing"
    );
}

/// The other half of that rule: a closed pipe (`| head`) stays silent and keeps
/// the typed exit code a wrapper branches on, instead of the 101 `println!`
/// panics with.
#[test]
fn a_closed_pipe_is_silent_and_keeps_the_typed_exit_code() {
    let ws = workspace(&config(&[URL]));
    std::fs::write(
        ws.path().join("sink/mismatches-2026-07-27.jsonl"),
        mismatch_record(URL),
    )
    .expect("sink record");
    let mut child = limen_command(
        ws.path(),
        &[
            "verdict",
            "-c",
            "limen.config.yaml",
            "--offline",
            "--dir",
            "sink",
            "--format",
            "human",
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn limen");
    // The reader goes away before the child has loaded its config, let alone
    // printed: its first write meets a pipe with no one at the other end. (Were
    // it to print first, the test would pass for the wrong reason, never fail
    // for one.)
    drop(child.stdout.take());
    let out = child.wait_with_output().expect("wait for limen");
    assert_eq!(out.status.code(), Some(10), "{}", stderr(&out));
    assert_eq!(stderr(&out), "", "a closed pipe is not worth a message");
}

/// The HTML page is printed as `--out` writes it, not run through the text
/// scrubber again: a pass over the escaped page reads the `#` of `&#39;` as the
/// start of a fragment and cuts the entity short (`&#39;https://h/p&;`). The page
/// cuts each piece of text itself, in its own context, so the canary is still
/// gone, from a route id that is a URL as from the quoted one.
#[test]
fn the_html_page_on_stdout_is_the_page_out_writes() {
    let ws = workspace(&config(&["r"]));
    std::fs::write(
        ws.path().join("sink/mismatches-2026-07-27.jsonl"),
        mismatch_record_at(URL, "/x/'https://h/p'"),
    )
    .expect("sink record");

    let on_stdout = limen(ws.path(), &["report", "--dir", "sink", "--format", "html"]);
    assert_eq!(on_stdout.status.code(), Some(0), "{}", stderr(&on_stdout));
    let written = ws.path().join("page.html");
    let to_file = limen(
        ws.path(),
        &[
            "report",
            "--dir",
            "sink",
            "--format",
            "html",
            "--out",
            written.to_str().unwrap(),
        ],
    );
    assert_eq!(to_file.status.code(), Some(0), "{}", stderr(&to_file));
    let file = std::fs::read(&written).expect("report file");

    assert_eq!(
        on_stdout.stdout,
        file,
        "stdout and --out hold different pages:\n{}",
        stdout(&on_stdout)
    );
    assert_cut("report --format html", &on_stdout, stdout);
    let page = stdout(&on_stdout);
    assert!(
        page.contains("&#39;https://h/p&#39;"),
        "the entity and the URL it quotes are intact:\n{page}"
    );
}

/// clap prints an env-bound arg's current value in `--help`
/// (`[env: LIMEN_CONFIG=<value>]`), and help leaves through clap's own printing,
/// not the backstop. The value can be a URL, so help must name the variable and
/// not the value, for the command and for every subcommand.
#[test]
fn help_names_the_env_variable_but_not_its_value() {
    use clap::CommandFactory;
    let ws = workspace(&config(&["r"]));
    let value = "https://user:CANARY-PW@canary.example/config?token=CANARY-Q";
    let command = limen::cli::Cli::command();
    let mut shows_the_variable = 0;
    let helps = std::iter::once(None).chain(command.get_subcommands().map(|s| Some(s.get_name())));
    for sub in helps {
        let args: Vec<&str> = sub.into_iter().chain(["--help"]).collect();
        let out = limen_command(ws.path(), &args)
            .env("LIMEN_CONFIG", value)
            .output()
            .expect("run limen");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
        assert!(
            stdout(&out).contains("Usage:"),
            "{args:?}: {}",
            stdout(&out)
        );
        assert_no_canary(&format!("{args:?}"), &out);
        if stdout(&out).contains("[env: LIMEN_CONFIG]") {
            shows_the_variable += 1;
        }
    }
    // Not vacuous: the variable's name is still shown where it applies.
    assert!(shows_the_variable > 0, "no help names LIMEN_CONFIG");
}
