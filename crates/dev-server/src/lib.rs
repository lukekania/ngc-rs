//! HTTP dev server with live reload for ngc-rs.
//!
//! This crate provides a lightweight, synchronous HTTP server that serves a
//! built `dist/` directory and notifies connected browsers when a rebuild has
//! completed. Live reload is delivered over Server-Sent Events (SSE), which is
//! significantly simpler to implement on top of plain blocking HTTP than a
//! full WebSocket handshake — no framing, no upgrade negotiation, just a
//! long-lived `text/event-stream` response.
//!
//! # Design notes
//!
//! * **Sync IO only.** The server uses `tiny_http`, which dispatches requests
//!   on a worker pool of OS threads. No tokio, in keeping with the rayon-only
//!   rule for the rest of the workspace.
//! * **Decoupled from the watcher.** The server consumes a generic
//!   [`std::sync::mpsc::Receiver`] of [`ReloadEvent`]s. The producer side is
//!   left to the caller — the file watcher (issue #24) will become one such
//!   producer, but tests and other tooling can drive reloads with the same
//!   API.
//! * **SPA fallback.** Any unmatched `GET` whose path does not resolve to a
//!   real file under `dist/` returns the contents of `index.html`. This
//!   matches the behavior of `ng serve` for client-side routed apps.
//! * **Live reload injection.** When `index.html` is served, a tiny client
//!   script is injected just before the closing `</body>` tag. It opens an
//!   `EventSource` to `/__ngc_reload` and triggers `location.reload()` when a
//!   `reload` event arrives.

use std::io::Write;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use ngc_diagnostics::{NgcError, NgcResult};
use tiny_http::{Header, Method, Response, Server, StatusCode};

/// A signal that connected browsers should reload the page.
///
/// The struct is intentionally empty for now — future revisions may carry
/// metadata such as the changed file list to enable hot-module replacement.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReloadEvent;

/// Configuration for [`DevServer`].
#[derive(Debug, Clone)]
pub struct DevServerConfig {
    /// Path to the built `dist/` directory that will be served.
    pub root: PathBuf,
    /// Host to bind on. Defaults to `127.0.0.1`.
    pub host: String,
    /// Port to bind on. Defaults to `4200`. A value of `0` will let the OS
    /// pick an ephemeral port — useful for tests.
    pub port: u16,
}

impl DevServerConfig {
    /// Construct a config with default host (`127.0.0.1`) and port (`4200`)
    /// for the given `dist/` directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            host: "127.0.0.1".to_string(),
            port: 4200,
        }
    }

    /// Override the bind host.
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Override the bind port. Use `0` to let the OS pick.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

/// Handle to a running dev server.
///
/// Dropping the handle stops the server and closes any open SSE connections.
pub struct DevServer {
    addr: SocketAddr,
    reload_tx: Sender<ReloadEvent>,
    server: Arc<Server>,
    accept_join: Option<JoinHandle<()>>,
}

impl DevServer {
    /// Bind a new dev server on the configured address and start serving.
    ///
    /// The server runs on a background thread pool managed by `tiny_http`.
    /// Reload events are forwarded from `reload_rx` to all connected SSE
    /// clients. Pass [`ReloadEvent`]s to the matching sender returned by
    /// [`channel`](std::sync::mpsc::channel) elsewhere — typically from a file
    /// watcher.
    pub fn start(config: DevServerConfig, reload_rx: Receiver<ReloadEvent>) -> NgcResult<Self> {
        let bind = (config.host.as_str(), config.port);
        let addr = bind
            .to_socket_addrs()
            .map_err(|e| NgcError::ServeError {
                message: format!(
                    "could not resolve bind address {}:{}: {e}",
                    config.host, config.port
                ),
            })?
            .next()
            .ok_or_else(|| NgcError::ServeError {
                message: format!("no addresses resolved for {}:{}", config.host, config.port),
            })?;

        let listener = TcpListener::bind(addr).map_err(|e| NgcError::ServeError {
            message: format!("could not bind to {addr}: {e}"),
        })?;
        let actual_addr = listener.local_addr().map_err(|e| NgcError::ServeError {
            message: format!("could not read local address: {e}"),
        })?;

        let server = Server::from_listener(listener, None).map_err(|e| NgcError::ServeError {
            message: format!("tiny_http server init failed: {e}"),
        })?;
        let server = Arc::new(server);

        let clients: SseClients = Arc::new(Mutex::new(Vec::new()));
        let (internal_tx, internal_rx) = channel::<ReloadEvent>();

        spawn_bridge(reload_rx, internal_tx.clone())?;
        spawn_fanout(internal_rx, Arc::clone(&clients))?;

        let request_server = Arc::clone(&server);
        let root = config.root.clone();
        let request_clients = Arc::clone(&clients);
        let join = thread::Builder::new()
            .name("ngc-dev-server-accept".into())
            .spawn(move || serve_loop(request_server, root, request_clients))
            .map_err(|e| NgcError::ServeError {
                message: format!("could not spawn accept thread: {e}"),
            })?;

        tracing::info!(addr = %actual_addr, "ngc-rs dev server listening");

        Ok(Self {
            addr: actual_addr,
            reload_tx: internal_tx,
            server,
            accept_join: Some(join),
        })
    }

