//! Opt-in local HTTP IPC endpoint exposing the visible transcript.
//!
//! The HTTP thread never reads GPUI state. Instead, the app periodically
//! copies a small transcript snapshot into [`SharedSnapshot`], and the IPC
//! endpoint serves that copy to local clients such as `wisp-mcp`.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

use crate::app::{AppModel, SessionState, View};

const MAX_BODY_BYTES: usize = 1024 * 1024;

pub type SharedSnapshot = Arc<Mutex<ConversationSnapshot>>;

#[derive(Debug, Clone, Default)]
pub struct ConversationSnapshot {
    view: String,
    state: String,
    session_id: Option<i64>,
    title: Option<String>,
    segments: Vec<SnapshotSegment>,
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct SnapshotSegment {
    source: String,
    text: String,
    start_seconds: f64,
    end_seconds: f64,
    is_final: bool,
}

#[derive(Debug, Clone)]
pub struct IpcConfig {
    pub addr: String,
    pub token: Option<String>,
}

pub struct IpcServer {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl IpcServer {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub fn new_shared_snapshot() -> SharedSnapshot {
    Arc::new(Mutex::new(ConversationSnapshot::default()))
}

pub fn start(
    config: IpcConfig,
    snapshot: SharedSnapshot,
) -> Result<IpcServer, String> {
    let listener = TcpListener::bind(&config.addr)
        .map_err(|err| format!("failed to bind {}: {err}", config.addr))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to configure {}: {err}", config.addr))?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = stop.clone();
    let thread_name = "wisp-ipc-http".to_owned();
    let handle = thread::Builder::new()
        .name(thread_name)
        .spawn(move || run_http_server(listener, config, snapshot, stop_for_thread))
        .map_err(|err| format!("failed to start IPC HTTP thread: {err}"))?;
    Ok(IpcServer {
        stop,
        handle: Some(handle),
    })
}

pub fn env_enabled() -> bool {
    match std::env::var("WISP_IPC") {
        Ok(value) => !matches!(value.as_str(), "" | "0" | "false" | "off"),
        Err(_) => std::env::var("WISP_IPC_ADDR").is_ok(),
    }
}

pub fn env_addr_override() -> Option<String> {
    std::env::var("WISP_IPC_ADDR")
        .ok()
        .filter(|addr| !addr.is_empty())
}

pub fn env_token() -> Option<String> {
    std::env::var("WISP_IPC_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
}

pub fn refresh_snapshot(
    snapshot: &SharedSnapshot,
    model: &AppModel,
) {
    if let Ok(mut current) = snapshot.lock() {
        *current = ConversationSnapshot::from_model(model);
    }
}

impl ConversationSnapshot {
    fn from_model(model: &AppModel) -> Self {
        let (view, session_id, title) = match &model.view {
            View::Library => ("library", None, None),
            View::LiveSession => (
                "live_session",
                model.current_session_id.map(wisp_core::SessionId::as_i64),
                None,
            ),
            View::History { session_id } => (
                "history",
                Some(session_id.as_i64()),
                model
                    .viewed_session
                    .as_ref()
                    .map(|session| session.title.clone()),
            ),
        };
        let title = title.or_else(|| {
            session_id.and_then(|id| {
                model
                    .library
                    .iter()
                    .find(|session| session.id.as_i64() == id)
                    .map(|session| session.title.clone())
            })
        });
        Self {
            view: view.to_owned(),
            state: state_label(model.state).to_owned(),
            session_id,
            title,
            segments: model
                .segments
                .iter()
                .map(|segment| SnapshotSegment {
                    source: segment.source.as_str().to_owned(),
                    text: segment.text.clone(),
                    start_seconds: segment.start_seconds,
                    end_seconds: segment.end_seconds,
                    is_final: segment.is_final,
                })
                .collect(),
            last_error: model.last_error.as_ref().map(ToString::to_string),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "view": self.view,
            "state": self.state,
            "session_id": self.session_id,
            "title": self.title,
            "last_error": self.last_error,
            "segments": self.segments.iter().map(SnapshotSegment::to_json).collect::<Vec<_>>()
        })
    }
}

impl SnapshotSegment {
    fn to_json(&self) -> Value {
        json!({
            "source": self.source,
            "text": self.text,
            "start_seconds": self.start_seconds,
            "end_seconds": self.end_seconds,
            "is_final": self.is_final
        })
    }
}

fn state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Idle => "idle",
        SessionState::Starting => "starting",
        SessionState::Recording { .. } => "recording",
        SessionState::Stopping => "stopping",
        SessionState::Failed => "failed",
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_http_server(
    listener: TcpListener,
    config: IpcConfig,
    snapshot: SharedSnapshot,
    stop: Arc<AtomicBool>,
) {
    eprintln!(
        "wisp: IPC endpoint listening at http://{}/conversation",
        config.addr
    );
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let snapshot = snapshot.clone();
                let config = config.clone();
                let spawn = thread::Builder::new()
                    .name("wisp-ipc-http-connection".to_owned())
                    .spawn(move || handle_connection(stream, &config, &snapshot));
                if let Err(err) = spawn {
                    eprintln!("wisp: failed to handle IPC HTTP connection: {err}");
                }
            },
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            },
            Err(err) => eprintln!("wisp: IPC HTTP accept failed: {err}"),
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    config: &IpcConfig,
    snapshot: &SharedSnapshot,
) {
    let response = match read_http_request(&mut stream) {
        Ok(request) => handle_http_request(&request, config, snapshot),
        Err(err) => HttpResponse::text(400, format!("bad request: {err}")),
    };
    if let Err(err) = write_http_response(&mut stream, &response) {
        eprintln!("wisp: failed to write IPC HTTP response: {err}");
    }
}

struct HttpRequest {
    method: String,
    path: String,
    authorization: Option<String>,
}

struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl HttpResponse {
    fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
        }
    }

    fn text(
        status: u16,
        body: impl Into<String>,
    ) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into().into_bytes(),
        }
    }

    fn json(
        status: u16,
        body: &Value,
    ) -> Self {
        let body = serde_json::to_vec(&body).unwrap_or_else(|err| {
            format!(r#"{{"error":"failed to serialize response: {err}"}}"#).into_bytes()
        });
        Self {
            status,
            content_type: "application/json",
            body,
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts
        .next()
        .and_then(|raw| raw.split('?').next())
        .unwrap_or_default()
        .to_owned();
    let mut content_length = 0usize;
    let mut authorization = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
        }
    }
    if content_length > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request body too large",
        ));
    }
    let mut body = vec![0; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(HttpRequest {
        method,
        path,
        authorization,
    })
}

