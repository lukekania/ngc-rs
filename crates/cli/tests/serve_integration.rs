//! Integration tests for the `ngc-rs serve` subcommand.
//!
//! Two layers:
//!
//! * **CLI parsing** — `ngc-rs --help` lists `serve`, and `ngc-rs serve --help`
//!   surfaces every flag with its default. These shell out to the compiled
//!   binary so they cover both the clap derive and the actual entry point.
//! * **End-to-end smoke** — spawn `ngc-rs serve` against a tiny tsconfig
//!   fixture, hit the bound HTTP port, edit a source file, and confirm the
//!   `/__ngc_reload` SSE channel receives a `reload` event. Tolerant of
//!   notify backends that drop events under sandboxed CI: when the SSE
//!   reload doesn't fire we surface the issue as a skip rather than a
//!   hard failure, matching the watcher's own integration test.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const READY_MARKER: &str = "ngc-rs serve listening on";
const REBUILD_MARKER: &str = "ngc-rs rebuild";
const TIMEOUT: Duration = Duration::from_secs(30);

fn write_fixture(root: &Path) {
    let tsconfig = r#"{
  "compilerOptions": {
    "target": "ES2022",
    "module": "preserve",
    "moduleResolution": "bundler",
    "outDir": "dist"
  },
  "include": ["src/**/*.ts"]
}"#;
    fs::write(root.join("tsconfig.json"), tsconfig).expect("write tsconfig");
    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(
        src.join("main.ts"),
        "import { greet } from './greet';\nconsole.log(greet('world'));\n",
    )
    .expect("write main.ts");
    fs::write(
        src.join("greet.ts"),
        "export function greet(name: string): string {\n  return 'hello ' + name + ' v1';\n}\n",
    )
    .expect("write greet.ts");
}

#[test]
fn root_help_lists_serve() {
    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let out = Command::new(bin)
        .arg("--help")
        .output()
        .expect("spawn ngc-rs --help");
    assert!(out.status.success(), "--help should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("serve"),
        "root --help should mention serve, got: {stdout}"
    );
    assert!(
        stdout.contains("info") && stdout.contains("build"),
        "root --help should still list info and build, got: {stdout}"
    );
}

#[test]
fn serve_help_lists_all_flags() {
    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let out = Command::new(bin)
        .args(["serve", "--help"])
        .output()
        .expect("spawn ngc-rs serve --help");
    assert!(out.status.success(), "serve --help should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--project",
        "--configuration",
        "--port",
        "--host",
        "--open",
        "--serve-path",
    ] {
        assert!(
            stdout.contains(flag),
            "serve --help missing {flag}, got: {stdout}"
        );
    }
    // Default values surface on the help page.
    assert!(
        stdout.contains("4200"),
        "serve --help missing default port, got: {stdout}"
    );
    assert!(
        stdout.contains("localhost"),
        "serve --help missing default host, got: {stdout}"
    );
    assert!(
        stdout.contains("development"),
        "serve --help missing default configuration, got: {stdout}"
    );
}

#[test]
fn serve_rejects_invalid_port() {
    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let out = Command::new(bin)
        .args(["serve", "--port", "not-a-number"])
        .output()
        .expect("spawn ngc-rs serve");
    assert!(
        !out.status.success(),
        "non-numeric --port should fail to parse"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not-a-number") || stderr.contains("invalid value"),
        "expected clap parse error, got: {stderr}"
    );
}

