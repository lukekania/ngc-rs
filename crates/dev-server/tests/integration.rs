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

fn prefixed_fixture(serve_path: &str) -> Fixture {
    let root = TempDir::new().expect("tempdir");
    write_file(
        root.path(),
        "index.html",
        b"<html><body><h1>hi</h1></body></html>",
    );
    write_file(root.path(), "main.js", b"console.log('hello');");
    let cfg = DevServerConfig::new(root.path())
        .with_port(0)
        .with_serve_path(Some(serve_path));
    let (_tx, rx) = channel::<DevServerEvent>();
    let server = DevServer::start(cfg, rx).expect("start dev server");
    Fixture {
        server,
        _root: root,
    }
}

#[test]
fn prefixed_root_serves_index_html() {
    let fx = prefixed_fixture("/admin/");
    let resp = http_get(fx.server.addr(), "/admin/");
    assert_eq!(resp.status, 200);
    let body = std::str::from_utf8(&resp.body).expect("utf8 body");
    assert!(body.contains("<h1>hi</h1>"));
    // EventSource URL must point to the prefixed SSE channel.
    assert!(body.contains("'/admin/__ngc_reload'"));
}

#[test]
fn prefixed_static_asset_strips_prefix_for_filesystem_lookup() {
    let fx = prefixed_fixture("/admin/");
    let resp = http_get(fx.server.addr(), "/admin/main.js");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"console.log('hello');");
}

#[test]
fn prefixed_deep_link_falls_back_to_index_html() {
    let fx = prefixed_fixture("/admin/");
    let resp = http_get(fx.server.addr(), "/admin/users/42");
    assert_eq!(resp.status, 200);
    let body = std::str::from_utf8(&resp.body).expect("utf8 body");
    assert!(body.contains("<h1>hi</h1>"));
}

#[test]
fn unprefixed_request_returns_404_when_serve_path_set() {
    let fx = prefixed_fixture("/admin/");
    assert_eq!(http_get(fx.server.addr(), "/").status, 404);
    assert_eq!(http_get(fx.server.addr(), "/main.js").status, 404);
    assert_eq!(http_get(fx.server.addr(), "/__ngc_reload").status, 404);
}

fn http_get_with_host(addr: std::net::SocketAddr, path: &str, host_header: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n");
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

fn allowed_hosts_fixture(patterns: &[&str]) -> Fixture {
    let root = TempDir::new().expect("tempdir");
    write_file(
        root.path(),
        "index.html",
        b"<html><body><h1>hi</h1></body></html>",
    );
    let cfg = DevServerConfig::new(root.path())
        .with_port(0)
        .with_allowed_hosts(patterns.iter().copied());
    let (_tx, rx) = channel::<DevServerEvent>();
    let server = DevServer::start(cfg, rx).expect("start dev server");
    Fixture {
        server,
        _root: root,
    }
}

#[test]
fn default_allowed_hosts_accept_loopback_and_403_others() {
    let fx = allowed_hosts_fixture(&[]);
    assert_eq!(
        http_get_with_host(fx.server.addr(), "/", "localhost").status,
        200
    );
    assert_eq!(
        http_get_with_host(fx.server.addr(), "/", "127.0.0.1").status,
        200
    );
    assert_eq!(
        http_get_with_host(fx.server.addr(), "/", "[::1]").status,
        200
    );
    let blocked = http_get_with_host(fx.server.addr(), "/", "my-app.ngrok.io");
    assert_eq!(blocked.status, 403);
    let body = std::str::from_utf8(&blocked.body).unwrap_or("");
    assert!(
        body.contains("my-app.ngrok.io") && body.contains("allowedHosts"),
        "403 body should name the host and point at allowedHosts: {body}"
    );
}

#[test]
fn explicit_allowed_host_lets_ngrok_traffic_through() {
    let fx = allowed_hosts_fixture(&["my-app.ngrok.io"]);
    assert_eq!(
        http_get_with_host(fx.server.addr(), "/", "my-app.ngrok.io").status,
        200
    );
    // Port stripping: a tunneling proxy may forward Host with a port.
    assert_eq!(
        http_get_with_host(fx.server.addr(), "/", "my-app.ngrok.io:8443").status,
        200
    );
    // Loopback still works.
    assert_eq!(
        http_get_with_host(fx.server.addr(), "/", "localhost").status,
        200
    );
    // Anything else is still blocked.
    assert_eq!(
        http_get_with_host(fx.server.addr(), "/", "other.ngrok.io").status,
        403
    );
}

#[test]
fn allowed_hosts_all_disables_check() {
    let fx = allowed_hosts_fixture(&["all"]);
    assert_eq!(
        http_get_with_host(fx.server.addr(), "/", "anything.example.com").status,
        200
    );
    assert_eq!(
        http_get_with_host(fx.server.addr(), "/", "my-app.ngrok.io").status,
        200
    );
}

#[test]
fn prefixed_sse_channel_is_reachable_under_prefix() {
    let fx = prefixed_fixture("/admin/");
    let mut stream = TcpStream::connect(fx.server.addr()).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let req =
        "GET /admin/__ngc_reload HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n";
    stream.write_all(req.as_bytes()).expect("write");
    stream.flush().expect("flush");

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("status line");
    assert!(status_line.contains("200"), "got {status_line}");

    let mut saw_event_stream = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("header");
        if n == 0 || line == "\r\n" {
            break;
        }
        if line.to_ascii_lowercase().contains("text/event-stream") {
            saw_event_stream = true;
        }
    }
    assert!(saw_event_stream);
}

