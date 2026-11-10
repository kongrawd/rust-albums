//! Server-side sign-in sessions, kept in the memory of this one process.
//!
//! The browser holds only a random 256-bit token in a `__Host-` cookie; the
//! token leaves this module only inside that cookie's `Set-Cookie` value.
//! Sessions are keyed by the token's SHA-256, so the store never holds a usable
//! token. Signing out deletes the session, which makes every copy of the cookie
//! worthless.
//!
//! Checking a session takes a shared read lock and refreshes the idle timer
//! atomically, so concurrent requests (a page loading many images) never wait
//! for each other. Only sign-in, sign-out and the periodic purge take the write
//! lock. Timeouts use the monotonic clock, so wall-clock changes don't affect
//! them; time while the host is suspended isn't counted.
//!
//! Single instance only: another replica can't see these sessions, and
//! restarting the server signs everyone out (see docs/architecture.md "Single replica").
//!
//! Not tower-sessions: its in-memory store never deletes expired sessions,
//! keeps the raw session ids and has no absolute lifetime.

use crate::config;
use crate::crypto::{Digest, random, sha256};
use axum::http::{HeaderMap, HeaderValue};
use axum_extra::extract::cookie::CookieJar;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;
use tokio::time::Instant;

const COOKIE_NAME: &str = "__Host-albums_session";
const TOKEN_BYTES: usize = 32;
/// How often expired sessions are deleted from memory.
const PURGE_INTERVAL: Duration = Duration::from_secs(60);

/// The sessions of this process (see the module docs for the single-instance scope).
pub struct Sessions {
    sessions: RwLock<HashMap<Digest, Session>>,
    timeouts: config::Session,
}

struct Session {
    created: Instant,
    /// Last activity, as milliseconds after `created`. Atomic so readers can
    /// move it forward under the shared lock.
    last_seen_ms: AtomicU64,
}

impl Session {
    fn new(created: Instant) -> Self {
        Self {
            created,
            last_seen_ms: AtomicU64::new(0),
        }
    }

    fn is_live(&self, timeouts: &config::Session, now: Instant) -> bool {
        let age = now.saturating_duration_since(self.created);
        let last_seen = Duration::from_millis(self.last_seen_ms.load(Ordering::Relaxed));
        age < timeouts.max_lifetime && age.saturating_sub(last_seen) < timeouts.idle_timeout
    }

    fn mark_seen(&self, now: Instant) {
        let age = now.saturating_duration_since(self.created).as_millis() as u64;
        self.last_seen_ms.fetch_max(age, Ordering::Relaxed);
    }
}

