//! The router, handlers (sign-in, sign-out and health too) and the security
//! headers on every response.

use crate::auth::{self, Verdict};
use crate::{AppState, html, routes, session};
use axum::{
    Extension, Form, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::Markup;
use serde::Deserialize;
use std::net::SocketAddr;
use std::time::Duration;
use tower::ServiceExt;
use tower_http::{services::ServeFile, set_header::SetResponseHeaderLayer};

/// Pages run no scripts and load nothing but images and videos from this
/// server, plus the inline styles of `html.rs`. Forms post only here, and other
/// sites can't frame the pages (clickjacking).
const PAGE_CSP: &str = "default-src 'none'; img-src 'self'; media-src 'self'; \
    style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'";
/// Items are files straight from storage. Opened directly, they may load
/// nothing and run no scripts (`default-src 'none'`); `sandbox` also gives them
/// an opaque origin with no forms. Inline styles are allowed so SVGs render.
const ITEM_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";
/// Browsers and proxies never store pages, so going Back after signing out
/// fetches the page again and lands on the sign-in page.
const NO_STORE: &str = "no-store";
/// Items may be cached only by the browser (`private`, never by a shared
/// proxy) and must be revalidated before reuse (`no-cache`), which needs a live
/// session.
const ITEM_CACHE: &str = "private, no-cache";
/// Clears the cached items and anything else the browser stored for this site
/// (`"storage"`: e.g. local storage, which these pages don't use today). Not
/// `"cookies"`: that would clear cookies of the whole registrable domain,
/// signing users out of sibling sites. The session cookie is expired directly.
const SIGN_OUT_CLEAR_SITE_DATA: &str = r#""cache", "storage""#;
/// Limit on finding an item's file, opening it and reading its metadata; past
/// it the request gets a 503. Sending the body has no limit.
const ITEM_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const CLEAR_SITE_DATA: HeaderName = HeaderName::from_static("clear-site-data");
const CROSS_ORIGIN_RESOURCE_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-resource-policy");

/// All routes and their headers. Layers wrap from the outside in: the last
/// `.layer` added runs first on a request and last on its response.
///
/// - The sign-in pages sit outside the sign-in check: signed-out users must
///   reach them, and signing out must still expire the cookie when the session
///   already has.
/// - Checks use `route_layer`, so they only run for known routes: an unknown
///   path gets a plain 404 instead of a sign-in redirect.
/// - The cross-site check wraps the page and item headers, so its 403 carries
///   only the headers every response gets (nosniff, CORP and HSTS).
pub fn router(state: AppState) -> Router {
    let sign_in_required = middleware::from_fn_with_state(state.clone(), auth::require_sign_in);

    let sign_in_pages = Router::new()
        .route(routes::SIGN_IN, get(sign_in_form).post(sign_in))
        .route(routes::SIGN_OUT, post(sign_out));
    let album_pages = Router::new()
        .route(routes::HOME, get(list_albums))
        .route(routes::ALBUM, get(show_album))
        .route_layer(sign_in_required.clone());
    let pages = sign_in_pages
        .merge(album_pages)
        .layer(set_header(header::CONTENT_SECURITY_POLICY, PAGE_CSP))
        .layer(set_header(header::CACHE_CONTROL, NO_STORE));

    let items = Router::new()
        .route(routes::ITEM, get(serve_item))
        .route_layer(sign_in_required)
        .layer(set_header(header::CONTENT_SECURITY_POLICY, ITEM_CSP))
        .layer(set_header(header::CACHE_CONTROL, ITEM_CACHE));

    // Cross-site checks run before the sign-in check, so another site can't use
    // the user's session at all; `/healthz` stays open for monitoring.
    let site = pages
        .merge(items)
        .route_layer(middleware::from_fn(auth::reject_cross_site));

    Router::new()
        .route(routes::HEALTH, get(healthz))
        .merge(site)
        .layer(set_header(header::X_CONTENT_TYPE_OPTIONS, "nosniff"))
        // `reject_cross_site` lets through requests without Fetch Metadata;
        // browsers that support CORP still won't load our responses into other sites.
        .layer(set_header(CROSS_ORIGIN_RESOURCE_POLICY, "same-origin"))
        .layer(SetResponseHeaderLayer::overriding(
            header::STRICT_TRANSPORT_SECURITY,
            hsts(state.hsts_max_age),
        ))
        .with_state(state)
}

/// `Strict-Transport-Security` value; `None` (header omitted) when `max_age` is 0.
fn hsts(max_age: Duration) -> Option<HeaderValue> {
    let seconds = max_age.as_secs();
    (seconds > 0).then(|| HeaderValue::try_from(format!("max-age={seconds}")).expect("ASCII"))
}

fn set_header(name: HeaderName, value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::overriding(name, HeaderValue::from_static(value))
}

#[derive(Deserialize)]
struct SignInForm {
    username: String,
    password: String,
    /// The direct link that sent the user here (see [`routes::return_target`]).
    next: Option<String>,
}

#[derive(Deserialize)]
struct SignInQuery {
    next: Option<String>,
}

async fn sign_in_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SignInQuery>,
) -> Response {
    let next = routes::return_target(query.next.as_deref());
    if state.sessions.is_signed_in(&headers) {
        return Redirect::to(next).into_response();
    }
    html::sign_in_page(None, next).into_response()
}