    /// Address the server is actually bound to. When the configured port was
    /// `0` this reflects the ephemeral port chosen by the OS.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Send a reload event to all connected browsers without going through
    /// an external channel. Convenient for tests and ad-hoc tooling.
    pub fn trigger_reload(&self) -> NgcResult<()> {
        self.reload_tx
            .send(ReloadEvent)
            .map_err(|e| NgcError::ServeError {
                message: format!("could not enqueue reload event: {e}"),
            })
    }
}

impl Drop for DevServer {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(handle) = self.accept_join.take() {
            let _ = handle.join();
        }
    }
}

type SseWriter = Box<dyn Write + Send>;
type SseClients = Arc<Mutex<Vec<SseWriter>>>;

fn spawn_bridge(rx: Receiver<ReloadEvent>, tx: Sender<ReloadEvent>) -> NgcResult<()> {
    thread::Builder::new()
        .name("ngc-dev-server-bridge".into())
        .spawn(move || {
            while let Ok(ev) = rx.recv() {
                if tx.send(ev).is_err() {
                    break;
                }
            }
        })
        .map(|_| ())
        .map_err(|e| NgcError::ServeError {
            message: format!("could not spawn bridge thread: {e}"),
        })
}

fn spawn_fanout(rx: Receiver<ReloadEvent>, clients: SseClients) -> NgcResult<()> {
    thread::Builder::new()
        .name("ngc-dev-server-fanout".into())
        .spawn(move || fanout_loop(rx, clients))
        .map(|_| ())
        .map_err(|e| NgcError::ServeError {
            message: format!("could not spawn fanout thread: {e}"),
        })
}

fn fanout_loop(rx: Receiver<ReloadEvent>, clients: SseClients) {
    while rx.recv().is_ok() {
        let mut guard = match clients.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.retain_mut(|stream| {
            stream
                .write_all(b"event: reload\ndata: rebuild\n\n")
                .and_then(|_| stream.flush())
                .is_ok()
        });
    }
}

fn serve_loop(server: Arc<Server>, root: PathBuf, clients: SseClients) {
    for request in server.incoming_requests() {
        let root = root.clone();
        let clients = Arc::clone(&clients);
        thread::spawn(move || {
            if let Err(e) = handle_request(request, &root, &clients) {
                tracing::warn!(error = %e, "dev server request failed");
            }
        });
    }
}

fn handle_request(
    request: tiny_http::Request,
    root: &Path,
    clients: &SseClients,
) -> NgcResult<()> {
    if !matches!(request.method(), Method::Get | Method::Head) {
        let resp = Response::from_string("method not allowed").with_status_code(StatusCode(405));
        return request.respond(resp).map_err(io_err);
    }

    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("/");

    if path == "/__ngc_reload" {
        return handle_sse(request, clients);
    }

    serve_static(request, root, path)
}

fn handle_sse(request: tiny_http::Request, clients: &SseClients) -> NgcResult<()> {
    let response_head = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache\r\n\
Connection: keep-alive\r\n\
Access-Control-Allow-Origin: *\r\n\
\r\n\
: connected\n\n";

    let mut writer = request.into_writer();
    writer
        .write_all(response_head)
        .and_then(|_| writer.flush())
        .map_err(|e| NgcError::ServeError {
            message: format!("could not start SSE stream: {e}"),
        })?;

    let mut guard = clients.lock().map_err(|_| NgcError::ServeError {
        message: "SSE client list mutex was poisoned".into(),
    })?;
    guard.push(writer);
    Ok(())
}

