//! Localhost REST API that lets an AI (or any local script) control the
//! running aetherium UI: open shell/log tabs, run commands on a target, and
//! read/replace files — all through the visible UI's own SSH sessions.
//!
//! - Binds 127.0.0.1 only; every endpoint except `/` and `/health` needs
//!   `Authorization: Bearer <token>` where the token lives in
//!   `<config dir>/api_token` (created on first start).
//! - Port: `AETHERIUM_API_PORT`, else 48920..=48925, else ephemeral.
//! - Requests cross to the UI thread as [`ApiRequest`]s; replies come back
//!   over oneshot channels as serde_json `Value`s of the shape
//!   `{ "ok": true, ... }` or `{ "ok": false, "error": "..." }`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::crypto::config_dir;

/// A request from the HTTP layer to the UI thread. The reply convention:
/// `{ "ok": true, ... }` on success, `{ "ok": false, "error": "..." }` on
/// failure — the HTTP layer wraps it with a status code and serves it.
pub enum ApiRequest {
    /// What exists right now: profiles and open tabs (AI discovery).
    Status { reply: Responder },
    /// Open a visible shell tab for the target profile.
    OpenSession { target: String, reply: Responder },
    /// Open a SnakeTail-style log-follow tab for a remote file.
    OpenLog { target: String, path: String, reply: Responder },
    /// Run a command on the target's connected session.
    Exec {
        target: String,
        command: String,
        timeout_secs: u64,
        reply: Responder,
    },
    /// List a remote directory.
    ListFiles { target: String, path: String, reply: Responder },
    /// Download a remote file; the reply carries base64 content.
    DownloadFile { target: String, path: String, reply: Responder },
    /// Replace/upload a remote file with the given content.
    UploadFile {
        target: String,
        path: String,
        content: Vec<u8>,
        reply: Responder,
    },
}

pub type Responder = std_mpsc::Sender<Value>;

/// What startup advertises (log line + status bar).
pub struct ApiInfo {
    pub port: u16,
    pub token_path: std::path::PathBuf,
}

pub fn url(info: &ApiInfo) -> String {
    format!("http://127.0.0.1:{}", info.port)
}

/// Load (or create) the bearer token.
fn load_token() -> (String, std::path::PathBuf) {
    let path = config_dir().join("api_token");
    if let Ok(token) = std::fs::read_to_string(&path) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return (token, path);
        }
    }
    let token = format!("{:016x}{:016x}", rand::random::<u64>(), rand::random::<u64>());
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new(".")));
    let _ = std::fs::write(&path, &token);
    (token, path)
}

/// Start the listener thread. Returns `None` when no port could be bound.
pub fn start(ui_tx: std_mpsc::Sender<ApiRequest>) -> Option<ApiInfo> {
    let (token, token_path) = load_token();
    let env_port = std::env::var("AETHERIUM_API_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok());
    let mut listener: Option<TcpListener> = None;
    let mut port = 0u16;
    let mut candidates: Vec<u16> = env_port.into_iter().collect();
    candidates.extend(48920..=48925);
    candidates.push(0);
    for candidate in candidates {
        match TcpListener::bind(("127.0.0.1", candidate)) {
            Ok(bound) => {
                port = bound.local_addr().ok()?.port();
                listener = Some(bound);
                break;
            }
            Err(_) => continue,
        }
    }
    let listener = listener?;
    listener
        .set_nonblocking(false)
        .ok()?;
    let token_for_thread = token.clone();
    std::thread::Builder::new()
        .name("aetherium-api".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let ui_tx = ui_tx.clone();
                        let token = token_for_thread.clone();
                        std::thread::spawn(move || {
                            let _ = handle_connection(stream, ui_tx, token);
                        });
                    }
                    Err(_) => continue,
                }
            }
        })
        .ok()?;
    Some(ApiInfo { port, token_path })
}