/// Build a fixture whose dev server is configured with the given custom
/// response `headers`.
fn headers_fixture(headers: &[(&str, &str)]) -> Fixture {
    let root = TempDir::new().expect("tempdir");
    write_file(
        root.path(),
        "index.html",
        b"<html><body><h1>hi</h1></body></html>",
    );
    write_file(root.path(), "main.js", b"console.log('hello');");

    let cfg = DevServerConfig::new(root.path())
        .with_port(0)
        .with_headers(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let (_tx, rx) = channel::<DevServerEvent>();
    let server = DevServer::start(cfg, rx).expect("start dev server");
    Fixture {
        server,
        _root: root,
    }
}

#[test]
fn custom_headers_are_emitted_on_static_assets() {
    let fx = headers_fixture(&[("Cross-Origin-Opener-Policy", "same-origin")]);
    let resp = http_get(fx.server.addr(), "/main.js");
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("Cross-Origin-Opener-Policy"),
        Some("same-origin")
    );
}

#[test]
fn custom_headers_are_emitted_on_index_html() {
    let fx = headers_fixture(&[("Cross-Origin-Opener-Policy", "same-origin")]);
    let resp = http_get(fx.server.addr(), "/");
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("Cross-Origin-Opener-Policy"),
        Some("same-origin")
    );
}

#[test]
fn custom_headers_are_emitted_on_spa_fallback() {
    let fx = headers_fixture(&[("X-Frame-Options", "DENY")]);
    // A deep client-side route resolves to no file and falls back to
    // index.html — the custom headers must ride along.
    let resp = http_get(fx.server.addr(), "/users/42/profile");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("X-Frame-Options"), Some("DENY"));
}

#[test]
fn multiple_custom_headers_are_all_emitted() {
    let fx = headers_fixture(&[
        ("X-Frame-Options", "DENY"),
        ("X-Content-Type-Options", "nosniff"),
    ]);
    let resp = http_get(fx.server.addr(), "/main.js");
    assert_eq!(resp.header("X-Frame-Options"), Some("DENY"));
    assert_eq!(resp.header("X-Content-Type-Options"), Some("nosniff"));
}

#[test]
fn custom_content_type_header_does_not_clobber_the_real_one() {
    // A user `Content-Type` entry must never override the MIME type the
    // server picked for the served file.
    let fx = headers_fixture(&[("Content-Type", "text/plain")]);
    let resp = http_get(fx.server.addr(), "/main.js");
    assert_eq!(resp.status, 200);
    let ct = resp.header("Content-Type").expect("content-type");
    assert!(
        ct.starts_with("application/javascript"),
        "user Content-Type clobbered the server's: {ct}"
    );
}

#[test]
fn custom_cache_control_header_does_not_clobber_the_dev_server_one() {
    // Live reload depends on responses not being cached; a user
    // `Cache-Control` entry must not override the dev server's `no-cache`.
    let fx = headers_fixture(&[("Cache-Control", "max-age=31536000")]);
    let resp = http_get(fx.server.addr(), "/main.js");
    assert_eq!(resp.header("Cache-Control"), Some("no-cache"));
}

#[test]
fn no_custom_headers_keeps_responses_unchanged() {
    let fx = headers_fixture(&[]);
    let resp = http_get(fx.server.addr(), "/main.js");
    assert_eq!(resp.status, 200);
    assert!(resp.header("Cross-Origin-Opener-Policy").is_none());
}

#[test]
fn custom_headers_are_emitted_on_the_sse_stream() {
    let fx = headers_fixture(&[("Cross-Origin-Opener-Policy", "same-origin")]);
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
    assert!(status_line.contains("200"), "got {status_line}");

    let mut saw_header = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("header");
        if n == 0 || line == "\r\n" {
            break;
        }
        if line
            .to_ascii_lowercase()
            .starts_with("cross-origin-opener-policy:")
        {
            assert!(line.to_ascii_lowercase().contains("same-origin"));
            saw_header = true;
        }
    }
    assert!(saw_header, "custom header missing from SSE response head");
}

// ----------------------------------------------------------------------------
// HTTPS / TLS (#142)
//
// These tests stand up a dev server with a throwaway self-signed certificate
// and drive it through a rustls client that skips certificate verification —
// the equivalent of clicking through the browser's untrusted-certificate
// warning. They confirm both ordinary static serving and the long-lived SSE
// live-reload stream work once the connection is wrapped in TLS.
// ----------------------------------------------------------------------------

mod tls {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::sync::mpsc::channel;
    use std::sync::Arc;
    use std::time::Duration;