async fn sign_in(
    State(state): State<AppState>,
    connection: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Form(form): Form<SignInForm>,
) -> Response {
    let next = routes::return_target(form.next.as_deref());
    let client = auth::client_ip(connection.as_ref().map(|Extension(info)| info));
    match state
        .credentials
        .verify(client, &form.username, &form.password)
    {
        Verdict::Valid => {}
        Verdict::Invalid => {
            let page = html::sign_in_page(Some("Wrong username or password."), next);
            return (StatusCode::UNAUTHORIZED, page).into_response();
        }
        Verdict::Throttled(wait) => {
            let page = html::sign_in_page(
                Some("Too many attempts. Wait a moment and try again."),
                next,
            );
            return auth::too_many_attempts(wait, page);
        }
    }
    // Always a fresh session; any session this browser already had is ended.
    state.sessions.sign_out(&headers);
    (
        [(header::SET_COOKIE, state.sessions.sign_in())],
        Redirect::to(next),
    )
        .into_response()
}

/// Ends the session server-side, expires the cookie and asks the browser to
/// wipe this site's cache and storage.
async fn sign_out(State(state): State<AppState>, headers: HeaderMap) -> Response {
    state.sessions.sign_out(&headers);
    let headers = [
        (header::SET_COOKIE, session::expired_cookie()),
        (
            CLEAR_SITE_DATA,
            HeaderValue::from_static(SIGN_OUT_CLEAR_SITE_DATA),
        ),
    ];
    (headers, Redirect::to(routes::SIGN_IN)).into_response()
}

async fn list_albums(State(state): State<AppState>) -> Markup {
    html::album_list(&state.library.snapshot())
}

async fn show_album(
    State(state): State<AppState>,
    Path(album_id): Path<String>,
) -> Result<Markup, StatusCode> {
    let snapshot = state.library.snapshot();
    let album = snapshot.album(&album_id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(html::album_detail(&snapshot, album))
}

/// 200 while storage is available, else 503 with a generic reason (this route
/// is public, so no paths or details). For monitoring and container health checks.
async fn healthz(State(state): State<AppState>) -> (StatusCode, String) {
    let storage = state.library.snapshot().storage;
    let status = if storage.is_available() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, storage.to_string())
}

