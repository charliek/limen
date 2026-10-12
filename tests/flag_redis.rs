//! The Redis flag provider against a real socket: what a refresh does when the
//! server answers, when it goes away, when it comes back, and when it never
//! answers (spec §8.2, safety invariant 1).
//!
//! The server is a few dozen lines of RESP rather than a Redis install: tests
//! fail loudly instead of skipping, so they cannot lean on a daemon that may not
//! be running, and a scripted server can die and return on the same port at a
//! moment the test chooses. It speaks only what the provider sends — `KEYS`
//! and `GET`, with every other command (the client's own `CLIENT SETINFO`
//! handshake, say) acknowledged.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{free_port, LogCapture};
use limen::flags::redis_provider::RedisProvider;
use limen::flags::{FlagProvider, FlagValue};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

const PREFIX: &str = "limen:flags:";

/// How the scripted server answers.
#[derive(Clone, Copy)]
enum Behavior {
    /// Serve the data.
    Serve,
    /// Accept connections and never answer: a blackholed server.
    Hang,
    /// Reject authentication the way Redis does.
    RejectAuth,
    /// Serve the data, but take this long over every reply: slow, not dead.
    Slow(Duration),
}

struct FakeRedis(JoinHandle<()>);

impl FakeRedis {
    /// Listen on `port` and serve `data`. The same port can be listened on again
    /// after [`stop`](Self::stop): that is the "server comes back" step.
    async fn start(port: u16, behavior: Behavior, data: &[(&str, &str)]) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind the scripted redis port");
        let data: BTreeMap<String, String> = data
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Self(tokio::spawn(async move {
            // Owned by the accept task, so aborting it drops the listener and
            // aborts every connection with it.
            let mut connections = JoinSet::new();
            while let Ok((sock, _)) = listener.accept().await {
                connections.spawn(serve(sock, behavior, data.clone()));
            }
        }))
    }

    /// Stop listening and drop every open connection, as a crashed or
    /// restarting server does.
    async fn stop(self) {
        self.0.abort();
        let _ = self.0.await;
    }
}

async fn serve(sock: TcpStream, behavior: Behavior, data: BTreeMap<String, String>) {
    if matches!(behavior, Behavior::Hang) {
        // Hold the socket open and say nothing, until the test aborts the task.
        let _sock = sock;
        return std::future::pending().await;
    }
    let (read, mut write) = sock.into_split();
    let mut read = BufReader::new(read);
    while let Some(command) = read_command(&mut read).await {
        let reply = match (command[0].to_ascii_uppercase().as_str(), behavior) {
            ("AUTH", Behavior::RejectAuth) => {
                b"-WRONGPASS invalid username-password pair or user is disabled.\r\n".to_vec()
            }
            ("KEYS", _) => {
                let pattern = command.get(1).map(String::as_str).unwrap_or("*");
                let prefix = pattern.trim_end_matches('*');
                let keys: Vec<&String> = data.keys().filter(|k| k.starts_with(prefix)).collect();
                let mut out = format!("*{}\r\n", keys.len()).into_bytes();
                for key in keys {
                    out.extend(format!("${}\r\n{key}\r\n", key.len()).into_bytes());
                }
                out
            }
            ("GET", _) => match command.get(1).and_then(|k| data.get(k)) {
                Some(value) => format!("${}\r\n{value}\r\n", value.len()).into_bytes(),
                None => b"$-1\r\n".to_vec(),
            },
            _ => b"+OK\r\n".to_vec(),
        };
        if let Behavior::Slow(delay) = behavior {
            tokio::time::sleep(delay).await;
        }
        if write.write_all(&reply).await.is_err() {
            return;
        }
    }
}