    use ngc_dev_server::{
        DevServer, DevServerConfig, DevServerEvent, TlsConfig, LIVE_RELOAD_SCRIPT,
    };
    use rustls::{ClientConfig, ClientConnection, StreamOwned};
    use tempfile::TempDir;

    struct TlsFixture {
        server: DevServer,
        _root: TempDir,
    }

    impl TlsFixture {
        fn new() -> Self {
            let root = TempDir::new().expect("tempdir");
            std::fs::write(
                root.path().join("index.html"),
                b"<html><body><h1>secure</h1></body></html>",
            )
            .expect("write index");
            let tls = TlsConfig::self_signed(&["127.0.0.1".to_string()]).expect("self-signed");
            let cfg = DevServerConfig::new(root.path())
                .with_port(0)
                .with_tls(Some(tls));
            let (_tx, rx) = channel::<DevServerEvent>();
            let server = DevServer::start(cfg, rx).expect("start tls dev server");
            Self {
                server,
                _root: root,
            }
        }
    }

    // A certificate verifier that accepts everything — the test cert is
    // self-signed and not in any trust store, which is exactly the dev
    // workflow this feature targets.
    struct NoVerify;

    impl rustls::client::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::Certificate,
            _intermediates: &[rustls::Certificate],
            _server_name: &rustls::ServerName,
            _scts: &mut dyn Iterator<Item = &[u8]>,
            _ocsp_response: &[u8],
            _now: std::time::SystemTime,
        ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::ServerCertVerified::assertion())
        }
    }

    fn tls_stream(addr: std::net::SocketAddr) -> StreamOwned<ClientConnection, TcpStream> {
        let config = ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        let server_name = rustls::ServerName::try_from("localhost").expect("server name");
        let conn = ClientConnection::new(Arc::new(config), server_name).expect("client conn");
        let sock = TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        StreamOwned::new(conn, sock)
    }

    #[test]
    fn serves_index_over_https_with_injected_live_reload_script() {
        let fx = TlsFixture::new();
        let mut stream = tls_stream(fx.server.addr());
        let req = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        stream.write_all(req.as_bytes()).expect("write");
        stream.flush().expect("flush");

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read response");
        let text = String::from_utf8_lossy(&raw);

        let status_line = text.lines().next().expect("status line");
        assert!(status_line.contains("200"), "status was {status_line}");
        // The SPA index is served and the live-reload client is injected,
        // proving TLS framing of an ordinary file response works.
        assert!(text.contains("<h1>secure</h1>"), "body missing app markup");
        assert!(
            text.contains(LIVE_RELOAD_SCRIPT),
            "live-reload script not injected over https"
        );
    }

    #[test]
    fn scheme_reports_https_when_tls_enabled() {
        let fx = TlsFixture::new();
        assert_eq!(fx.server.scheme(), "https");
    }

    #[test]
    fn sse_live_reload_stream_works_over_https() {
        let fx = TlsFixture::new();
        let stream = tls_stream(fx.server.addr());
        let mut writer = stream;
        let req =
            "GET /__ngc_reload HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n";
        writer.write_all(req.as_bytes()).expect("write");
        writer.flush().expect("flush");

        let mut reader = BufReader::new(writer);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).expect("status line");
        assert!(status_line.contains("200"), "status was {status_line}");

        let mut saw_event_stream = false;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).expect("header");
            if n == 0 || line == "\r\n" {
                break;
            }
            if line.to_ascii_lowercase().contains("text/event-stream") {
                saw_event_stream = true;
            }
        }
        assert!(
            saw_event_stream,
            "missing event-stream content type over tls"
        );

        let mut connected = String::new();
        reader.read_line(&mut connected).expect("connected");
        assert!(connected.starts_with(": connected"), "got {connected:?}");
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
    fn serves_over_https_with_explicit_cert_and_key() {
        // Mint a cert/key pair and feed the raw PEM through `from_pem` — the
        // path explicit sslKey/sslCert files take — then confirm the server
        // comes up and serves over TLS.
        let ck = rcgen_pair();
        let root = TempDir::new().expect("tempdir");
        std::fs::write(
            root.path().join("index.html"),
            b"<html><body>ok</body></html>",
        )
        .expect("write index");
        let cfg = DevServerConfig::new(root.path())
            .with_port(0)
            .with_tls(Some(TlsConfig::from_pem(ck.0, ck.1)));
        let (_tx, rx) = channel::<DevServerEvent>();
        let server = DevServer::start(cfg, rx).expect("start with explicit pem");

        let mut stream = tls_stream(server.addr());
        let req = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        stream.write_all(req.as_bytes()).expect("write");
        stream.flush().expect("flush");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read");
        let text = String::from_utf8_lossy(&raw);
        assert!(text.lines().next().unwrap_or("").contains("200"));
    }

    // Generate a (cert_pem, key_pem) pair the same way the production
    // self-signed path does, but expose the raw PEM so the test can feed it
    // through `TlsConfig::from_pem`.
    fn rcgen_pair() -> (Vec<u8>, Vec<u8>) {
        let ck = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("rcgen");
        (
            ck.cert.pem().into_bytes(),
            ck.signing_key.serialize_pem().into_bytes(),
        )
    }
}
