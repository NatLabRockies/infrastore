//! Tests that run the `infrastore-server` **binary**.
//!
//! Nothing else executes it: the integration tests construct
//! `CatalogStoreService` in-process and attach the auth interceptor by hand, so
//! `src/bin/server.rs` — argument parsing, config loading, the
//! `auth = "none"` / `"api_key"` dispatch, and the failure exits — was never
//! run. A misconfiguration that crashed on startup, or an `api_key` branch that
//! forgot the interceptor, would not have failed any test.
//!
//! Each test writes a TOML config to a temp dir, spawns the binary, waits for
//! the port to accept a connection, drives one RPC, and kills the child.

use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    Features, OwnerCategory, SingleTimeSeries, TimeSeriesData, TypedArray, create_store,
};
use infrastore_proto::pb::{CountsReq, catalog_store_client::CatalogStoreClient};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

const BIN: &str = env!("CARGO_BIN_EXE_infrastore-server");

/// Kill the child on drop so a failing assertion never leaks a server process.
struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Build a small on-disk store and return its HDF5 path.
fn write_store(dir: &Path) -> PathBuf {
    let path = dir.join("store.h5");
    let mut store = create_store(Some(path.as_path()), false).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let data = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial,
                Duration::hours(1),
                data,
                "load",
            )),
            Features::new(),
        )
        .unwrap();
    store.flush().unwrap();
    path
}

/// Serializes the window between picking a port and the server owning it, so
/// that two concurrent tests cannot be handed the same one. See `spawn_server`.
static PORT_HANDOFF: Mutex<()> = Mutex::new(());

/// A port that is free right now. The binary calls `serve(addr)` with the
/// configured port and does not log the port it actually bound, so a port must
/// be chosen up front; binding and immediately dropping a listener is the usual
/// way. Callers must hold `PORT_HANDOFF` until their server has bound it.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn write_config(dir: &Path, store: &Path, port: u16, auth: &str, keys: &[&str]) -> PathBuf {
    let path = dir.join("server.toml");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "[server]").unwrap();
    writeln!(f, "host = \"127.0.0.1\"").unwrap();
    writeln!(f, "port = {port}").unwrap();
    writeln!(f).unwrap();
    writeln!(f, "[data]").unwrap();
    // TOML strings need escaping on Windows paths; `to_string_lossy` plus
    // `escape_debug` keeps backslashes intact without hard-coding a separator.
    writeln!(
        f,
        "files = [\"{}\"]",
        store.to_string_lossy().escape_debug()
    )
    .unwrap();
    writeln!(f).unwrap();
    writeln!(f, "[authentication]").unwrap();
    writeln!(f, "method = \"{auth}\"").unwrap();
    if !keys.is_empty() {
        let list: Vec<String> = keys.iter().map(|k| format!("\"{k}\"")).collect();
        writeln!(f, "keys = [{}]", list.join(", ")).unwrap();
    }
    path
}

fn spawn_binary(config: &Path) -> Child {
    Command::new(BIN)
        .arg("--config")
        .arg(config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the server binary")
}

/// Wait until `port` accepts a TCP connection, or the child exits.
///
/// A socket answering is not on its own proof that the child owns the port —
/// something else may have taken it while the child died binding — so the
/// child must still be running for the connection to count.
fn wait_until_listening(child: &mut Child, port: u16) -> bool {
    let deadline = Instant::now() + StdDuration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(Some(_status)) = child.try_wait() {
            return false; // exited before listening
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return matches!(child.try_wait(), Ok(None));
        }
        std::thread::sleep(StdDuration::from_millis(50));
    }
    false
}

