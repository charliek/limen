//! The rustls crypto provider, in isolation.
//!
//! rustls keeps ONE process-wide default provider, and
//! `rustls::ClientConfig::builder()` — which redis-rs calls on every `rediss://`
//! connect — panics unless it can name one: either an installed default, or
//! exactly one provider feature compiled in. A build that gains a second
//! provider (`ring` next to aws-lc-rs) therefore fails at the first TLS connect
//! and nowhere before it; no plain-`redis://` test and no unit test reaches it.
//!
//! Both of these tests run their body in a fresh process that starts with no
//! provider installed, so a default installed by an earlier test in this binary
//! cannot cover for the build. The parent re-runs this test binary with one
//! child test selected; the child is `#[ignore]`d so an ordinary run skips it,
//! and checks its own premise so running it any other way fails instead of
//! passing vacuously.

use std::net::TcpListener;
use std::process::Command;
use std::time::Duration;

/// Run the child test `name` ALONE in a fresh copy of this test binary and
/// require that exactly that one test ran and passed.
fn run_alone_in_a_fresh_process(name: &str) {
    let exe = std::env::current_exe().expect("the test binary's path");
    let output = Command::new(exe)
        .args([name, "--exact", "--include-ignored"])
        .output()
        .expect("re-running the test binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the fresh-process run of {name} failed:\n{stdout}\n{stderr}"
    );
    // An `--exact` filter that matches nothing also exits 0.
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the fresh-process run did not run exactly {name}:\n{stdout}"
    );
}

/// The premise both children share: nothing in this process has installed a
/// default provider. Fails if the child is run in a process that has.
fn assert_no_provider_installed() {
    assert!(
        rustls::crypto::CryptoProvider::get_default().is_none(),
        "premise: this child must start with no default crypto provider installed"
    );
}

/// A `rediss://` connect to a listener that accepts and hangs up on the
/// handshake must come back as an error — and, the point of the test, must not
/// panic in `ClientConfig::builder()` on its way there.
#[test]
fn a_rediss_connect_without_an_installed_provider_errors_instead_of_panicking() {
    run_alone_in_a_fresh_process("child_rediss_connect_never_installs_a_provider");
}

#[test]
#[ignore = "child half of a_rediss_connect_without_an_installed_provider_errors_instead_of_panicking"]
fn child_rediss_connect_never_installs_a_provider() {
    assert_no_provider_installed();
    assert_rediss_connect_errors();
}

/// The startup install (`main` calls it before anything builds a TLS config)
/// succeeds in a process that has nothing installed, leaves aws-lc-rs as the
/// default, is accepted when repeated, and the connect still errors cleanly.
#[test]
fn the_startup_install_succeeds_and_is_idempotent() {
    run_alone_in_a_fresh_process("child_startup_install");
}

#[test]
#[ignore = "child half of the_startup_install_succeeds_and_is_idempotent"]
fn child_startup_install() {
    assert_no_provider_installed();

    assert!(
        limen::http::client::install_crypto_provider(),
        "the first install in a fresh process must be the one that takes"
    );
    let installed = rustls::crypto::CryptoProvider::get_default()
        .expect("the install made a default provider available");
    // aws-lc-rs, not ring: of the two, only aws-lc-rs offers this hybrid group.
    assert!(
        installed
            .kx_groups
            .iter()
            .any(|group| group.name() == rustls::NamedGroup::X25519MLKEM768),
        "the installed default is not aws-lc-rs"
    );
    assert!(
        !limen::http::client::install_crypto_provider(),
        "a repeated install reports that a provider was already there"
    );

    assert_rediss_connect_errors();
}

/// Dial `rediss://127.0.0.1:<port>` where the listener drops every connection
/// on accept, and require an error. `Client::open` does no I/O, so the connect
/// is driven for real: that is where rustls builds its client config.
fn assert_rediss_connect_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            // Accepted and dropped: the TLS handshake gets EOF.
            drop(stream);
        }
    });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let outcome = runtime.block_on(async {
        let client = redis::Client::open(format!("rediss://127.0.0.1:{port}"))
            .expect("Client::open parses the url and does no I/O");
        tokio::time::timeout(
            Duration::from_secs(10),
            client.get_multiplexed_async_connection(),
        )
        .await
    });
    let error = outcome
        .expect("a dropped handshake must fail the connect, not hang it")
        .expect_err("a listener that drops the handshake cannot yield a connection");
    // An I/O-level failure of the handshake (EOF), which is what a hang-up is.
    assert!(
        error.is_io_error(),
        "expected an I/O error from the dropped handshake, got: {error}"
    );
}