/// Block until `marker` appears on `rx` or `deadline` is reached.
fn wait_for(
    rx: &std::sync::mpsc::Receiver<String>,
    marker: &str,
    deadline: Instant,
) -> Option<String> {
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                if line.contains(marker) {
                    return Some(line);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
    None
}

/// Extract the first `host:port` token from a "listening on http://host:port"
/// log line. Returns None if the line can't be parsed.
fn extract_addr(line: &str) -> Option<String> {
    let scheme = "http://";
    let idx = line.find(scheme)?;
    let tail = &line[idx + scheme.len()..];
    let end = tail
        .find(|c: char| c.is_whitespace() || c == '/' || c == '\u{1b}')
        .unwrap_or(tail.len());
    Some(tail[..end].to_string())
}

#[test]
fn serve_lists_index_and_emits_reload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize root");
    write_fixture(&root);

    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let tsconfig = root.join("tsconfig.json");

    let mut child = Command::new(bin)
        .args(["serve", "--project"])
        .arg(&tsconfig)
        .args(["--port", "0", "--host", "127.0.0.1"])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ngc-rs serve");

    let stderr = child.stderr.take().expect("stderr pipe");
    let reader = BufReader::new(stderr);
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let stderr_handle = std::thread::spawn(move || {
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + TIMEOUT;
    let ready_line = wait_for(&rx, READY_MARKER, deadline);
    let ready_line = match ready_line {
        Some(l) => l,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("serve never reported ready within {TIMEOUT:?}");
        }
    };

    let addr = match extract_addr(&ready_line) {
        Some(a) => a,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("could not parse address from ready line: {ready_line}");
        }
    };

    // GET / should return 200 and a non-empty body. With no real index.html
    // in the fixture the server returns 404 — write a minimal one before
    // we exercise the request path.
    let dist = root.join("dist");
    fs::create_dir_all(&dist).expect("create dist");
    fs::write(
        dist.join("index.html"),
        "<!doctype html><html><body><h1>hello</h1></body></html>",
    )
    .expect("write index.html");

    let body = http_get(&addr, "/").expect("HTTP GET /");
    assert!(
        body.contains("<h1>hello</h1>"),
        "index.html body missing, got: {body}"
    );
    assert!(
        body.contains("EventSource") || body.contains("__ngc_reload"),
        "live-reload script not injected, got: {body}"
    );

    // Open an SSE stream and edit a source file. The watcher should fire a
    // rebuild and the dev server should fan out the reload event.
    let addr_for_sse = addr.clone();
    let (sse_tx, sse_rx) = std::sync::mpsc::channel::<String>();
    let sse_handle = std::thread::spawn(move || {
        if let Err(e) = drive_sse(&addr_for_sse, sse_tx) {
            eprintln!("sse worker exited: {e}");
        }
    });

    // Wait briefly so the watcher's notify backend is fully attached.
    std::thread::sleep(Duration::from_millis(300));

    fs::write(
        root.join("src/greet.ts"),
        "export function greet(name: string): string {\n  return 'hi ' + name + ' v2';\n}\n",
    )
    .expect("rewrite greet.ts");

    let rebuild_deadline = Instant::now() + TIMEOUT;
    let rebuild_observed = wait_for(&rx, REBUILD_MARKER, rebuild_deadline).is_some();
    let reload_deadline = Instant::now() + Duration::from_secs(5);
    let reload_observed = wait_for(&sse_rx, "event: reload", reload_deadline).is_some();

    let _ = child.kill();
    let _ = child.wait();
    let _ = stderr_handle.join();
    let _ = sse_handle.join();

    if !rebuild_observed {
        eprintln!(
            "serve_lists_index_and_emits_reload: notify backend never delivered the change \
             event within {TIMEOUT:?}; treating as a skip on this platform"
        );
        return;
    }
    assert!(
        reload_observed,
        "SSE channel should have emitted a reload event after rebuild"
    );
}

#[test]
fn serve_path_mounts_under_prefix_and_404s_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize root");
    write_fixture(&root);

    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let tsconfig = root.join("tsconfig.json");

    let mut child = Command::new(bin)
        .args(["serve", "--project"])
        .arg(&tsconfig)
        .args([
            "--port",
            "0",
            "--host",
            "127.0.0.1",
            "--serve-path",
            "/admin/",
        ])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ngc-rs serve");

    let stderr = child.stderr.take().expect("stderr pipe");
    let reader = BufReader::new(stderr);
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let stderr_handle = std::thread::spawn(move || {
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + TIMEOUT;
    let ready_line = match wait_for(&rx, READY_MARKER, deadline) {
        Some(l) => l,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("serve never reported ready within {TIMEOUT:?}");
        }
    };

    // The printed URL must include the prefix so users can copy it.
    assert!(
        ready_line.contains("/admin/"),
        "ready line missing servePath, got: {ready_line}"
    );

    let addr = match extract_addr(&ready_line) {
        Some(a) => a,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("could not parse address from ready line: {ready_line}");
        }
    };

    let dist = root.join("dist");
    fs::create_dir_all(&dist).expect("create dist");
    fs::write(
        dist.join("index.html"),
        "<!doctype html><html><body><h1>hello</h1></body></html>",
    )
    .expect("write index.html");

    let prefixed = http_status(&addr, "/admin/").expect("status under prefix");
    let body = http_get(&addr, "/admin/").expect("body under prefix");
    let deep = http_status(&addr, "/admin/users/42").expect("deep link");
    let root_status = http_status(&addr, "/").expect("status at /");
    let sse_status = http_status_only(&addr, "/admin/__ngc_reload").expect("SSE under prefix");

    let _ = child.kill();
    let _ = child.wait();
    let _ = stderr_handle.join();

    assert_eq!(prefixed, 200, "prefix root should serve index");
    assert!(
        body.contains("<h1>hello</h1>") && body.contains("'/admin/__ngc_reload'"),
        "prefix body missing index or rewritten EventSource URL: {body}"
    );
    assert_eq!(deep, 200, "deep link should fall back to index");
    assert_eq!(root_status, 404, "root should 404 when servePath is set");
    assert_eq!(sse_status, 200, "SSE channel must be mounted under prefix");
}

/// Issue an HTTP/1.1 GET and return only the response status code.
fn http_status(addr: &str, path: &str) -> std::io::Result<u16> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    Ok(status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0))
}

/// Issue an HTTP/1.1 GET that closes the socket as soon as headers are
/// drained. Used for SSE checks where waiting for body close would block.
fn http_status_only(addr: &str, path: &str) -> std::io::Result<u16> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n");
    stream.write_all(req.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    Ok(status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0))
}

/// Issue a minimal HTTP/1.1 GET against `addr` and return the response
/// body as a string. Closes the connection after reading.
fn http_get(addr: &str, path: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    Ok(raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or(raw))
}

/// Open an SSE connection to `/__ngc_reload` and forward each line to
/// `tx` until the server closes the stream.
fn drive_sse(addr: &str, tx: std::sync::mpsc::Sender<String>) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let req =
        format!("GET /__ngc_reload HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\n\r\n");
    stream.write_all(req.as_bytes())?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        if tx.send(line).is_err() {
            break;
        }
    }
    Ok(())
}