/// One RESP command: an array of bulk strings.
async fn read_command(read: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Option<Vec<String>> {
    let mut line = String::new();
    if read.read_line(&mut line).await.ok()? == 0 {
        return None;
    }
    let count: usize = line.trim().strip_prefix('*')?.parse().ok()?;
    let mut args = Vec::with_capacity(count);
    for _ in 0..count {
        line.clear();
        read.read_line(&mut line).await.ok()?;
        let len: usize = line.trim().strip_prefix('$')?.parse().ok()?;
        let mut buf = vec![0u8; len + 2];
        read.read_exact(&mut buf).await.ok()?;
        buf.truncate(len);
        args.push(String::from_utf8_lossy(&buf).into_owned());
    }
    Some(args)
}

fn provider(url: &str, interval: Duration) -> RedisProvider {
    RedisProvider::new(url, PREFIX.to_string(), interval, Duration::from_secs(30))
        .expect("a well-formed redis url opens a client without I/O")
}

#[tokio::test]
async fn a_refresh_reads_every_flag_under_the_prefix() {
    let port = free_port();
    let server = FakeRedis::start(
        port,
        Behavior::Serve,
        &[
            ("limen:flags:rollout", "25"),
            ("limen:flags:shadow", "true"),
            ("limen:flags:mode", "legacy_only"),
            ("someone:else", "1"),
        ],
    )
    .await;

    let flags = provider(&format!("redis://127.0.0.1:{port}"), Duration::from_secs(1));
    flags.refresh().await;

    assert_eq!(flags.get("rollout").await, Some(FlagValue::Number(25.0)));
    assert_eq!(flags.get("shadow").await, Some(FlagValue::Bool(true)));
    assert_eq!(
        flags.get("mode").await,
        Some(FlagValue::String("legacy_only".into()))
    );
    assert_eq!(
        flags.get("else").await,
        None,
        "other prefixes are not flags"
    );
    let health = flags.health();
    assert!(!health.stale);
    assert_eq!(health.consecutive_failures, 0);
    server.stop().await;
}

/// The fail-safe the provider exists for: while the server is gone the last
/// known good values keep being served and the failures are counted; when it
/// returns, the next refresh dials fresh and takes the new values.
#[tokio::test]
async fn a_dead_server_keeps_last_known_good_and_a_returning_one_is_picked_up() {
    let port = free_port();
    let url = format!("redis://127.0.0.1:{port}");
    let server = FakeRedis::start(port, Behavior::Serve, &[("limen:flags:rollout", "25")]).await;
    let flags = provider(&url, Duration::from_secs(1));
    flags.refresh().await;
    assert_eq!(flags.get("rollout").await, Some(FlagValue::Number(25.0)));

    // The server dies: connecting is refused.
    server.stop().await;
    flags.refresh().await;
    flags.refresh().await;
    assert_eq!(
        flags.get("rollout").await,
        Some(FlagValue::Number(25.0)),
        "a failed refresh must keep the last known good value"
    );
    let health = flags.health();
    assert_eq!(health.consecutive_failures, 2);
    assert!(
        !health.stale,
        "staleness comes from the TTL on the last success, not from one failure"
    );

    // It comes back on the same address with a different value.
    let server = FakeRedis::start(port, Behavior::Serve, &[("limen:flags:rollout", "75")]).await;
    flags.refresh().await;
    assert_eq!(
        flags.get("rollout").await,
        Some(FlagValue::Number(75.0)),
        "the next refresh must reconnect and take the new value"
    );
    assert_eq!(flags.health().consecutive_failures, 0);
    server.stop().await;
}

/// A live Redis that is merely slow must be given the provider's whole budget,
/// not the client library's defaults: redis 1.x times a connect out at 1 s and a
/// command at 500 ms where 0.27 did neither, and the provider's contract is one
/// bound, the poll interval. Every reply here takes 700 ms, so the connect (two
/// pipelined `CLIENT SETINFO` replies), `KEYS` and then a `GET` all succeed only
/// if neither default is in force.
#[tokio::test]
async fn a_slow_but_live_server_is_given_the_whole_budget() {
    let port = free_port();
    let server = FakeRedis::start(
        port,
        Behavior::Slow(Duration::from_millis(700)),
        &[("limen:flags:rollout", "25")],
    )
    .await;
    let flags = provider(&format!("redis://127.0.0.1:{port}"), Duration::from_secs(5));

    flags.refresh().await;

    assert_eq!(
        flags.get("rollout").await,
        Some(FlagValue::Number(25.0)),
        "a server that answers within the budget must not be failed early"
    );
    assert_eq!(flags.health().consecutive_failures, 0);
    server.stop().await;
}

/// A server that accepts and never answers must not hang the refresh (and with
/// it startup): the load is bounded by the poll interval.
#[tokio::test]
async fn a_blackholed_server_fails_the_refresh_within_the_poll_interval() {
    let port = free_port();
    let server = FakeRedis::start(port, Behavior::Hang, &[]).await;
    let flags = provider(
        &format!("redis://127.0.0.1:{port}"),
        Duration::from_millis(300),
    );

    // The outer timeout is the test's own: if the provider's bound were lost this
    // would otherwise hang the run instead of failing it.
    tokio::time::timeout(Duration::from_secs(3), flags.refresh())
        .await
        .expect("the refresh was not bounded by the poll interval");
    let health = flags.health();
    assert_eq!(health.consecutive_failures, 1);
    assert!(
        health.stale,
        "never succeeded, so stale (fail-safe to legacy)"
    );
    server.stop().await;
}

/// Invariant 5 on the Redis path: a refresh that fails logs the error, and the
/// error must not carry the password in the configured URL. Two failures: a
/// refused connection, and a server that rejects the credentials.
#[tokio::test]
async fn a_failed_refresh_logs_no_credentials() {
    const PASSWORD: &str = "CANARY-REDIS-PASS-4c1d90";
    const USERNAME: &str = "CANARY-REDIS-USER-4c1d90";

    /// One refresh against `port` with both canaries in the URL, and what it
    /// logged — in which neither may appear.
    async fn failed_refresh(port: u16) -> (RedisProvider, String) {
        let logs = LogCapture::start();
        let flags = provider(
            &format!("redis://{USERNAME}:{PASSWORD}@127.0.0.1:{port}/0"),
            Duration::from_secs(1),
        );
        flags.refresh().await;
        let text = logs.text();
        for secret in [PASSWORD, USERNAME] {
            assert!(!text.contains(secret), "{secret} reached the log:\n{text}");
        }
        (flags, text)
    }

    // Refused: nothing listens on the port.
    let (_, text) = failed_refresh(free_port()).await;
    assert!(
        text.contains("redis flag refresh failed"),
        "the failure was not logged:\n{text}"
    );

    // Rejected: the server answers AUTH with WRONGPASS.
    let port = free_port();
    let server = FakeRedis::start(port, Behavior::RejectAuth, &[]).await;
    let (flags, text) = failed_refresh(port).await;
    assert!(
        text.contains("redis flag refresh failed") && text.contains("authentication failed"),
        "the rejection was not logged as such:\n{text}"
    );
    assert_eq!(flags.health().consecutive_failures, 1);
    server.stop().await;
}