fn serve_static(request: tiny_http::Request, root: &Path, url_path: &str) -> NgcResult<()> {
    let decoded = decode_path(url_path);
    let candidate = match resolve_under_root(root, &decoded) {
        Some(p) => p,
        None => {
            let resp = Response::from_string("forbidden").with_status_code(StatusCode(403));
            return request.respond(resp).map_err(io_err);
        }
    };

    match pick_file(&candidate) {
        Some(file_path) => respond_with_file(request, &file_path),
        None => spa_fallback(request, root),
    }
}

fn pick_file(candidate: &Path) -> Option<PathBuf> {
    if candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    if candidate.is_dir() {
        let index = candidate.join("index.html");
        if index.is_file() {
            return Some(index);
        }
    }
    None
}

fn spa_fallback(request: tiny_http::Request, root: &Path) -> NgcResult<()> {
    let index = root.join("index.html");
    if index.is_file() {
        respond_with_file(request, &index)
    } else {
        let resp = Response::from_string("not found").with_status_code(StatusCode(404));
        request.respond(resp).map_err(io_err)
    }
}

fn respond_with_file(request: tiny_http::Request, path: &Path) -> NgcResult<()> {
    let bytes = std::fs::read(path).map_err(|e| NgcError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mime = mime_for(path);

    let body = if is_index_html(path) {
        inject_live_reload_client(&bytes)
    } else {
        bytes
    };

    let mut resp = Response::from_data(body);
    resp.add_header(header("Content-Type", mime)?);
    resp.add_header(header("Cache-Control", "no-cache")?);
    request.respond(resp).map_err(io_err)
}

fn is_index_html(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case("index.html"))
        .unwrap_or(false)
}

fn header(name: &str, value: &str) -> NgcResult<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).map_err(|_| NgcError::ServeError {
        message: format!("invalid HTTP header value for {name}: {value}"),
    })
}

fn io_err(e: std::io::Error) -> NgcError {
    NgcError::ServeError {
        message: format!("response write failed: {e}"),
    }
}

fn decode_path(url_path: &str) -> String {
    let mut out = String::with_capacity(url_path.len());
    let bytes = url_path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(((hi << 4) | lo) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn resolve_under_root(root: &Path, url_path: &str) -> Option<PathBuf> {
    let trimmed = url_path.trim_start_matches('/');
    if trimmed.is_empty() {
        return Some(root.to_path_buf());
    }
    let mut joined = root.to_path_buf();
    for segment in trimmed.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return None,
            other => joined.push(other),
        }
    }
    Some(joined)
}

/// MIME type lookup for a given path's extension. Falls back to
/// `application/octet-stream` for unknown extensions.
pub fn mime_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs" | "cjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("map") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("avif") => "image/avif",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("eot") => "application/vnd.ms-fontobject",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The live-reload client script that gets injected into served `index.html`.
///
/// Subscribes to `/__ngc_reload` via `EventSource`, listens for `reload`
/// events, and triggers a full page refresh. Wrapped in a script tag so it
/// can be inserted directly into HTML.
pub const LIVE_RELOAD_SCRIPT: &str = r#"<script>(function(){try{var s=new EventSource('/__ngc_reload');s.addEventListener('reload',function(){location.reload();});}catch(e){console.warn('[ngc-rs] live reload unavailable',e);}})();</script>"#;