/// Spawn the binary on a fresh port, retrying if the port was taken in the race
/// between `free_port` and the bind. Returns `(process, port)`.
///
/// `PORT_HANDOFF` is held across the whole attempt: `free_port` drops its
/// listener before the child binds, so without it two tests running in
/// parallel can be handed the same port. The loser then exits with an
/// address-in-use error while the winner's socket answers on that port,
/// which used to read as a successful start and only surfaced later, as a
/// connection refused once the winner shut down.
fn spawn_server(dir: &Path, store: &Path, auth: &str, keys: &[&str]) -> (ServerProcess, u16) {
    for attempt in 0..5 {
        let bound = PORT_HANDOFF.lock().unwrap_or_else(|e| e.into_inner());
        let port = free_port();
        let config = write_config(dir, store, port, auth, keys);
        let mut child = spawn_binary(&config);
        let listening = wait_until_listening(&mut child, port);
        drop(bound);
        if listening {
            return (ServerProcess(child), port);
        }
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        if attempt == 4 {
            panic!(
                "the server never listened on {port}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }
    unreachable!()
}

async fn channel(port: u16) -> Channel {
    Channel::from_shared(format!("http://127.0.0.1:{port}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Startup failures
// ---------------------------------------------------------------------------

#[test]
fn a_nonexistent_store_file_exits_nonzero_with_a_useful_message() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does_not_exist.h5");
    let config = write_config(dir.path(), &missing, free_port(), "none", &[]);

    let output = Command::new(BIN)
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "the binary must not start with an unreadable store"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.trim().is_empty(),
        "a startup failure must say something on stderr; stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn a_missing_config_file_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(BIN)
        .arg("--config")
        .arg(dir.path().join("no_such_config.toml"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).trim().is_empty());
}

#[test]
fn an_empty_data_files_list_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("server.toml");
    std::fs::write(
        &config,
        "[server]\nhost = \"127.0.0.1\"\nport = 50999\n\n\
         [data]\nfiles = []\n\n[authentication]\nmethod = \"none\"\n",
    )
    .unwrap();

    let output = Command::new(BIN)
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("files") || stderr.contains("config error"),
        "expected a diagnostic naming the empty file list, got: {stderr}"
    );
}

#[test]
fn api_key_auth_with_no_keys_exits_nonzero() {
    // `AuthSection::validate` rejects `api_key` with an empty key list; that
    // check runs in the binary and nowhere else.
    let dir = tempfile::tempdir().unwrap();
    let store = write_store(dir.path());
    let config = write_config(dir.path(), &store, free_port(), "api_key", &[]);

    let output = Command::new(BIN)
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "api_key with no keys must be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("config error") || stderr.contains("key"),
        "expected a diagnostic about the missing keys, got: {stderr}"
    );
}

#[test]
fn an_unknown_auth_method_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let store = write_store(dir.path());
    let config = write_config(dir.path(), &store, free_port(), "oauth2", &[]);

    let output = Command::new(BIN)
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "an unknown auth method must be rejected before serving, not hit the \
         unreachable!() in the dispatch"
    );
}

#[test]
fn a_missing_config_argument_exits_nonzero() {
    let output = Command::new(BIN).output().unwrap();
    assert!(!output.status.success(), "--config is required");
    // clap writes its own usage message.
    assert!(!String::from_utf8_lossy(&output.stderr).trim().is_empty());
}

// ---------------------------------------------------------------------------
// The binary actually serves
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn auth_none_serves_without_a_header() {
    // The `"none"` dispatch branch in `main` is only reachable by running the
    // binary: the in-process tests either attach no interceptor or attach one
    // directly.
    let dir = tempfile::tempdir().unwrap();
    let store = write_store(dir.path());
    let (_proc, port) = spawn_server(dir.path(), &store, "none", &[]);

    let mut client = CatalogStoreClient::new(channel(port).await);
    let resp = client.get_counts(CountsReq {}).await.unwrap().into_inner();
    assert_eq!(resp.static_time_series, 1);
    assert_eq!(resp.components_with_time_series, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_none_ignores_a_supplied_api_key_header() {
    // With auth off there is no interceptor, so an unexpected header is simply
    // not looked at rather than rejected.
    let dir = tempfile::tempdir().unwrap();
    let store = write_store(dir.path());
    let (_proc, port) = spawn_server(dir.path(), &store, "none", &[]);

    let key: MetadataValue<_> = "irrelevant".parse().unwrap();
    let mut client = CatalogStoreClient::with_interceptor(
        channel(port).await,
        move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("x-api-key", key.clone());
            Ok(req)
        },
    );
    let resp = client.get_counts(CountsReq {}).await.unwrap().into_inner();
    assert_eq!(resp.static_time_series, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn api_key_auth_through_the_binary_accepts_and_rejects() {
    let dir = tempfile::tempdir().unwrap();
    let store = write_store(dir.path());
    let (_proc, port) = spawn_server(dir.path(), &store, "api_key", &["s3cret", "other"]);

    // No header at all.
    let mut anon = CatalogStoreClient::new(channel(port).await);
    let err = anon.get_counts(CountsReq {}).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");

    // A wrong key.
    let bad: MetadataValue<_> = "nope".parse().unwrap();
    let mut wrong = CatalogStoreClient::with_interceptor(
        channel(port).await,
        move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("x-api-key", bad.clone());
            Ok(req)
        },
    );
    let err = wrong.get_counts(CountsReq {}).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");

    // Either configured key works, and the data comes back.
    for key_text in ["s3cret", "other"] {
        let key: MetadataValue<_> = key_text.parse().unwrap();
        let mut client = CatalogStoreClient::with_interceptor(
            channel(port).await,
            move |mut req: tonic::Request<()>| {
                req.metadata_mut().insert("x-api-key", key.clone());
                Ok(req)
            },
        );
        let resp = client.get_counts(CountsReq {}).await.unwrap().into_inner();
        assert_eq!(resp.static_time_series, 1, "key {key_text}");
    }
}