async fn serve_item(
    State(state): State<AppState>,
    Path(item_id): Path<String>,
    request: Request,
) -> Result<impl IntoResponse, StatusCode> {
    let item_path = state
        .library
        .item_path(&item_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let open = async {
        let file = state.library.item_file(&item_path).await;
        let file = file.ok_or(StatusCode::NOT_FOUND)?;
        // ServeFile handles Content-Type, Range (video seeking) and conditional requests.
        let Ok(response) = ServeFile::new(file).oneshot(request).await;
        Ok(response)
    };
    // A hung mount must not hang the request too: finding the file, opening it
    // and reading its metadata all share one time limit.
    tokio::time::timeout(ITEM_OPEN_TIMEOUT, open)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{App, auth::Credentials, config, library::Library, link_ids::LinkIds};
    use axum::{body::Body, http::request::Builder};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;

    const OLD_BROWSER: &str = "Mozilla/5.0 (Macintosh) AppleWebKit/605 Safari/605";
    const ALICE: [u8; 4] = [192, 168, 1, 10];
    const MALLORY: [u8; 4] = [10, 0, 0, 66];

    /// URLs of the test album and its one item.
    struct Links {
        album: String,
        item: String,
    }

    /// A router over a temporary library with one album, `Trip`, holding one
    /// item, `a.svg`. The
    /// `TempDir` deletes the files when dropped, even if the test panics.
    async fn app() -> (tempfile::TempDir, Router, Links) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("storage");
        std::fs::create_dir_all(root.join("Trip")).unwrap();
        std::fs::write(root.join(".albums-root"), "").unwrap();
        std::fs::write(root.join("Trip/a.svg"), "<svg/>").unwrap();
        let link_ids = LinkIds::load_or_create(&dir.path().join("link.key")).unwrap();
        let links = Links {
            album: routes::album_url(&link_ids.album("Trip")),
            item: routes::item_url(&link_ids.item("Trip/a.svg")),
        };
        let storage = config::Storage {
            backend: config::StorageBackend::Local,
            root: root.clone(),
            pattern: "{album_name}/{file_name}".into(),
            marker: ".albums-root".into(),
            rescan_interval: Duration::from_secs(30),
            settle_time: Duration::ZERO,
            ignore: Vec::new(),
            link_key: dir.path().join("link.key"),
        };
        let library = Library::open(&storage, link_ids).await.unwrap();
        let router = router(Arc::new(App {
            library,
            credentials: Credentials::new("alice", "pw").unwrap(),
            sessions: Arc::new(session::Sessions::new(&config::Session {
                idle_timeout: Duration::from_secs(1800),
                max_lifetime: Duration::from_secs(43200),
            })),
            hsts_max_age: Duration::from_secs(60),
        }));
        (dir, router, links)
    }

    async fn send(app: &Router, request: Builder) -> Response {
        send_body(app, request, Body::empty()).await
    }

    async fn send_body(app: &Router, request: Builder, body: Body) -> Response {
        app.clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
    }

    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The connection a request arrives on, as the server sees it.
    fn from(client: [u8; 4], request: Builder) -> Builder {
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(client)), 50000);
        request.extension(ConnectInfo(address))
    }

    /// A browser opening `path` directly (typed, bookmarked or pasted).
    fn open(path: &str, cookie: Option<&str>) -> Builder {
        let request = Request::get(path)
            .header(header::ACCEPT, "text/html")
            .header("sec-fetch-site", "none")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-dest", "document");
        match cookie {
            Some(cookie) => request.header(header::COOKIE, cookie),
            None => request,
        }
    }

    /// A browser loading `path` as an `<img>`; `site` is the `Sec-Fetch-Site`
    /// value (`same-origin`, `cross-site`, ...).
    fn embed(path: &str, cookie: &str, site: &str) -> Builder {
        Request::get(path)
            .header(header::ACCEPT, "image/*")
            .header(header::COOKIE, cookie)
            .header("sec-fetch-site", site)
            .header("sec-fetch-mode", "no-cors")
            .header("sec-fetch-dest", "image")
    }

    fn basic(password: &str) -> String {
        format!("Basic {}", STANDARD.encode(format!("alice:{password}")))
    }

    fn header_of(response: &Response, name: impl header::AsHeaderName) -> &str {
        response
            .headers()
            .get(name)
            .map_or("", |v| v.to_str().unwrap())
    }

    async fn sign_in_as(app: &Router, client: [u8; 4], password: &str, next: &str) -> Response {
        let request = Request::post(routes::SIGN_IN)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("sec-fetch-site", "same-origin");
        let body = format!("username=alice&password={password}&next={next}");
        send_body(app, from(client, request), Body::from(body)).await
    }

    /// Signs in and returns the `Cookie` header value for the new session.
    async fn session_cookie(app: &Router) -> String {
        let response = sign_in_as(app, ALICE, "pw", "/").await;
        let set_cookie = header_of(&response, header::SET_COOKIE);
        set_cookie.split(';').next().unwrap().to_string()
    }

    async fn sign_out_with(app: &Router, cookie: &str, site: &str) -> Response {
        let request = Request::post(routes::SIGN_OUT)
            .header(header::COOKIE, cookie)
            .header("sec-fetch-site", site);
        send(app, request).await
    }

    #[tokio::test]
    async fn sign_in_sign_out_lifecycle() {
        let (_root, app, links) = app().await;

        // Signed out: pages redirect to the sign-in page; images get a bare 401 (no Basic
        // challenge, which would make browsers cache credentials).
        let response = send(&app, open("/", None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(header_of(&response, header::LOCATION), "/login?next=%2F");
        let response = send(&app, embed(&links.item, "", "same-origin")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());

        let response = sign_in_as(&app, MALLORY, "wrong", "/").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = sign_in_as(&app, ALICE, "pw", "/").await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let set_cookie = header_of(&response, header::SET_COOKIE);
        for attribute in [
            "__Host-albums_session=",
            "Path=/",
            "Secure",
            "HttpOnly",
            "SameSite=Strict",
        ] {
            assert!(
                set_cookie.contains(attribute),
                "{attribute} in {set_cookie}"
            );
        }
        let cookie = set_cookie.split(';').next().unwrap().to_string();

        let response = send(&app, open("/", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-store");
        assert_eq!(
            header_of(&response, header::STRICT_TRANSPORT_SECURITY),
            "max-age=60"
        );

        // A cross-site sign-out (CSRF) is refused and the session survives.
        let response = sign_out_with(&app, &cookie, "cross-site").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = send(&app, open("/", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = sign_out_with(&app, &cookie, "same-origin").await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(header_of(&response, header::LOCATION), routes::SIGN_IN);
        assert!(header_of(&response, header::SET_COOKIE).contains("Max-Age=0"));
        let clear_site_data = header_of(&response, CLEAR_SITE_DATA);
        assert!(clear_site_data.contains("\"cache\""));
        assert!(
            !clear_site_data.contains("cookies"),
            "must not sign users out of sibling sites"
        );

        // The old cookie is now worthless, even if a copy was kept.
        let response = send(&app, open("/", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let response = send(&app, embed(&links.item, &cookie, "same-origin")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// Failed attempts slow down only the client that made them, so nobody can
    /// delay the real user's sign-in; Basic auth attempts count too.
    #[tokio::test]
    async fn failed_attempts_throttle_only_their_client() {
        let (_root, app, links) = app().await;
        assert_eq!(
            sign_in_as(&app, MALLORY, "guess", "/").await.status(),
            StatusCode::UNAUTHORIZED
        );
        let response = sign_in_as(&app, MALLORY, "guess", "/").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(header_of(&response, header::RETRY_AFTER), "1");
        assert!(body_text(response).await.contains("Too many attempts"));

        // Alice isn't delayed by Mallory's failures.
        let response = sign_in_as(&app, ALICE, "pw", "/").await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        // Basic auth shares the same per-client throttle.
        let script = |password: &str| {
            let request = Request::get(&links.item)
                .header(header::USER_AGENT, "curl/8.7.1")
                .header(header::AUTHORIZATION, basic(password));
            from(MALLORY, request)
        };
        let response = send(&app, script("pw")).await;
        assert_eq!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "still waiting"
        );
    }

    #[tokio::test]
    async fn titles_are_fixed_and_links_use_ids() {
        let (_root, app, links) = app().await;
        let cookie = session_cookie(&app).await;
        for (path, title) in [(routes::HOME, "Albums"), (links.album.as_str(), "Album")] {
            let html = body_text(send(&app, open(path, Some(&cookie))).await).await;
            assert!(html.contains(&format!("<title>{title}</title>")), "{path}");
            assert!(html.contains(&format!("<h1>{title}</h1>")), "{path}");
            assert!(
                html.contains("Trip"),
                "the album name is rendered in the body"
            );
            assert!(
                !html.contains("/albums/Trip") && !html.contains("/media/Trip"),
                "links use link ids, not paths"
            );
        }
    }

    #[tokio::test]
    async fn direct_links_require_a_session_and_return_after_sign_in() {
        let (_root, app, links) = app().await;

        // Signed out: a direct link to an item goes to sign-in and remembers the link.
        let response = send(&app, open(&links.item, None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = header_of(&response, header::LOCATION).to_string();
        assert_eq!(location, routes::sign_in_url(&links.item));

        // The sign-in form carries `next`, and signing in lands on the link.
        let html = body_text(send(&app, open(&location, None)).await).await;
        assert!(html.contains(&format!(r#"value="{}""#, links.item)));
        let response = sign_in_as(&app, ALICE, "pw", &links.item.replace('/', "%2F")).await;
        assert_eq!(header_of(&response, header::LOCATION), links.item);

        // Signed in: the direct link opens.
        let cookie = session_cookie(&app).await;
        let response = send(&app, open(&links.item, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, CROSS_ORIGIN_RESOURCE_POLICY),
            "same-origin"
        );

        // `next` can't send the user to another site.
        for hostile in [
            "https%3A%2F%2Fevil.example",
            "%2F%2Fevil.example",
            "%2F%5Cevil.example",
        ] {
            let response = sign_in_as(&app, ALICE, "pw", hostile).await;
            assert_eq!(header_of(&response, header::LOCATION), "/", "{hostile}");
        }
    }

    #[tokio::test]
    async fn other_sites_cannot_use_the_session() {
        let (_root, app, links) = app().await;
        let cookie = session_cookie(&app).await;

        // Our own pages may load items; other sites may not, even with the cookie.
        let ours = send(&app, embed(&links.item, &cookie, "same-origin")).await;
        assert_eq!(ours.status(), StatusCode::OK);
        for site in ["cross-site", "same-site"] {
            let theirs = send(&app, embed(&links.item, &cookie, site)).await;
            assert_eq!(theirs.status(), StatusCode::FORBIDDEN, "{site}");
        }

        // A link from another site (e.g. an email) may navigate here; with
        // `SameSite=Strict` the browser sends no cookie (as this request
        // mimics), so the user is sent to sign in.
        let link = Request::get(&links.album)
            .header(header::ACCEPT, "text/html")
            .header("sec-fetch-site", "cross-site")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-dest", "document");
        let response = send(&app, link).await;
        assert_eq!(
            header_of(&response, header::LOCATION),
            routes::sign_in_url(&links.album)
        );

        // Older browsers without Fetch Metadata: a foreign `Origin` on a POST is
        // refused, and so is a browser POST with no `Origin` at all.
        let foreign = Request::post(routes::SIGN_OUT)
            .header(header::COOKIE, &cookie)
            .header(header::HOST, "albums.local")
            .header(header::ORIGIN, "https://evil.example");
        assert_eq!(send(&app, foreign).await.status(), StatusCode::FORBIDDEN);
        let anonymous = Request::post(routes::SIGN_OUT)
            .header(header::COOKIE, &cookie)
            .header(header::USER_AGENT, OLD_BROWSER);
        assert_eq!(send(&app, anonymous).await.status(), StatusCode::FORBIDDEN);
    }

    /// A browser that cached Basic credentials (e.g. from an earlier password
    /// prompt) sends them on every request; that must not keep it signed in
    /// after signing out. Browsers must hold a live session.
    #[tokio::test]
    async fn browsers_cannot_bypass_sign_out_with_cached_basic_credentials() {
        let (_root, app, links) = app().await;

        let modern = open("/", None).header(header::AUTHORIZATION, basic("pw"));
        let response = send(&app, modern).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "Fetch Metadata");

        let legacy_page = Request::get("/")
            .header(header::ACCEPT, "text/html")
            .header("upgrade-insecure-requests", "1")
            .header(header::AUTHORIZATION, basic("pw"));
        let response = send(&app, legacy_page).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "old browser page");

        // An old browser's `<img>` request has no Fetch Metadata and no
        // `Upgrade-Insecure-Requests`, only its `Mozilla/` user agent.
        let legacy_image = Request::get(&links.item)
            .header(header::ACCEPT, "image/*")
            .header(header::USER_AGENT, OLD_BROWSER)
            .header(header::AUTHORIZATION, basic("pw"));
        let response = send(&app, legacy_image).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "old browser image"
        );
        assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
    }

    #[tokio::test]
    async fn scripts_use_basic_auth() {
        let (_root, app, links) = app().await;
        let request = Request::get(&links.item)
            .header(header::USER_AGENT, "curl/8.7.1")
            .header(header::AUTHORIZATION, basic("pw"));
        assert_eq!(send(&app, request).await.status(), StatusCode::OK);

        // Without credentials they get a standard challenge (RFC 9110 §11), so
        // tools such as wget or `curl --anyauth` know to send them.
        let request = Request::get(&links.item).header(header::USER_AGENT, "Wget/1.24");
        let response = send(&app, request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(header_of(&response, header::WWW_AUTHENTICATE).starts_with("Basic realm="));
    }
}
