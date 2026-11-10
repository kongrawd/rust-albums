//! Authentication. Browsers sign in through the sign-in form and then carry a
//! session cookie (see `session`). Scripts and tools (curl, wget) may instead
//! send HTTP Basic credentials (RFC 7617) on every request. Only safe over TLS.
//! Failed attempts of either kind are throttled per client ([`Throttle`]).
//!
//! Basic credentials are checked here rather than with tower-http's
//! `ValidateRequestHeaderLayer::basic`, which compares the whole header with
//! `==` (not constant-time) and matches the scheme case-sensitively.

use crate::crypto::{Digest, digests_equal, sha256};
use crate::{AppState, routes};
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{self, AsHeaderName},
    },
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::headers::{Authorization, HeaderMapExt, authorization::Basic};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use tokio::time::Instant;

const BASIC_CHALLENGE: &str = r#"Basic realm="albums", charset="UTF-8""#;
/// Wait after a client's first failed attempt; doubles with each further failure.
const FIRST_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(60);
/// A client's failures are forgotten after this long without new ones.
const FORGET_AFTER: Duration = Duration::from_secs(15 * 60);
/// Clients tracked before forgotten ones are pruned, bounding memory.
const PRUNE_ABOVE: usize = 1024;

pub struct Credentials {
    /// SHA-256 of `user:password`, so comparisons are fixed-length and constant-time.
    expected: Digest,
    throttle: Throttle,
}

/// Outcome of checking credentials.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Valid,
    Invalid,
    /// This client failed recently and must wait this long before trying again.
    Throttled(Duration),
}

impl Credentials {
    pub fn new(user: &str, password: &str) -> Result<Self, String> {
        if user.contains(':') {
            return Err("AUTH_USER must not contain ':' (RFC 7617)".into());
        }
        Ok(Self {
            expected: sha256(format!("{user}:{password}").as_bytes()),
            throttle: Throttle::default(),
        })
    }

    /// Checks a username and password from `client` (sign-in form or Basic).
    pub fn verify(&self, client: IpAddr, user: &str, password: &str) -> Verdict {
        if let Err(wait) = self.throttle.check(client) {
            return Verdict::Throttled(wait);
        }
        let valid = !user.contains(':')
            && digests_equal(
                &sha256(format!("{user}:{password}").as_bytes()),
                &self.expected,
            );
        if valid {
            self.throttle.forget(client);
            Verdict::Valid
        } else {
            self.throttle.record_failure(client);
            Verdict::Invalid
        }
    }
}

/// Per-client throttle for failed attempts: after a failure the same client
/// must wait [`FIRST_DELAY`], doubling up to [`MAX_DELAY`]; a success clears
/// it. Other clients are never delayed, so failures from one address can't
/// lock the real user out. Clients are told how long to wait instead of being
/// held, so no connections pile up. Behind a reverse proxy all clients share
/// the proxy's address.
#[derive(Default)]
struct Throttle {
    clients: Mutex<HashMap<IpAddr, Failures>>,
}

struct Failures {
    count: u32,
    retry_at: Instant,
}

impl Throttle {
    fn check(&self, client: IpAddr) -> Result<(), Duration> {
        let clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        match clients.get(&client) {
            Some(failures) if failures.retry_at > Instant::now() => {
                Err(failures.retry_at - Instant::now())
            }
            _ => Ok(()),
        }
    }

    fn record_failure(&self, client: IpAddr) {
        let now = Instant::now();
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if clients.len() > PRUNE_ABOVE {
            clients.retain(|_, failures| failures.retry_at + FORGET_AFTER > now);
        }
        let failures = clients.entry(client).or_insert(Failures {
            count: 0,
            retry_at: now,
        });
        if failures.retry_at + FORGET_AFTER <= now {
            failures.count = 0; // quiet long enough: start over
        }
        failures.count += 1;
        let delay = FIRST_DELAY.saturating_mul(1 << (failures.count - 1).min(6));
        failures.retry_at = now + delay.min(MAX_DELAY);
    }

    fn forget(&self, client: IpAddr) {
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        clients.remove(&client);
    }
}

/// The connecting client's address; unspecified when unknown (e.g. in tests).
pub fn client_ip(connection: Option<&ConnectInfo<SocketAddr>>) -> IpAddr {
    connection.map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |info| info.0.ip())
}

