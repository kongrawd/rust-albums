//! The access log: one line per request in the Combined Log Format, on stdout,
//! as the official nginx image does (diagnostics go to stderr). Docker,
//! systemd and terminals all collect the stream; the app writes no log files.
//!
//! `192.168.0.10 - - [15/Nov/2026:20:19:04 +0000] "GET /media/{item_id} HTTP/2.0" 206 2 "-" "Mozilla/5.0 …"`
//!
//! The query string is left out, so a token in a URL can never land in the
//! log, and the referer is always `-`. Bytes a client chose (path, user agent)
//! are escaped like nginx does, so no client can forge lines or fields.
//! `/healthz` isn't logged: the container healthcheck would flood the log.

use crate::{AppState, auth, routes};
use axum::{
    body::HttpBody,
    extract::{Request, State},
    http::{Method, header},
    middleware::Next,
    response::Response,
};
use std::fmt::Write as _;
use std::io::Write as _;
#[cfg(test)]
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// Lines kept in memory by tests.
#[cfg(test)]
pub type Lines = Arc<Mutex<Vec<String>>>;

/// Where access-log lines go.
pub enum AccessLog {
    Stdout,
    #[cfg(test)]
    Memory(Lines),
}

impl AccessLog {
    fn write(&self, line: String) {
        match self {
            // One locked write per line, so lines from parallel requests never mix.
            Self::Stdout => {
                let _ = writeln!(std::io::stdout().lock(), "{line}");
            }
            #[cfg(test)]
            Self::Memory(lines) => lines.lock().unwrap().push(line),
        }
    }
}

/// Logs the final answer to every request but `/healthz`.
pub async fn record(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request.uri().path() == routes::HEALTH {
        return next.run(request).await;
    }
    let client = auth::client_ip(request.extensions().get());
    // Axum strips the body of a HEAD answer after this layer.
    let head = request.method() == Method::HEAD;
    let request_line = format!(
        "{} {} {:?}",
        request.method(),
        escape(request.uri().path().as_bytes()),
        request.version()
    );
    let agent = request
        .headers()
        .get(header::USER_AGENT)
        .map_or_else(|| "-".to_string(), |agent| escape(agent.as_bytes()));
    let response = next.run(request).await;
    let size = if head {
        Some("0".to_string())
    } else {
        response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|length| length.to_str().ok().map(str::to_string))
            .or_else(|| response.body().size_hint().exact().map(|n| n.to_string()))
    }
    .unwrap_or_else(|| "-".to_string());
    state.access_log.write(format!(
        r#"{client} - - [{}] "{request_line}" {} {size} "-" "{agent}""#,
        clf_time(OffsetDateTime::now_utc()),
        response.status().as_u16(),
    ));
    response
}

/// `15/Nov/2026:20:19:04 +0000`.
fn clf_time(now: OffsetDateTime) -> String {
    let month = now.month().to_string();
    format!(
        "{:02}/{}/{:04}:{:02}:{:02}:{:02} +0000",
        now.day(),
        &month[..3],
        now.year(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// Printable ASCII as is; `"`, `\` and every other byte as `\xHH`.
pub fn escape(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b' '..=b'~' if byte != b'"' && byte != b'\\' => text.push(char::from(byte)),
            _ => {
                let _ = write!(text, "\\x{byte:02x}");
            }
        }
    }
    text
}