impl Sessions {
    pub fn new(timeouts: &config::Session) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            timeouts: *timeouts,
        }
    }

    /// Starts a session; returns the `Set-Cookie` value that hands it to the browser.
    pub fn sign_in(&self) -> HeaderValue {
        let token = random::<TOKEN_BYTES>();
        self.write()
            .insert(sha256(&token), Session::new(Instant::now()));
        cookie_header(
            &URL_SAFE_NO_PAD.encode(token),
            self.timeouts.max_lifetime.as_secs(),
        )
    }

    /// Whether the request carries a live session. If so, its idle timer restarts.
    pub fn is_signed_in(&self, headers: &HeaderMap) -> bool {
        session_key(headers).is_some_and(|key| self.touch_if_live(&key))
    }

    /// Ends the request's session, if it has one. Safe to repeat.
    pub fn sign_out(&self, headers: &HeaderMap) {
        if let Some(key) = session_key(headers) {
            self.write().remove(&key);
        }
    }

    /// Deletes expired sessions every minute, so memory only holds live ones.
    /// Runs until the process exits.
    pub async fn run_purger(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(PURGE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            self.purge_expired();
        }
    }

    fn purge_expired(&self) {
        let now = Instant::now();
        let timeouts = self.timeouts;
        self.write()
            .retain(|_, session| session.is_live(&timeouts, now));
    }

    /// Restarts the idle timer of a live session; `false` if there is none.
    fn touch_if_live(&self, key: &Digest) -> bool {
        let now = Instant::now();
        match self.read().get(key) {
            Some(session) if session.is_live(&self.timeouts, now) => {
                session.mark_seen(now);
                true
            }
            _ => false, // expired sessions are removed by `purge_expired`
        }
    }

    // A panic while holding the lock can't leave the map half-updated, so a
    // poisoned lock is safe to keep using.
    fn read(&self) -> RwLockReadGuard<'_, HashMap<Digest, Session>> {
        self.sessions.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, HashMap<Digest, Session>> {
        self.sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// `Set-Cookie` value that deletes the session cookie.
pub fn expired_cookie() -> HeaderValue {
    cookie_header("", 0)
}

/// `__Host-` + `Secure` + `Path=/` pin the cookie to this exact HTTPS origin;
/// `HttpOnly` hides it from scripts; `SameSite=Strict` keeps other sites from
/// sending it.
fn cookie_header(token: &str, max_age_secs: u64) -> HeaderValue {
    HeaderValue::try_from(format!(
        "{COOKIE_NAME}={token}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}"
    ))
    .expect("cookie is ASCII")
}

/// The key of the request's session: the SHA-256 of its cookie token, if the
/// request has a well-formed one.
fn session_key(headers: &HeaderMap) -> Option<Digest> {
    let jar = CookieJar::from_headers(headers);
    let raw = URL_SAFE_NO_PAD.decode(jar.get(COOKIE_NAME)?.value()).ok()?;
    (raw.len() == TOKEN_BYTES).then(|| sha256(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;

    fn sessions(idle_timeout_secs: u64, max_lifetime_secs: u64) -> Sessions {
        Sessions::new(&config::Session {
            idle_timeout: Duration::from_secs(idle_timeout_secs),
            max_lifetime: Duration::from_secs(max_lifetime_secs),
        })
    }

    fn cookie_headers(cookie_value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let value = format!("{COOKIE_NAME}={cookie_value}");
        headers.insert(header::COOKIE, HeaderValue::try_from(value).unwrap());
        headers
    }

    /// Signs in and returns request headers carrying the new session's cookie.
    fn signed_in(store: &Sessions) -> HeaderMap {
        let set_cookie = store.sign_in();
        let pair = set_cookie.to_str().unwrap().split(';').next().unwrap();
        let token = pair.strip_prefix(&format!("{COOKIE_NAME}=")).unwrap();
        assert_eq!(token.len(), 43, "256-bit token, base64url without padding");
        cookie_headers(token)
    }

    #[tokio::test]
    async fn sign_in_and_sign_out() {
        let store = sessions(60, 600);
        let headers = signed_in(&store);
        assert!(store.is_signed_in(&headers));
        store.sign_out(&headers);
        assert!(!store.is_signed_in(&headers));
        store.sign_out(&headers); // repeating is harmless
    }

    #[tokio::test]
    async fn rejects_unknown_and_malformed_tokens() {
        let store = sessions(60, 600);
        signed_in(&store);
        for token in [
            URL_SAFE_NO_PAD.encode([7u8; TOKEN_BYTES]),
            "not base64!".into(),
            String::new(),
        ] {
            assert!(!store.is_signed_in(&cookie_headers(&token)), "{token:?}");
            store.sign_out(&cookie_headers(&token)); // ignored, no panic
        }
        assert!(!store.is_signed_in(&HeaderMap::new()));
    }

    #[tokio::test(start_paused = true)]
    async fn activity_restarts_the_idle_timer_until_the_max_lifetime() {
        let store = sessions(600, 3600);
        let headers = signed_in(&store);

        // Activity every 9 minutes keeps a 10-minute idle timeout alive...
        for _ in 0..6 {
            tokio::time::advance(Duration::from_secs(540)).await;
            assert!(store.is_signed_in(&headers));
        }
        // ...until the hour of max lifetime is over (54 min so far), however active.
        tokio::time::advance(Duration::from_secs(300)).await;
        assert!(store.is_signed_in(&headers), "59 minutes");
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(!store.is_signed_in(&headers), "60 minutes");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_sessions_expire_and_are_purged() {
        let store = sessions(600, 3600);
        let idle = signed_in(&store);
        tokio::time::advance(Duration::from_secs(300)).await;
        let active = signed_in(&store);
        tokio::time::advance(Duration::from_secs(301)).await;
        assert!(!store.is_signed_in(&idle), "idle for over 10 minutes");
        assert!(store.is_signed_in(&active));

        store.purge_expired();
        assert_eq!(store.read().len(), 1, "only the active session is kept");
    }

    #[test]
    fn reads_the_session_key_from_the_cookie_header() {
        let token = URL_SAFE_NO_PAD.encode([1u8; TOKEN_BYTES]);
        let mut headers = HeaderMap::new();
        let value = format!("a=1; {COOKIE_NAME}_x=no; {COOKIE_NAME}={token}");
        headers.insert(header::COOKIE, HeaderValue::try_from(value).unwrap());
        assert_eq!(session_key(&headers), Some(sha256(&[1u8; TOKEN_BYTES])));
        assert_eq!(session_key(&HeaderMap::new()), None);
    }
}
