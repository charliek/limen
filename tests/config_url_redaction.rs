//! Safety invariant 5 on the configuration surfaces: a URL an operator wrote
//! into the config (`flags.redis.url`, a route's upstreams) or onto a command
//! line (`--control-url`) can carry a password in its userinfo and a token in
//! its query, and no message `limen` prints about it may repeat either.
//!
//! Each test drives the real binary, because the messages are made on the way
//! to the terminal: `load_and_validate` prints validation errors to stderr,
//! `print-routes` prints the table to stdout, and the exit-50 verdicts are
//! rendered by the CLI in two formats. Every URL carries a canary where a real
//! credential would sit, and no stream of any run may contain it, while the
//! field or URL it belongs to is still named. The unit tests beside
//! `config::validate` and `routing::matcher` cover the same values below the
//! CLI.

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CANARY: &str = "CANARY-9z";

/// Values a mistyped config or a copy-pasted flag might hold. The first two are
/// the shape of a real credential-bearing URL that fails to parse; the third is
/// a value we cannot tell any part of.
const UNPARSEABLE: [&str; 3] = [
    "redis://:CANARY-9z@host:99999",
    "https://user:CANARY-9z@x:99999/?token=CANARY-9z",
    "CANARY-9z is not a url",
];

const BASE_CONFIG: &str = r#"
server:
  listen_addr: "127.0.0.1:18080"
  graceful_shutdown_timeout_ms: 1000
  request_body_limit_bytes: 1000
metrics:
  listen_addr: "127.0.0.1:18090"
  path: "/metrics"
"#;

/// A config whose one route has the given upstreams, and whose flags provider
/// is `redis` at `redis_url` when one is given (static otherwise).
fn config(redis_url: Option<&str>, legacy: &str, new: &str) -> String {
    let flags = match redis_url {
        Some(url) => format!(
            "flags:\n  provider: redis\n  redis: {{ url: \"{url}\", key_prefix: \"p\", \
             refresh_interval_ms: 1000 }}\n  stale_ttl_ms: 30000\n  fail_safe_mode: legacy_only\n"
        ),
        None => String::new(),
    };
    format!(
        r#"{BASE_CONFIG}{flags}routes:
  - id: "r1"
    match:
      methods: ["GET"]
      path_prefix: "/a"
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: "shadow_legacy_primary"
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 1000
"#
    )
}

struct Workspace {
    dir: tempfile::TempDir,
}

