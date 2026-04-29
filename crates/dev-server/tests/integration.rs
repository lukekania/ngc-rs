//! Integration tests for the dev server.
//!
//! Each test binds an ephemeral port, drives requests through a minimal raw
//! HTTP/1.1 client (no extra crate dependency), and asserts the server's
//! behavior end to end.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;

use ngc_dev_server::{DevServer, DevServerConfig, DevServerEvent, LIVE_RELOAD_SCRIPT};
use tempfile::TempDir;

struct Fixture {
    server: DevServer,
    _root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().expect("tempdir");
        write_file(
            root.path(),
            "index.html",
            b"<html><body><h1>hi</h1></body></html>",
        );
        write_file(root.path(), "main.js", b"console.log('hello');");
        write_file(root.path(), "main.js.map", b"{\"version\":3}");
        write_file(root.path(), "styles.css", b"body{color:red}");
        write_file(root.path(), "assets/logo.svg", b"<svg/>");

        let cfg = DevServerConfig::new(root.path()).with_port(0);
        let (_tx, rx) = channel::<DevServerEvent>();
        let server = DevServer::start(cfg, rx).expect("start dev server");
        Self {
            server,
            _root: root,
        }
    }
}

fn write_file(root: &std::path::Path, rel: &str, bytes: &[u8]) {
    let path: PathBuf = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(&path, bytes).expect("write");
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn http_get(addr: std::net::SocketAddr, path: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("write");
    stream.flush().expect("flush");

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("header line");
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.trim_end_matches("\r\n").split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let mut body = Vec::new();
    reader.read_to_end(&mut body).expect("body");
    HttpResponse {
        status,
        headers,
        body,
    }
}

#[test]
fn get_root_returns_index_html_with_injected_client() {
    let fx = Fixture::new();
    let resp = http_get(fx.server.addr(), "/");
    assert_eq!(resp.status, 200);
    let ct = resp.header("Content-Type").expect("content-type");
    assert!(ct.starts_with("text/html"), "got {ct}");
    let body = std::str::from_utf8(&resp.body).expect("utf8 body");
    assert!(
        body.contains(LIVE_RELOAD_SCRIPT),
        "live-reload script not injected: {body}"
    );
    assert!(body.contains("<h1>hi</h1>"));
}

#[test]
fn get_index_html_directly_also_injects_client() {
    let fx = Fixture::new();
    let resp = http_get(fx.server.addr(), "/index.html");
    assert_eq!(resp.status, 200);
    let body = std::str::from_utf8(&resp.body).expect("utf8 body");
    assert!(body.contains(LIVE_RELOAD_SCRIPT));
}

#[test]
fn get_existing_js_file_returns_correct_mime() {
    let fx = Fixture::new();
    let resp = http_get(fx.server.addr(), "/main.js");
    assert_eq!(resp.status, 200);
    let ct = resp.header("Content-Type").expect("content-type");
    assert!(ct.starts_with("application/javascript"), "got {ct}");
    assert_eq!(resp.body, b"console.log('hello');");
}

#[test]
fn get_source_map_returns_json_mime() {
    let fx = Fixture::new();
    let resp = http_get(fx.server.addr(), "/main.js.map");
    assert_eq!(resp.status, 200);
    let ct = resp.header("Content-Type").expect("content-type");
    assert!(ct.starts_with("application/json"), "got {ct}");
    assert_eq!(resp.body, b"{\"version\":3}");
}

#[test]
fn get_css_returns_css_mime() {
    let fx = Fixture::new();
    let resp = http_get(fx.server.addr(), "/styles.css");
    assert_eq!(resp.status, 200);
    let ct = resp.header("Content-Type").expect("content-type");
    assert!(ct.starts_with("text/css"), "got {ct}");
}

#[test]
fn get_nested_asset_resolves_under_root() {
    let fx = Fixture::new();
    let resp = http_get(fx.server.addr(), "/assets/logo.svg");
    assert_eq!(resp.status, 200);
    let ct = resp.header("Content-Type").expect("content-type");
    assert_eq!(ct, "image/svg+xml");
    assert_eq!(resp.body, b"<svg/>");
}

#[test]
fn unknown_path_falls_back_to_index_html() {
    let fx = Fixture::new();
    let resp = http_get(fx.server.addr(), "/some/spa/route");
    assert_eq!(resp.status, 200);
    let ct = resp.header("Content-Type").expect("content-type");
    assert!(ct.starts_with("text/html"), "got {ct}");
    let body = std::str::from_utf8(&resp.body).expect("utf8 body");
    assert!(body.contains("<h1>hi</h1>"));
    assert!(body.contains(LIVE_RELOAD_SCRIPT));
}

#[test]
fn path_traversal_is_refused() {
    let fx = Fixture::new();
    let resp = http_get(fx.server.addr(), "/../etc/passwd");
    assert_eq!(resp.status, 403);
}

#[test]
fn trigger_reload_pushes_event_to_sse_client() {
    let fx = Fixture::new();
    let mut stream = TcpStream::connect(fx.server.addr()).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let req = "GET /__ngc_reload HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n";
    stream.write_all(req.as_bytes()).expect("write");
    stream.flush().expect("flush");

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("status line");
    assert!(status_line.contains("200"), "status was {status_line}");

    let mut saw_event_stream = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("header");
        if n == 0 {
            break;
        }
        if line.to_ascii_lowercase().contains("text/event-stream") {
            saw_event_stream = true;
        }
        if line == "\r\n" {
            break;
        }
    }
    assert!(saw_event_stream, "missing event-stream content type");

    let mut got_connected = String::new();
    reader.read_line(&mut got_connected).expect("connected");
    assert!(
        got_connected.starts_with(": connected"),
        "got {got_connected:?}"
    );
    let mut blank = String::new();
    reader.read_line(&mut blank).expect("blank");

    std::thread::sleep(Duration::from_millis(100));
    fx.server.trigger_reload().expect("trigger reload");

    let mut event = String::new();
    reader.read_line(&mut event).expect("event line");
    assert_eq!(event, "event: reload\n");
    let mut data = String::new();
    reader.read_line(&mut data).expect("data line");
    assert_eq!(data, "data: rebuild\n");
}