fn handle_http_request(
    request: &HttpRequest,
    config: &IpcConfig,
    snapshot: &SharedSnapshot,
) -> HttpResponse {
    if request.method == "OPTIONS" {
        return HttpResponse::empty(204);
    }
    if request.method == "GET" && request.path == "/health" {
        return HttpResponse::text(200, "ok");
    }
    if !authorized(request, config) {
        return HttpResponse::json(401, &json!({"error": "unauthorized"}));
    }
    if request.method != "GET" {
        return HttpResponse::text(405, "IPC endpoint expects GET /conversation");
    }
    if request.path != "/conversation" {
        return HttpResponse::text(404, "not found");
    }
    let snapshot = match snapshot.lock() {
        Ok(snapshot) => snapshot.to_json(),
        Err(_) => return HttpResponse::json(503, &json!({"error": "snapshot unavailable"})),
    };
    HttpResponse::json(200, &snapshot)
}

fn authorized(
    request: &HttpRequest,
    config: &IpcConfig,
) -> bool {
    let Some(token) = &config.token else {
        return true;
    };
    let expected = format!("Bearer {token}");
    request.authorization.as_deref() == Some(expected.as_str())
}

fn write_http_response(
    stream: &mut TcpStream,
    response: &HttpResponse,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: null\r\nAccess-Control-Allow-Methods: GET, OPTIONS\r\nAccess-Control-Allow-Headers: authorization\r\n\r\n",
        response.status,
        reason_phrase(response.status),
        response.content_type,
        response.body.len()
    )?;
    stream.write_all(&response.body)
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::{ConversationSnapshot, HttpRequest, IpcConfig, SnapshotSegment, authorized};

    #[test]
    fn conversation_snapshot_serializes_segments() {
        let snapshot = ConversationSnapshot {
            view: "live_session".into(),
            state: "recording".into(),
            session_id: Some(7),
            title: Some("demo".into()),
            segments: vec![SnapshotSegment {
                source: "mic".into(),
                text: "今日はロードマップの話をしています。".into(),
                start_seconds: 0.0,
                end_seconds: 2.0,
                is_final: true,
            }],
            last_error: None,
        };
        let value = snapshot.to_json();
        assert_eq!(
            value["segments"][0]["text"],
            "今日はロードマップの話をしています。"
        );
    }

    #[test]
    fn token_auth_requires_bearer_header() {
        let config = IpcConfig {
            addr: "127.0.0.1:8765".into(),
            token: Some("secret".into()),
        };
        let missing = HttpRequest {
            method: "GET".into(),
            path: "/conversation".into(),
            authorization: None,
        };
        assert!(!authorized(&missing, &config));

        let present = HttpRequest {
            authorization: Some("Bearer secret".into()),
            ..missing
        };
        assert!(authorized(&present, &config));
    }
}