impl Workspace {
    fn with_config(yaml: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("limen.config.yaml"), yaml).expect("config");
        std::fs::create_dir(dir.path().join("sink")).expect("sink dir");
        Self { dir }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_limen"));
        cmd.current_dir(self.dir.path())
            .args(args)
            .args(["-c", "limen.config.yaml"]);
        // The binary reads these as overrides; a developer's shell must not
        // change what the test feeds it.
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

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run limen")
    }

    /// The pre-created sink directory, as the string `--dir` takes.
    fn sink_dir(&self) -> String {
        self.dir
            .path()
            .join("sink")
            .to_str()
            .expect("utf-8 temp path")
            .to_owned()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn text(output: &Output) -> String {
    format!("stdout:\n{}\nstderr:\n{}", stdout(output), stderr(output))
}

/// Nothing either stream of `output` holds may contain the canary.
fn assert_no_canary(what: &str, output: &Output) {
    let all = text(output);
    assert!(!all.contains(CANARY), "{what} leaked the canary:\n{all}");
}

#[test]
fn validate_config_never_echoes_an_unparseable_redis_url() {
    for bad in UNPARSEABLE {
        let ws = Workspace::with_config(&config(Some(bad), "https://l", "https://n"));
        let out = ws.run(&["validate-config"]);
        assert_eq!(out.status.code(), Some(1), "{bad}: {}", text(&out));
        assert_no_canary(bad, &out);
        let err = stderr(&out);
        assert!(
            err.contains("flags.redis.url: is not a valid URL"),
            "the field must still be named: {err}"
        );
    }
}

#[test]
fn validate_config_never_echoes_an_unparseable_upstream() {
    for bad in UNPARSEABLE {
        let ws = Workspace::with_config(&config(None, bad, bad));
        let out = ws.run(&["validate-config"]);
        assert_eq!(out.status.code(), Some(1), "{bad}: {}", text(&out));
        assert_no_canary(bad, &out);
        let err = stderr(&out);
        for field in ["legacy_upstream", "new_upstream"] {
            assert!(
                err.contains(&format!("routes[0] \"r1\".{field}: is not a valid URL")),
                "{field} must still be named: {err}"
            );
        }
    }
}

/// The override layers feed the same validator, so the same value arriving as
/// `--redis-url` or `LIMEN_REDIS_URL` must stay out of the message too.
#[test]
fn an_overridden_redis_url_is_not_echoed_either() {
    let ws = Workspace::with_config(&config(
        Some("redis://localhost:6379"),
        "https://l",
        "https://n",
    ));
    let bad = UNPARSEABLE[0];

    let flag = ws.run(&["validate-config", "--redis-url", bad]);
    assert_eq!(flag.status.code(), Some(1), "{}", text(&flag));
    assert_no_canary("--redis-url", &flag);
    assert!(stderr(&flag).contains("flags.redis.url: is not a valid URL"));

    let env = ws
        .command(&["validate-config"])
        .env("LIMEN_REDIS_URL", bad)
        .output()
        .expect("run limen");
    assert_eq!(env.status.code(), Some(1), "{}", text(&env));
    assert_no_canary("LIMEN_REDIS_URL", &env);
    assert!(stderr(&env).contains("flags.redis.url: is not a valid URL"));
}

/// A URL that parses is accepted, userinfo and query and all, so the routing
/// table has to be the thing that leaves them out.
#[test]
fn print_routes_shows_upstreams_without_userinfo_query_or_fragment() {
    let ws = Workspace::with_config(&config(
        None,
        &format!("https://user:{CANARY}@legacy.internal:8443/api/?token={CANARY}#{CANARY}"),
        &format!("https://{CANARY}@new.internal/?token={CANARY}"),
    ));

    // The premise: validation accepts these, so print-routes does run on them.
    let valid = ws.run(&["validate-config"]);
    assert_eq!(valid.status.code(), Some(0), "{}", text(&valid));

    let out = ws.run(&["print-routes"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_no_canary("print-routes", &out);
    let table = stdout(&out);
    assert!(
        table.contains("  legacy:     https://legacy.internal:8443/api/\n"),
        "{table}"
    );
    assert!(
        table.contains("  new:        https://new.internal\n"),
        "{table}"
    );
}

/// `redis` can't build a client from a URL the `url` crate accepted, and says
/// why by quoting the part it rejected: a `?protocol=` value, here. The
/// startup failure keeps the reason and drops the quote.
#[test]
fn run_refuses_a_redis_url_the_client_rejects_without_quoting_it() {
    let ws = Workspace::with_config(&config(
        Some(&format!(
            "redis://:{CANARY}@localhost:6379/0?protocol={CANARY}"
        )),
        "https://l",
        "https://n",
    ));
    let valid = ws.run(&["validate-config"]);
    assert_eq!(valid.status.code(), Some(0), "premise: {}", text(&valid));

    let mut child = ws
        .command(&["run"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn limen");
    // `run` must refuse before it serves; a server that came up is a failure,
    // not something to wait on.
    let started = Instant::now();
    while child.try_wait().expect("poll limen").is_none() {
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("limen run did not refuse to start");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().expect("collect output");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert_no_canary("run", &out);
    let err = stderr(&out);
    assert!(err.contains("flags.redis.url"), "{err}");
    assert!(err.contains("Invalid protocol version"), "{err}");
}

/// `--control-url` is an operator-written URL like any other: when the control
/// plane is unreachable, the exit-50 report names it, and may not carry its
/// userinfo or query — in the human report or the JSON one.
#[test]
fn an_unreachable_control_url_is_reported_without_userinfo_or_query() {
    let ws = Workspace::with_config(&config(None, "https://l", "https://n"));
    let sink = ws.sink_dir();

    // (the --control-url value, the URL the failing request must read as)
    for (control_url, shown) in [
        (
            format!("http://user:{CANARY}@127.0.0.1:1"),
            "http://127.0.0.1:1/metrics",
        ),
        // A query swallows the appended path, so only the base is left to show.
        (
            format!("http://user:{CANARY}@127.0.0.1:1/ctl?token={CANARY}"),
            "http://127.0.0.1:1/ctl",
        ),
    ] {
        for (command, format) in [
            ("verdict", "human"),
            ("verdict", "json"),
            ("suggest-routes", "yaml"),
            ("suggest-routes", "json"),
        ] {
            let mut argv = vec![command];
            // The sink directory is a verdict input; suggest-routes has none.
            if command == "verdict" {
                argv.extend(["--dir", &sink]);
            }
            argv.extend(["--control-url", &control_url, "--format", format]);
            let out = ws.run(&argv);
            let all = text(&out);
            assert_eq!(out.status.code(), Some(50), "{command} {format}: {all}");
            assert_no_canary(&format!("{command} --format {format}"), &out);
            // `suggest-routes` scrapes `/metrics` first, so the same URL is the
            // one named.
            assert_eq!(
                all.matches(shown).count(),
                1,
                "{command} --format {format} must name {shown} once: {all}"
            );
        }
    }
}

/// One control-plane stub answer: `(http method, path, status, body)`.
type Stub = (&'static str, &'static str, u16, &'static str);

/// Run `args` (`verdict` or `suggest-routes`) against a control plane answering
/// as `stubs` say, with a `--control-url` that carries a password and a token,
/// in both of the command's output formats. The run must exit 50 and name the
/// URL it asked once, as `scheme://host:port/path`, beside the message
/// `marker` — and the canary in neither stream. These are the messages for a
/// control plane that answered badly rather than not at all; the unreachable
/// case is the test above.
async fn assert_bad_answer_is_reported_redacted(
    args: &[&str],
    stubs: &[Stub],
    marker: &str,
    asked: &str,
) {
    let server = MockServer::start().await;
    for (verb, route, status, body) in stubs {
        Mock::given(method(*verb))
            .and(path(*route))
            .respond_with(ResponseTemplate::new(*status).set_body_string(*body))
            .mount(&server)
            .await;
    }
    let addr = server.address();
    let control_url = format!("http://user:{CANARY}@{addr}/");
    let shown = format!("http://{addr}{asked}");

    let ws = Workspace::with_config(&config(None, "https://l", "https://n"));
    let sink = ws.sink_dir();
    let is_verdict = args[0] == "verdict";
    let formats = if is_verdict {
        ["human", "json"]
    } else {
        ["yaml", "json"]
    };
    for format in formats {
        let mut argv: Vec<&str> = args.to_vec();
        // The sink directory is a verdict input; suggest-routes has none.
        if is_verdict {
            argv.extend(["--dir", &sink]);
        }
        argv.extend(["--control-url", &control_url, "--format", format]);
        let out = tokio::task::spawn_blocking({
            let mut cmd = ws.command(&argv);
            move || cmd.output().expect("run limen")
        })
        .await
        .expect("join");
        let all = text(&out);
        assert_eq!(out.status.code(), Some(50), "{args:?} {format}: {all}");
        assert_no_canary(&format!("{args:?} --format {format}"), &out);
        assert!(
            all.contains(marker),
            "{args:?} {format} lost {marker:?}: {all}"
        );
        assert_eq!(
            all.matches(&shown).count(),
            1,
            "{args:?} {format} must name {shown} once: {all}"
        );
    }
}

#[tokio::test]
async fn verdict_reports_a_failing_scrape_without_the_control_url_credentials() {
    assert_bad_answer_is_reported_redacted(
        &["verdict"],
        &[("GET", "/metrics", 500, "")],
        "control plane returned HTTP 500",
        "/metrics",
    )
    .await;
}

#[tokio::test]
async fn verdict_reports_a_refused_canary_without_the_control_url_credentials() {
    assert_bad_answer_is_reported_redacted(
        &["verdict", "--canary"],
        &[("POST", "/debug/canary", 403, "")],
        "canary trigger refused at",
        "/debug/canary",
    )
    .await;
}

#[tokio::test]
async fn verdict_reports_a_non_json_canary_answer_without_the_control_url_credentials() {
    assert_bad_answer_is_reported_redacted(
        &["verdict", "--canary"],
        &[("POST", "/debug/canary", 200, "not json")],
        "returned a non-JSON body",
        "/debug/canary",
    )
    .await;
}

#[tokio::test]
async fn verdict_reports_an_unacknowledged_canary_without_the_control_url_credentials() {
    assert_bad_answer_is_reported_redacted(
        &["verdict", "--canary"],
        &[("POST", "/debug/canary", 200, r#"{"injected": false}"#)],
        "did not acknowledge the injection",
        "/debug/canary",
    )
    .await;
}

#[tokio::test]
async fn suggest_routes_reports_a_failing_scrape_without_the_control_url_credentials() {
    assert_bad_answer_is_reported_redacted(
        &["suggest-routes"],
        &[("GET", "/metrics", 500, "")],
        "control plane returned HTTP 500",
        "/metrics",
    )
    .await;
}

#[tokio::test]
async fn suggest_routes_reports_a_scrape_without_the_gauge_without_the_credentials() {
    assert_bad_answer_is_reported_redacted(
        &["suggest-routes"],
        &[("GET", "/metrics", 200, "some_other_series 1\n")],
        "required series limen_in_flight_requests absent from",
        "/metrics",
    )
    .await;
}

#[tokio::test]
async fn suggest_routes_reports_a_missing_profile_without_the_credentials() {
    // No stub for the profile: the stub server answers 404, which the command
    // reads as "this proxy has no `observe:` block".
    assert_bad_answer_is_reported_redacted(
        &["suggest-routes"],
        &[("GET", "/metrics", 200, "limen_in_flight_requests 0\n")],
        "returned 404",
        "/observe/profile",
    )
    .await;
}

#[tokio::test]
async fn suggest_routes_reports_a_failing_profile_without_the_credentials() {
    assert_bad_answer_is_reported_redacted(
        &["suggest-routes"],
        &[
            ("GET", "/metrics", 200, "limen_in_flight_requests 0\n"),
            ("GET", "/observe/profile", 500, ""),
        ],
        "control plane returned HTTP 500",
        "/observe/profile",
    )
    .await;
}

#[tokio::test]
async fn suggest_routes_reports_a_garbled_profile_without_the_credentials() {
    assert_bad_answer_is_reported_redacted(
        &["suggest-routes"],
        &[
            ("GET", "/metrics", 200, "limen_in_flight_requests 0\n"),
            ("GET", "/observe/profile", 200, "not json"),
        ],
        "did not serve an observe profile",
        "/observe/profile",
    )
    .await;
}