#[test]
fn build_failed_event_is_fanned_out_as_named_sse_frame() {
    let fx = Fixture::new();
    let mut stream = TcpStream::connect(fx.server.addr()).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let req = "GET /__ngc_reload HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n";
    stream.write_all(req.as_bytes()).expect("write");
    stream.flush().expect("flush");

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("status line");
    assert!(status_line.contains("200"), "status was {status_line}");

    // Drain headers up to and including the blank separator line.
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("header");
        if n == 0 || line == "\r\n" {
            break;
        }
    }

    // Drain the initial `: connected` SSE comment + blank line.
    let mut connected = String::new();
    reader.read_line(&mut connected).expect("connected");
    assert!(connected.starts_with(": connected"), "got {connected:?}");
    let mut blank = String::new();
    reader.read_line(&mut blank).expect("blank");

    std::thread::sleep(Duration::from_millis(100));
    fx.server
        .send_event(ngc_dev_server::DevServerEvent::BuildFailed {
            message: "syntax error: unexpected }".to_string(),
            file: Some(PathBuf::from("/tmp/proj/src/app.ts")),
            line: Some(7),
            column: Some(2),
        })
        .expect("send build-failed");

    let mut event_line = String::new();
    reader.read_line(&mut event_line).expect("event line");
    assert_eq!(event_line, "event: build-failed\n");

    let mut data_line = String::new();
    reader.read_line(&mut data_line).expect("data line");
    let data = data_line
        .trim_end_matches('\n')
        .strip_prefix("data: ")
        .expect("data: prefix");
    let parsed: serde_json::Value = serde_json::from_str(data).expect("json data payload");
    assert_eq!(parsed["message"], "syntax error: unexpected }");
    assert_eq!(parsed["file"], "/tmp/proj/src/app.ts");
    assert_eq!(parsed["line"], 7);
    assert_eq!(parsed["column"], 2);
}

#[test]
fn build_failed_followed_by_reload_clears_overlay_path() {
    let fx = Fixture::new();
    let mut stream = TcpStream::connect(fx.server.addr()).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let req = "GET /__ngc_reload HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n";
    stream.write_all(req.as_bytes()).expect("write");
    stream.flush().expect("flush");

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("status line");
    assert!(status_line.contains("200"));

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("header");
        if n == 0 || line == "\r\n" {
            break;
        }
    }
    let mut connected = String::new();
    reader.read_line(&mut connected).expect("connected");
    let mut blank = String::new();
    reader.read_line(&mut blank).expect("blank");

    std::thread::sleep(Duration::from_millis(100));
    fx.server
        .send_event(ngc_dev_server::DevServerEvent::BuildFailed {
            message: "boom".to_string(),
            file: None,
            line: None,
            column: None,
        })
        .expect("send build-failed");

    let mut ev1 = String::new();
    reader.read_line(&mut ev1).expect("ev1");
    assert_eq!(ev1, "event: build-failed\n");
    let mut data1 = String::new();
    reader.read_line(&mut data1).expect("data1");
    let mut sep1 = String::new();
    reader.read_line(&mut sep1).expect("sep1");

    fx.server.trigger_reload().expect("trigger reload");

    let mut ev2 = String::new();
    reader.read_line(&mut ev2).expect("ev2");
    assert_eq!(ev2, "event: reload\n");
    let mut data2 = String::new();
    reader.read_line(&mut data2).expect("data2");
    assert_eq!(data2, "data: rebuild\n");
}

#[test]
fn injected_overlay_client_listens_for_build_failed_event() {
    let fx = Fixture::new();
    let resp = http_get(fx.server.addr(), "/");
    assert_eq!(resp.status, 200);
    let body = std::str::from_utf8(&resp.body).expect("utf8 body");
    assert!(
        body.contains("addEventListener('build-failed'"),
        "overlay listener not injected: {body}"
    );
    assert!(
        body.contains("addEventListener('reload'"),
        "reload listener missing: {body}"
    );
    assert!(
        body.contains("__ngcRsOverlay"),
        "overlay window handle missing: {body}"
    );
}

#[test]
fn unknown_extension_serves_octet_stream() {
    let root = TempDir::new().expect("tempdir");
    write_file(root.path(), "index.html", b"<html><body></body></html>");
    write_file(root.path(), "blob.weird", b"\x00\x01\x02blob");
    let cfg = DevServerConfig::new(root.path()).with_port(0);
    let (_tx, rx) = channel::<DevServerEvent>();
    let server = DevServer::start(cfg, rx).expect("start");
    let resp = http_get(server.addr(), "/blob.weird");
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("Content-Type").unwrap_or_default(),
        "application/octet-stream"
    );
}