/// Insert the live-reload client script into an HTML byte buffer.
///
/// Inserts immediately before the closing `</body>` tag (case-insensitive).
/// If no `</body>` is present, appends to the end.
pub fn inject_live_reload_client(html: &[u8]) -> Vec<u8> {
    let s = match std::str::from_utf8(html) {
        Ok(s) => s,
        Err(_) => {
            let mut out = Vec::with_capacity(html.len() + LIVE_RELOAD_SCRIPT.len());
            out.extend_from_slice(html);
            out.extend_from_slice(LIVE_RELOAD_SCRIPT.as_bytes());
            return out;
        }
    };
    let lower = s.to_ascii_lowercase();
    if let Some(idx) = lower.rfind("</body>") {
        let mut out = String::with_capacity(s.len() + LIVE_RELOAD_SCRIPT.len());
        out.push_str(&s[..idx]);
        out.push_str(LIVE_RELOAD_SCRIPT);
        out.push_str(&s[idx..]);
        return out.into_bytes();
    }
    let mut out = String::with_capacity(s.len() + LIVE_RELOAD_SCRIPT.len());
    out.push_str(s);
    out.push_str(LIVE_RELOAD_SCRIPT);
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_for_known_extensions() {
        assert!(mime_for(Path::new("a.js")).starts_with("application/javascript"));
        assert!(mime_for(Path::new("a.mjs")).starts_with("application/javascript"));
        assert!(mime_for(Path::new("a.css")).starts_with("text/css"));
        assert!(mime_for(Path::new("a.html")).starts_with("text/html"));
        assert_eq!(
            mime_for(Path::new("a.map")),
            "application/json; charset=utf-8"
        );
        assert_eq!(mime_for(Path::new("a.woff2")), "font/woff2");
        assert_eq!(mime_for(Path::new("a.svg")), "image/svg+xml");
        assert_eq!(mime_for(Path::new("a.wasm")), "application/wasm");
    }

    #[test]
    fn mime_for_unknown_extension() {
        assert_eq!(
            mime_for(Path::new("nope.unknownext")),
            "application/octet-stream"
        );
        assert_eq!(mime_for(Path::new("noext")), "application/octet-stream");
    }

    #[test]
    fn inject_live_reload_client_inserts_before_body_close() {
        let html = b"<html><body><h1>hi</h1></body></html>";
        let injected = inject_live_reload_client(html);
        let s = std::str::from_utf8(&injected).expect("utf8");
        assert!(s.contains(LIVE_RELOAD_SCRIPT));
        let script_idx = s.find(LIVE_RELOAD_SCRIPT).expect("script present");
        let body_close = s.find("</body>").expect("body close present");
        assert!(script_idx < body_close);
    }

    #[test]
    fn inject_live_reload_client_appends_when_no_body() {
        let html = b"<html><h1>hi</h1></html>";
        let injected = inject_live_reload_client(html);
        let s = std::str::from_utf8(&injected).expect("utf8");
        assert!(s.ends_with(LIVE_RELOAD_SCRIPT));
    }

    #[test]
    fn inject_live_reload_client_handles_uppercase_body() {
        let html = b"<HTML><BODY><h1>hi</h1></BODY></HTML>";
        let injected = inject_live_reload_client(html);
        let s = std::str::from_utf8(&injected).expect("utf8");
        let script_idx = s.find(LIVE_RELOAD_SCRIPT).expect("script present");
        let body_close = s.find("</BODY>").expect("body close present");
        assert!(script_idx < body_close);
    }

    #[test]
    fn resolve_under_root_rejects_traversal() {
        let root = Path::new("/tmp/dist");
        assert!(resolve_under_root(root, "/../etc/passwd").is_none());
        assert!(resolve_under_root(root, "/foo/../../etc").is_none());
    }

    #[test]
    fn resolve_under_root_accepts_normal() {
        let root = Path::new("/tmp/dist");
        let p = resolve_under_root(root, "/main.js").expect("ok");
        assert_eq!(p, Path::new("/tmp/dist/main.js"));
        let p = resolve_under_root(root, "/").expect("ok");
        assert_eq!(p, Path::new("/tmp/dist"));
    }

    #[test]
    fn decode_path_handles_percent_encoding() {
        assert_eq!(decode_path("/a%20b.js"), "/a b.js");
        assert_eq!(decode_path("/plain.js"), "/plain.js");
        assert_eq!(decode_path("/%2F"), "//");
    }

    #[test]
    fn devserver_config_defaults() {
        let c = DevServerConfig::new("/tmp/dist");
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 4200);
        assert_eq!(c.root, PathBuf::from("/tmp/dist"));
    }

    #[test]
    fn devserver_config_with_overrides() {
        let c = DevServerConfig::new("/tmp/dist")
            .with_host("0.0.0.0")
            .with_port(0);
        assert_eq!(c.host, "0.0.0.0");
        assert_eq!(c.port, 0);
    }

    #[test]
    fn is_index_html_recognizes_variants() {
        assert!(is_index_html(Path::new("/x/index.html")));
        assert!(is_index_html(Path::new("/x/INDEX.HTML")));
        assert!(!is_index_html(Path::new("/x/main.js")));
    }
}