struct Request {
    method: String,
    path: String,
    query: std::collections::HashMap<String, String>,
    headers: std::collections::HashMap<String, String>,
    body: Vec<u8>,
}

/// Minimal HTTP/1.1 read: request line, headers, then the body.
fn read_request(stream: &mut std::net::TcpStream) -> Option<Request> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            return None;
        }
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let target = parts.next()?.to_string();
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':')?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
        .min(512 * 1024 * 1024);
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).ok()?;
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Some(Request {
        method,
        path: path.to_string(),
        query: parse_query(query),
        headers,
        body,
    })
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode(key), decode(value)))
        .collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                if let (Some(hi), Some(lo)) =
                    (hex_val(bytes.get(index + 1)), hex_val(bytes.get(index + 2)))
                {
                    out.push(hi * 16 + lo);
                    index += 3;
                } else {
                    out.push(b'%');
                    index += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(byte: Option<&u8>) -> Option<u8> {
    match byte {
        Some(b'0'..=b'9') => Some(byte.unwrap() - b'0'),
        Some(b'a'..=b'f') => Some(byte.unwrap() - b'a' + 10),
        Some(b'A'..=b'F') => Some(byte.unwrap() - b'A' + 10),
        _ => None,
    }
}

fn write_json(stream: &mut std::net::TcpStream, status: u16, value: &Value) {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

fn err(message: impl Into<String>) -> Value {
    json!({"ok": false, "error": message.into()})
}

fn handle_connection(
    mut stream: std::net::TcpStream,
    ui_tx: std_mpsc::Sender<ApiRequest>,
    token: String,
) -> std::io::Result<()> {
    let Some(request) = read_request(&mut stream) else {
        return Ok(());
    };
    // The root documents the API openly; everything else needs the token.
    let authorized = request
        .headers
        .get("authorization")
        .is_some_and(|value| value == format!("Bearer {token}").as_str());
    if request.path == "/" {
        write_json(&mut stream, 200, &docs(&token));
        return Ok(());
    }
    if request.path == "/health" {
        write_json(&mut stream, 200, &json!({"ok": true, "service": "aetherium"}));
        return Ok(());
    }
    if !authorized {
        write_json(&mut stream, 401, &err("missing or invalid bearer token; see the api_token file"));
        return Ok(());
    }

    let (tx, rx) = std_mpsc::channel::<Value>();
    let timeout_secs: u64 = request
        .query
        .get("timeout")
        .and_then(|value| value.parse().ok())
        .unwrap_or(60)
        .clamp(1, 900);

    let forwarded: Option<ApiRequest> = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/status") => Some(ApiRequest::Status { reply: tx }),
        ("POST", "/sessions") => {
            let body = json_body(&request);
            Some(ApiRequest::OpenSession {
                target: target_of(&request, &body),
                reply: tx,
            })
        }
        ("POST", "/logs") => {
            let body = json_body(&request);
            Some(ApiRequest::OpenLog {
                target: target_of(&request, &body),
                path: path_of(&request, &body),
                reply: tx,
            })
        }
        ("POST", "/exec") => {
            let body = json_body(&request);
            let timeout_secs = body
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(timeout_secs)
                .clamp(1, 900);
            let command = body
                .get("command")
                .and_then(Value::as_str)
                .or_else(|| request.query.get("command").map(String::as_str))
                .unwrap_or_default()
                .to_string();
            if command.is_empty() {
                write_json(&mut stream, 400, &err("missing 'command'"));
                return Ok(());
            }
            Some(ApiRequest::Exec {
                target: target_of(&request, &body),
                command,
                timeout_secs,
                reply: tx,
            })
        }
        ("GET", "/files") => Some(ApiRequest::ListFiles {
            target: request.query.get("target").cloned().unwrap_or_default(),
            path: request.query.get("path").cloned().unwrap_or_else(|| "/".into()),
            reply: tx,
        }),
        ("GET", "/file") => Some(ApiRequest::DownloadFile {
            target: request.query.get("target").cloned().unwrap_or_default(),
            path: request.query.get("path").cloned().unwrap_or_default(),
            reply: tx,
        }),
        ("PUT", "/file") => Some(ApiRequest::UploadFile {
            target: request.query.get("target").cloned().unwrap_or_default(),
            path: request.query.get("path").cloned().unwrap_or_default(),
            content: request.body.clone(),
            reply: tx,
        }),
        _ => None,
    };

    let Some(forwarded) = forwarded else {
        write_json(&mut stream, 404, &err("unknown route; GET / for the endpoint list"));
        return Ok(());
    };
    let ok = ui_tx.send(forwarded).is_ok();
    if !ok {
        write_json(&mut stream, 504, &err("UI is gone"));
        return Ok(());
    }
    let value = match rx.recv_timeout(Duration::from_secs(timeout_secs.saturating_add(10))) {
        Ok(value) => value,
        Err(_) => err(format!("request timed out after {timeout_secs}s")),
    };
    write_json(&mut stream, 200, &value);
    Ok(())
}

fn json_body(request: &Request) -> Value {
    if request
        .headers
        .get("content-type")
        .is_some_and(|value| value.contains("application/json"))
    {
        serde_json::from_slice(&request.body).unwrap_or(Value::Null)
    } else {
        Value::Null
    }
}

fn target_of(request: &Request, body: &Value) -> String {
    body.get("target")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| request.query.get("target").cloned().unwrap_or_default())
}