/// `429 Too Many Requests` with `Retry-After`, rounded up to whole seconds.
pub fn too_many_attempts(wait: Duration, body: impl IntoResponse) -> Response {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    let retry_after = HeaderValue::from(seconds.max(1));
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, retry_after)],
        body,
    )
        .into_response()
}

/// Lets a request through if it has a live session, or valid Basic credentials
/// from a non-browser client. Otherwise a browser opening a page or a direct
/// link is sent to the sign-in form (and back afterwards); a browser loading a
/// resource gets a bare 401; a script gets a 401 with a Basic challenge.
///
/// Browsers must always hold a session: one that ever answered a Basic prompt
/// resends those credentials on every request until it quits, which would keep
/// it signed in after signing out. For the same reason browsers never get the
/// Basic challenge, which would make them show that prompt.
pub async fn require_sign_in(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    if state.sessions.is_signed_in(headers) {
        return next.run(request).await;
    }
    let browser = is_browser(headers);
    if !browser && let Some(Authorization(basic)) = headers.typed_get::<Authorization<Basic>>() {
        let client = client_ip(request.extensions().get());
        match state
            .credentials
            .verify(client, basic.username(), basic.password())
        {
            Verdict::Valid => return next.run(request).await,
            Verdict::Throttled(wait) => return too_many_attempts(wait, "Too many attempts"),
            Verdict::Invalid => {}
        }
    }
    if accepts_html(headers) {
        let target = request
            .uri()
            .path_and_query()
            .map_or(routes::HOME, |t| t.as_str());
        return Redirect::to(&routes::sign_in_url(target)).into_response();
    }
    if browser {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    let challenge = [(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(BASIC_CHALLENGE),
    )];
    (StatusCode::UNAUTHORIZED, challenge, "Unauthorized").into_response()
}

/// Browsers send Fetch Metadata on every HTTPS request, older ones
/// `Upgrade-Insecure-Requests` on navigations, and all of them a user agent
/// starting with `Mozilla/`. Scripts and tools send none of these; one that fakes a browser
/// user agent only loses Basic auth.
fn is_browser(headers: &HeaderMap) -> bool {
    headers.contains_key("sec-fetch-site")
        || headers.contains_key("upgrade-insecure-requests")
        || header_text(headers, header::USER_AGENT)
            .is_some_and(|agent| agent.starts_with("Mozilla/"))
}

fn accepts_html(headers: &HeaderMap) -> bool {
    header_text(headers, header::ACCEPT).is_some_and(|accept| accept.contains("text/html"))
}

fn header_text(headers: &HeaderMap, name: impl AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Fetch Metadata resource isolation (<https://web.dev/articles/fetch-metadata>).
/// Requests from this site's own pages, or opened directly (typed, bookmarked,
/// pasted link), pass. Other sites may only navigate here with a GET, such as a
/// link in an email, which still has to sign in. They can't embed images, frame
/// pages or post forms (CSRF), even while the user is signed in.
pub async fn reject_cross_site(request: Request, next: Next) -> Response {
    if !is_allowed_initiator(&request) {
        return (StatusCode::FORBIDDEN, "Cross-site request rejected").into_response();
    }
    next.run(request).await
}

fn is_allowed_initiator(request: &Request) -> bool {
    let headers = request.headers();
    match header_text(headers, "sec-fetch-site") {
        Some("same-origin" | "none") => true,
        Some(_) => {
            request.method() == Method::GET
                && header_text(headers, "sec-fetch-mode") == Some("navigate")
                && header_text(headers, "sec-fetch-dest") == Some("document")
        }
        None if request.method() == Method::GET => true,
        // Older browsers without Fetch Metadata send `Origin` on form posts. A
        // browser sending neither can't show where the form came from, so it is
        // refused; scripts and tools can't be tricked into posting anyway.
        None => match header_text(headers, header::ORIGIN) {
            Some(origin) => origin_matches_host(request, origin),
            None => !is_browser(headers),
        },
    }
}

fn origin_matches_host(request: &Request, origin: &str) -> bool {
    // HTTP/2 carries the host in the URI authority, HTTP/1.1 in the Host header.
    let host = request
        .uri()
        .authority()
        .map(|authority| authority.as_str())
        .or_else(|| header_text(request.headers(), header::HOST));
    host.is_some_and(|host| origin.strip_prefix("https://") == Some(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
    const MALLORY: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 66));

    #[test]
    fn verifies_exact_credentials_only() {
        let credentials = Credentials::new("alice", "s3cret:with:colons").unwrap();
        assert_eq!(
            credentials.verify(ALICE, "alice", "s3cret:with:colons"),
            Verdict::Valid
        );
        for (user, password) in [
            ("alice", "s3cret"),
            ("alice:s3cret", "with:colons"),
            ("bob", "s3cret:with:colons"),
        ] {
            let credentials = Credentials::new("alice", "s3cret:with:colons").unwrap();
            assert_eq!(
                credentials.verify(ALICE, user, password),
                Verdict::Invalid,
                "{user}"
            );
        }
    }

    #[test]
    fn basic_scheme_is_case_insensitive() {
        use axum_extra::headers::HeaderMapExt;
        for scheme in ["Basic", "basic", "BASIC", "bAsIc"] {
            let mut headers = HeaderMap::new();
            let value = format!("{scheme} YWxpY2U6cHc="); // alice:pw
            headers.insert(header::AUTHORIZATION, HeaderValue::try_from(value).unwrap());
            let Authorization(basic) = headers.typed_get::<Authorization<Basic>>().unwrap();
            assert_eq!(
                (basic.username(), basic.password()),
                ("alice", "pw"),
                "{scheme}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failures_throttle_only_the_failing_client() {
        let credentials = Credentials::new("alice", "pw").unwrap();
        assert_eq!(
            credentials.verify(MALLORY, "alice", "guess"),
            Verdict::Invalid
        );
        // Mallory has to wait; even the right password is refused meanwhile...
        assert!(matches!(
            credentials.verify(MALLORY, "alice", "pw"),
            Verdict::Throttled(_)
        ));
        // ...while Alice, on another address, signs in at once.
        assert_eq!(credentials.verify(ALICE, "alice", "pw"), Verdict::Valid);

        // The wait doubles with each further failure, up to the maximum.
        tokio::time::advance(FIRST_DELAY).await;
        assert_eq!(
            credentials.verify(MALLORY, "alice", "guess"),
            Verdict::Invalid
        );
        assert_eq!(
            credentials.verify(MALLORY, "alice", "guess"),
            Verdict::Throttled(2 * FIRST_DELAY)
        );
        for _ in 0..10 {
            tokio::time::advance(MAX_DELAY).await;
            credentials.verify(MALLORY, "alice", "guess");
        }
        assert_eq!(
            credentials.verify(MALLORY, "alice", "guess"),
            Verdict::Throttled(MAX_DELAY)
        );

        // A success clears the client's record.
        tokio::time::advance(MAX_DELAY).await;
        assert_eq!(credentials.verify(MALLORY, "alice", "pw"), Verdict::Valid);
        assert_eq!(
            credentials.verify(MALLORY, "alice", "guess"),
            Verdict::Invalid
        );
        assert_eq!(
            credentials.verify(MALLORY, "alice", "guess"),
            Verdict::Throttled(FIRST_DELAY)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_memory_stays_bounded() {
        let credentials = Credentials::new("alice", "pw").unwrap();
        for i in 0..=PRUNE_ABOVE as u32 {
            credentials.verify(IpAddr::from(i.to_be_bytes()), "alice", "guess");
        }
        tokio::time::advance(MAX_DELAY + FORGET_AFTER).await;
        credentials.verify(MALLORY, "alice", "guess");
        let tracked = credentials.throttle.clients.lock().unwrap().len();
        assert_eq!(tracked, 1, "forgotten clients were pruned");
    }

    #[test]
    fn recognises_browsers() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.insert(*name, HeaderValue::from_static(value));
            }
            map
        };
        assert!(is_browser(&headers(&[("sec-fetch-site", "none")])));
        assert!(is_browser(&headers(&[("upgrade-insecure-requests", "1")])));
        assert!(is_browser(&headers(&[(
            "user-agent",
            "Mozilla/5.0 (X11) Safari/605"
        )])));
        assert!(!is_browser(&headers(&[("user-agent", "curl/8.7.1")])));
        assert!(!is_browser(&headers(&[])));
    }

    #[test]
    fn rejects_colon_in_user() {
        assert!(Credentials::new("al:ice", "pw").is_err());
    }
}