fn path_of(request: &Request, body: &Value) -> String {
    body.get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| request.query.get("path").cloned().unwrap_or_default())
}

/// Self-describing API index: every endpoint with parameters and examples.
fn docs(token: &str) -> Value {
    json!({
        "service": "aetherium REST API",
        "description": "Control the running aetherium UI: open tabs, run commands on targets, read/replace files. All operations use the UI's visible SSH sessions.",
        "auth": {
            "header": "Authorization: Bearer <token>",
            "token_file": "api_token in the aetherium config dir",
            "current_token_preview": format!("{}…", &token[..8.min(token.len())]),
        },
        "endpoints": [
            {"method": "GET", "path": "/health", "auth": false, "description": "Liveness probe."},
            {"method": "GET", "path": "/status", "description": "Profiles and open tabs (use it to discover targets and state)."},
            {"method": "POST", "path": "/sessions", "description": "Open a visible shell tab for a target profile.",
             "params": {"target": "profile name (see /status)"},
             "example": {"target": "raspberry3bplus"}},
            {"method": "POST", "path": "/logs", "description": "Open a SnakeTail-style log-follow view of a remote file.",
             "params": {"target": "profile name", "path": "remote file path"},
             "example": {"target": "raspberry3bplus", "path": "/var/log/syslog"}},
            {"method": "POST", "path": "/exec", "description": "Run a command on the target's connected session.",
             "params": {"target": "profile name", "command": "shell command", "timeout_secs": "optional, default 60"},
             "example": {"target": "raspberry3bplus", "command": "uptime"}},
            {"method": "GET", "path": "/files?target=&path=", "description": "List a remote directory.",
             "example": "/files?target=raspberry3bplus&path=/var/log"},
            {"method": "GET", "path": "/file?target=&path=", "description": "Download a remote file (base64 content)."},
            {"method": "PUT", "path": "/file?target=&path=", "description": "Replace/upload a remote file; the request body is the new content (application/octet-stream)."},
        ],
        "notes": [
            "Endpoints return {\"ok\": true, ...} or {\"ok\": false, \"error\": ...} with HTTP 200; auth/transport failures use 401/404/504.",
            "Commands and file operations require a connected session: open one with POST /sessions or the Connect button.",
            "Exec stdout/stderr are UTF-8 when valid, plus *_base64 for binary data."
        ],
    })
}
