//! The router, handlers (sign-in, sign-out and health too) and the security
//! headers on every response.

use crate::auth::{self, Verdict};
use crate::{AppState, access_log, html, library, routes, session};
use axum::{
    Extension, Form, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::Markup;
use mime_guess::Mime;
use serde::Deserialize;
use std::net::SocketAddr;
use std::time::Duration;
use tower::ServiceExt;
use tower_http::compression::{CompressionLayer, CompressionLevel};
use tower_http::{services::ServeFile, set_header::SetResponseHeaderLayer};

/// Pages run no scripts and load nothing but images and videos from this
/// server, plus the inline styles of `html.rs`. Forms post only here, and other
/// sites can't frame the pages (clickjacking).
const PAGE_CSP: &str = "default-src 'none'; img-src 'self'; media-src 'self'; \
    style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'";
/// Originals are files straight from storage, and one policy fits every kind.
/// Opened directly, they may load only their own media: a video plays in a
/// player page the browser makes, which loads the same file in ranges. `sandbox`
/// runs no scripts, forms or popups, even in an SVG; `allow-same-origin` keeps
/// the file's real origin, without which the player's loads would go
/// cross-site, lose the session cookie and be refused. Safe only without
/// `allow-scripts`. Inline styles are allowed so SVGs render; other sites
/// can't frame originals.
const ORIGINAL_CSP: &str = "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; \
    frame-ancestors 'none'; sandbox allow-same-origin";
/// Browsers and proxies never store pages, so going Back after signing out
/// fetches the page again and lands on the sign-in page.
const NO_STORE: &str = "no-store";
/// Originals may be cached only by the browser (`private`, never by a shared
/// proxy) and must be revalidated before reuse (`no-cache`), which needs a live
/// session.
const ORIGINAL_CACHE: &str = "private, no-cache";
/// Clears the cached originals and anything else the browser stored for this site
/// (`"storage"`: e.g. local storage, which these pages don't use today). Not
/// `"cookies"`: that would clear cookies of the whole registrable domain,
/// signing users out of sibling sites. The session cookie is expired directly.
const SIGN_OUT_CLEAR_SITE_DATA: &str = r#""cache", "storage""#;
/// Limit on finding an item's original, opening it and reading its metadata; past
/// it the request gets a 503. Sending the body has no limit.
const ORIGINAL_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
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
/// - The cross-site check wraps the page and original headers, so its 403 carries
///   only the headers every response gets (nosniff, CORP and HSTS).
/// - Only pages are gzipped, at the fastest level: a large album page shrinks
///   about sevenfold for a few milliseconds. Originals are compressed already,
///   and gzipping a video would break the ranges it is played from. No secret
///   is ever in a page body, which is what a compression attack (BREACH) needs.
pub fn router(state: AppState) -> Router {
    let sign_in_required = middleware::from_fn_with_state(state.clone(), auth::require_sign_in);

    let sign_in_pages = Router::new()
        .route(routes::SIGN_IN, get(sign_in_form).post(sign_in))
        .route(routes::SIGN_OUT, post(sign_out));
    let album_pages = Router::new()
        .route(routes::HOME, get(list_albums))
        .route(routes::ALBUM, get(show_album))
        .route(routes::ITEM_PAGE, get(show_item))
        .route_layer(sign_in_required.clone());
    let pages = sign_in_pages
        .merge(album_pages)
        .layer(set_header(header::CONTENT_SECURITY_POLICY, PAGE_CSP))
        .layer(set_header(header::CACHE_CONTROL, NO_STORE))
        .layer(CompressionLayer::new().quality(CompressionLevel::Fastest));

    let originals = Router::new().route(routes::ORIGINAL, get(serve_original));
    // Thumbnails are small originals: the same sign-in and headers.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let originals = originals.route(routes::THUMBNAIL, get(serve_thumbnail));
    let originals = originals
        .route_layer(sign_in_required)
        .layer(set_header(header::CONTENT_SECURITY_POLICY, ORIGINAL_CSP))
        .layer(set_header(header::CACHE_CONTROL, ORIGINAL_CACHE));

    // Cross-site checks run before the sign-in check, so another site can't use
    // the user's session at all; `/healthz` stays open for monitoring.
    let site = pages
        .merge(originals)
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
        // Outermost, so it logs the final answer to every request.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            access_log::record,
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
            // A form error, not a 401: RFC 9110 §15.5.2 requires a challenge
            // with every 401, and browsers must never get one.
            let page = html::sign_in_page(Some("Wrong username or password."), next);
            return (StatusCode::UNPROCESSABLE_ENTITY, page).into_response();
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

/// An item's page: the same link id as its original at [`routes::ORIGINAL`].
async fn show_item(
    State(state): State<AppState>,
    Path(item_id): Path<String>,
) -> Result<Markup, StatusCode> {
    let snapshot = state.library.snapshot();
    let (album, item) = snapshot.item(&item_id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(html::item_page(&snapshot, album, item))
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

async fn serve_original(
    State(state): State<AppState>,
    Path(item_id): Path<String>,
    request: Request,
) -> Result<impl IntoResponse, StatusCode> {
    let item_path = state
        .library
        .item_path(&item_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let content_type = library::content_type(&item_path).ok_or(StatusCode::NOT_FOUND)?;
    let content_type: Mime = content_type.parse().expect("a valid media type");
    let open = async {
        let file = state.library.item_file(&item_path).await;
        let file = file.ok_or(StatusCode::NOT_FOUND)?;
        // ServeFile handles Range (video seeking) and conditional requests; the
        // Content-Type is the item's own. No sendfile: rustls encrypts in user
        // space, so zero-copy can't apply.
        let Ok(response) = ServeFile::new_with_mime(file, &content_type)
            .oneshot(request)
            .await;
        Ok(response)
    };
    // A hung mount must not hang the request too: finding the file, opening it
    // and reading its metadata all share one time limit.
    tokio::time::timeout(ORIGINAL_OPEN_TIMEOUT, open)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
}

/// An item's thumbnail: made the first time it is asked for and kept, then
/// served as a file, conditional requests and all. Items with none answer 404,
/// and their tiles keep their plain square.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn serve_thumbnail(
    State(state): State<AppState>,
    Path(item_id): Path<String>,
    request: Request,
) -> Result<impl IntoResponse, StatusCode> {
    use crate::thumbnail::{self, child::Original};
    let thumbnails = state.thumbnails.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let item_path = state
        .library
        .item_path(&item_id)
        .filter(|path| thumbnail::has_thumbnail(path))
        .ok_or(StatusCode::NOT_FOUND)?;
    // As for an original, a hung mount must not hang the request: finding
    // the file and opening it share one time limit.
    let open = async {
        let file = state.library.item_file(&item_path).await?;
        let open = tokio::task::spawn_blocking(move || Original::open(&file));
        open.await.ok()?.ok()
    };
    let original = tokio::time::timeout(ORIGINAL_OPEN_TIMEOUT, open)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let made = thumbnails
        .thumbnail(&item_id, original)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    let jpeg = mime_guess::mime::IMAGE_JPEG;
    let Ok(response) = ServeFile::new_with_mime(made, &jpeg).oneshot(request).await;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{App, auth::Credentials, config, library::Library, link_ids::LinkIds};
    use axum::{body::Body, http::request::Builder};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::collections::BTreeMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::{Arc, Mutex};

    /// Size of the test video, `b.mp4`, in bytes.
    const VIDEO_LEN: usize = 64;
    /// The test videos' contents.
    const VIDEO: [u8; VIDEO_LEN] = [0; VIDEO_LEN];
    const OLD_BROWSER: &str = "Mozilla/5.0 (Macintosh) AppleWebKit/605 Safari/605";
    const ALICE: [u8; 4] = [192, 168, 1, 10];
    const MALLORY: [u8; 4] = [10, 0, 0, 66];

    /// URLs of the test album and its items.
    struct Links {
        album: String,
        photo: String,
        video: String,
        /// The pages showing the photo and the video.
        photo_page: String,
        video_page: String,
        /// Link ids of the library, for items a test adds.
        ids: LinkIds,
    }

    /// A router over a temporary library with one album, `Trip`, holding the
    /// photo `a.svg` and the video `b.mp4`. The
    /// `TempDir` deletes the files when dropped, even if the test panics.
    async fn app() -> (tempfile::TempDir, Router, Links) {
        let (dir, router, links, _log) = app_with_log().await;
        (dir, router, links)
    }

    /// [`app`], with its access log kept in memory.
    async fn app_with_log() -> (tempfile::TempDir, Router, Links, access_log::Lines) {
        app_with_files(&[]).await
    }

    /// [`app_with_log`], with more items (`VIDEO_LEN` zero bytes each) at
    /// these item paths, in any album; their folders are created.
    async fn app_with_files(
        items: &[&str],
    ) -> (tempfile::TempDir, Router, Links, access_log::Lines) {
        let files: Vec<(&str, &[u8])> = [("Trip/a.svg", b"<svg/>".as_slice())]
            .into_iter()
            .chain(
                std::iter::once(&"Trip/b.mp4")
                    .chain(items)
                    .map(|item| (*item, &VIDEO[..])),
            )
            .collect();
        let (dir, router, ids, log) = app_over("{album_name}/{file_name}", &files).await;
        let (photo_id, video_id) = (ids.item("Trip/a.svg"), ids.item("Trip/b.mp4"));
        let links = Links {
            album: routes::album_url(&ids.album("Trip")).to_string(),
            photo: routes::original_url(&photo_id).to_string(),
            video: routes::original_url(&video_id).to_string(),
            photo_page: routes::item_page_url(&photo_id).to_string(),
            video_page: routes::item_page_url(&video_id).to_string(),
            ids,
        };
        (dir, router, links, log)
    }

    /// A router over a temporary library of `files` (item path and contents)
    /// laid out by `pattern`; their folders are created. Also returns the
    /// library's link ids and its access log, kept in memory.
    async fn app_over(
        pattern: &str,
        files: &[(&str, &[u8])],
    ) -> (tempfile::TempDir, Router, LinkIds, access_log::Lines) {
        app_over_with(pattern, files, true).await
    }

    /// [`app_over`], with or without thumbnails (as where none can be made).
    async fn app_over_with(
        pattern: &str,
        files: &[(&str, &[u8])],
        thumbnails: bool,
    ) -> (tempfile::TempDir, Router, LinkIds, access_log::Lines) {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage_with(dir.path(), pattern, files);
        let (state, link_ids, log) = state_over(&storage, dir.path(), thumbnails).await;
        (dir, router(state), link_ids, log)
    }

    /// Storage settings over `<dir>/storage`, a root holding `files` and the
    /// marker, laid out by `pattern`, with its link key in `dir`.
    fn storage_with(
        dir: &std::path::Path,
        pattern: &str,
        files: &[(&str, &[u8])],
    ) -> config::Storage {
        let root = dir.join("storage");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".albums-root"), "").unwrap();
        for (path, contents) in files {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, contents).unwrap();
        }
        config::Storage {
            backend: config::StorageBackend::Local,
            root,
            pattern: pattern.into(),
            marker: ".albums-root".into(),
            rescan_interval: Duration::from_secs(30),
            settle_time: Duration::ZERO,
            ignore: Vec::new(),
            link_key: dir.join("link.key"),
        }
    }

    /// The app over `storage` after its first scan, as a start builds it,
    /// with or without thumbnails, kept in `<dir>/thumbnails`.
    async fn state_over(
        storage: &config::Storage,
        dir: &std::path::Path,
        thumbnails: bool,
    ) -> (crate::AppState, LinkIds, access_log::Lines) {
        let link_ids = LinkIds::load_or_create(&storage.link_key).unwrap();
        let library = Library::open(storage, link_ids.clone()).await.unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        // Made by a child that writes back the video: tests choose it.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let thumbnails = thumbnails.then(|| {
            let child = crate::thumbnail::testing::fake("echo");
            crate::thumbnail::testing::service(child, Duration::from_secs(5), "test", dir).0
        });
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = (thumbnails, dir);
        let state = Arc::new(App {
            library,
            credentials: Credentials::new("alice", "pw").unwrap(),
            sessions: Arc::new(session::Sessions::new(&config::Session {
                idle_timeout: Duration::from_secs(1800),
                max_lifetime: Duration::from_secs(43200),
            })),
            hsts_max_age: Duration::from_secs(60),
            access_log: access_log::AccessLog::Memory(log.clone()),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            thumbnails,
        });
        (state, link_ids, log)
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
        with_cookie(request, cookie)
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

    /// A browser's media player loading `path`: an inline `<video>`, or the
    /// player page of a video opened in its own tab.
    fn play(path: &str, cookie: Option<&str>) -> Builder {
        let request = Request::get(path)
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-mode", "no-cors")
            .header("sec-fetch-dest", "video");
        with_cookie(request, cookie)
    }

    fn with_cookie(request: Builder, cookie: Option<&str>) -> Builder {
        match cookie {
            Some(cookie) => request.header(header::COOKIE, cookie),
            None => request,
        }
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

        // Signed out: pages redirect to the sign-in page; images get a 403 (no Basic
        // challenge, which would make browsers cache credentials).
        let response = send(&app, open("/", None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(header_of(&response, header::LOCATION), "/login?next=%2F");
        let response = send(&app, embed(&links.photo, "", "same-origin")).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());

        let response = sign_in_as(&app, MALLORY, "wrong", "/").await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

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
        let response = send(&app, embed(&links.photo, &cookie, "same-origin")).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// Failed attempts slow down only the client that made them, so nobody can
    /// delay the real user's sign-in; Basic auth attempts count too.
    #[tokio::test]
    async fn failed_attempts_throttle_only_their_client() {
        let (_root, app, links) = app().await;
        assert_eq!(
            sign_in_as(&app, MALLORY, "guess", "/").await.status(),
            StatusCode::UNPROCESSABLE_ENTITY
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
            let request = Request::get(&links.photo)
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
        assert_eq!(header_of(&response, header::RETRY_AFTER), "1");
        assert_eq!(body_text(response).await, "Too many attempts");
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
        let response = send(&app, open(&links.photo, None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = header_of(&response, header::LOCATION).to_string();
        assert_eq!(location, routes::sign_in_url(&links.photo));

        // The sign-in form carries `next`, and signing in lands on the link.
        let html = body_text(send(&app, open(&location, None)).await).await;
        assert!(html.contains(&format!(r#"value="{}""#, links.photo)));
        let response = sign_in_as(&app, ALICE, "pw", &links.photo.replace('/', "%2F")).await;
        assert_eq!(header_of(&response, header::LOCATION), links.photo);

        // Signed in: the direct link opens.
        let cookie = session_cookie(&app).await;
        let response = send(&app, open(&links.photo, Some(&cookie))).await;
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

    /// A photo tile shows its original, loaded lazily; a video tile shows its
    /// thumbnail, lazily too, under a play glyph, but no `<video>`, so the
    /// video isn't fetched until it is opened.
    #[tokio::test]
    async fn album_pages_fetch_media_only_when_needed() {
        let (_root, app, links) = app().await;
        let video_id = links.ids.item("Trip/b.mp4");
        let cookie = session_cookie(&app).await;
        let html = body_text(send(&app, open(&links.album, Some(&cookie))).await).await;
        let photo = format!(
            r#"<a class="tile" href="{}" title="a.svg"><img loading="lazy" src="{}" alt=""><span class="caption">a.svg</span></a>"#,
            links.photo_page, links.photo
        );
        let video = format!(
            r#"<a class="tile" href="{}" title="b.mp4"><img loading="lazy" src="{}" alt=""><span class="play" aria-hidden="true">▶{}</span><span class="caption">b.mp4</span></a>"#,
            links.video_page,
            routes::thumbnail_url(&video_id),
            '\u{FE0E}'
        );
        assert!(html.contains(&photo), "{html}");
        assert!(html.contains(&video), "{html}");
        assert!(!html.contains("<video"), "{html}");
    }

    /// A tile's tooltip is the file name and the item's metadata, which its
    /// page shows as chips; names are escaped wherever they appear.
    #[tokio::test]
    async fn tiles_and_item_pages_show_item_metadata() {
        let photo = r#"Lake/001_a&b "x" <y>.jpg"#;
        let files = [(photo, &b"x"[..]), ("Lake/002_clip.mp4", &VIDEO[..])];
        let (_dir, app, ids, _log) = app_over("{album_name}/{seq}_{file_name}", &files).await;
        let cookie = session_cookie(&app).await;
        let (photo_id, video_id) = (ids.item(photo), ids.item("Lake/002_clip.mp4"));
        let album = routes::album_url(&ids.album("Lake")).to_string();
        let html = body_text(send(&app, open(&album, Some(&cookie))).await).await;
        let name = "001_a&amp;b &quot;x&quot; &lt;y&gt;.jpg";
        let tiles = [
            format!(
                r#"<a class="tile" href="{}" title="{name} · seq: 001"><img loading="lazy" src="{}" alt=""><span class="caption">{name}</span></a>"#,
                routes::item_page_url(&photo_id),
                routes::original_url(&photo_id)
            ),
            format!(
                r#"<a class="tile" href="{}" title="002_clip.mp4 · seq: 002"><img loading="lazy" src="{}" alt=""><span class="play" aria-hidden="true">▶{}</span><span class="caption">002_clip.mp4</span></a>"#,
                routes::item_page_url(&video_id),
                routes::thumbnail_url(&video_id),
                '\u{FE0E}'
            ),
        ];
        for tile in tiles {
            assert!(html.contains(&tile), "{tile}\n{html}");
        }

        let page = routes::item_page_url(&photo_id).to_string();
        let html = body_text(send(&app, open(&page, Some(&cookie))).await).await;
        for shown in [
            format!(r#"<p class="name">{name}</p>"#),
            r#"<div class="chips"><span class="chip">seq: 001</span></div>"#.to_string(),
        ] {
            assert!(html.contains(&shown), "{shown}\n{html}");
        }
    }

    /// An album's cover is the picture of its first item that has one, loaded
    /// lazily: a photo's original or a video's thumbnail. A WebM video has no
    /// thumbnail, so it is passed over; an album of those gets an empty cover.
    #[tokio::test]
    async fn album_covers_show_the_first_picture() {
        let files = ["Clips/c.mp4", "Mix/0.webm", "Mix/1.svg", "Web/w.webm"];
        let (_dir, app, links, _log) = app_with_files(&files).await;
        let cookie = session_cookie(&app).await;
        let html = body_text(send(&app, open(routes::HOME, Some(&cookie))).await).await;
        let cover = |album: &str, src: Option<String>| {
            let img = src.map_or(String::new(), |src| {
                format!(r#"<img loading="lazy" src="{src}" alt="">"#)
            });
            let href = routes::album_url(&links.ids.album(album)).to_string();
            format!(r#"<a class="album" href="{href}"><div class="cover">{img}</div>"#)
        };
        let original = |item| Some(routes::original_url(&links.ids.item(item)).to_string());
        let thumbnail = |item| Some(routes::thumbnail_url(&links.ids.item(item)).to_string());
        for expected in [
            cover("Trip", original("Trip/a.svg")),
            cover("Clips", thumbnail("Clips/c.mp4")),
            cover("Mix", original("Mix/1.svg")),
            cover("Web", None),
        ] {
            assert!(html.contains(&expected), "{expected}\n{html}");
        }
    }

    /// What `ORIGINAL_CSP` allows and forbids (its doc says why), and that every
    /// response on an original's URL carries it, whatever the file. The ranges are
    /// the loads of a video's player page, as logged from Safari on iOS.
    #[tokio::test]
    async fn originals_share_one_policy() {
        let policy: BTreeMap<&str, Vec<&str>> = ORIGINAL_CSP
            .split(';')
            .filter_map(|directive| {
                let mut words = directive.split_whitespace();
                Some((words.next()?, words.collect()))
            })
            .collect();
        let expected = BTreeMap::from([
            ("default-src", vec!["'none'"]),
            ("media-src", vec!["'self'"]),
            ("style-src", vec!["'unsafe-inline'"]),
            ("frame-ancestors", vec!["'none'"]),
            ("sandbox", vec!["allow-same-origin"]),
        ]);
        assert_eq!(policy, expected);

        let (_root, app, links) = app().await;
        let cookie = session_cookie(&app).await;
        let head = || Request::head(&links.video).header(header::COOKIE, &cookie);
        let etag = header_of(&send(&app, head()).await, header::ETAG).to_string();
        let range = |range: String| play(&links.video, Some(&cookie)).header(header::RANGE, range);
        let cases = [
            ("photo", open(&links.photo, Some(&cookie)), StatusCode::OK),
            ("video", open(&links.video, Some(&cookie)), StatusCode::OK),
            (
                "first bytes",
                range("bytes=0-1".into()),
                StatusCode::PARTIAL_CONTENT,
            ),
            (
                "the rest",
                range(format!("bytes={}-", VIDEO_LEN / 2)),
                StatusCode::PARTIAL_CONTENT,
            ),
            (
                "past the end",
                range(format!("bytes={VIDEO_LEN}-")),
                StatusCode::RANGE_NOT_SATISFIABLE,
            ),
            ("head", head(), StatusCode::OK),
            (
                "cached",
                open(&links.video, Some(&cookie)).header(header::IF_NONE_MATCH, etag),
                StatusCode::NOT_MODIFIED,
            ),
            (
                "unknown",
                open(&routes::original_url("unknown").to_string(), Some(&cookie)),
                StatusCode::NOT_FOUND,
            ),
            (
                "signed out",
                open(&links.video, None),
                StatusCode::SEE_OTHER,
            ),
            (
                "signed-out player",
                play(&links.video, None),
                StatusCode::FORBIDDEN,
            ),
        ];
        for (case, request, status) in cases {
            let response = send(&app, request).await;
            assert_eq!(response.status(), status, "{case}");
            let csp = header_of(&response, header::CONTENT_SECURITY_POLICY);
            assert_eq!(csp, ORIGINAL_CSP, "{case}");
        }
    }

    /// Originals are served with a type every browser plays: QuickTime and M4V
    /// files are MP4 inside, and Chromium refuses to play `video/quicktime`.
    #[tokio::test]
    async fn originals_are_served_with_a_type_browsers_play() {
        let (_dir, app, links, _log) = app_with_files(&["Trip/c.MOV", "Trip/d.m4v"]).await;
        let cookie = session_cookie(&app).await;
        let original = |path: &str| routes::original_url(&links.ids.item(path)).to_string();
        for (url, content_type) in [
            (links.photo.clone(), "image/svg+xml"),
            (links.video.clone(), "video/mp4"),
            (original("Trip/c.MOV"), "video/mp4"),
            (original("Trip/d.m4v"), "video/mp4"),
        ] {
            let response = send(&app, open(&url, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::OK, "{url}");
            assert_eq!(
                header_of(&response, header::CONTENT_TYPE),
                content_type,
                "{url}"
            );
        }
    }

    /// The videos of the storage-change tests, by path.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    const VIDEOS: [&str; 3] = ["Trip/a.mp4", "Trip/b.mp4", "Home/c.mp4"];

    /// Starts over `storage` with the [`VIDEOS`] in its root, makes each one's
    /// thumbnail, then changes the files or settings with `change` and starts
    /// again, sweeping as the first scan of a start does. Gives the link ids
    /// before the change and the item folders left in the cache.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn restarted_after(
        change: impl FnOnce(&mut config::Storage, &std::path::Path),
    ) -> (tempfile::TempDir, crate::AppState, LinkIds, Vec<String>) {
        use crate::thumbnail::testing::{JPEG, cache, open};
        let dir = tempfile::tempdir().unwrap();
        let files = VIDEOS.map(|path| (path, JPEG));
        let mut storage = storage_with(dir.path(), "{album_name}/{file_name}", &files);
        let (before, ids, _) = state_over(&storage, dir.path(), true).await;
        let thumbnails = before.thumbnails.as_ref().unwrap();
        for path in VIDEOS {
            let original = open(&storage.root.join(path));
            assert!(
                thumbnails
                    .thumbnail(&ids.item(path), original)
                    .await
                    .is_some()
            );
        }
        change(&mut storage, dir.path());
        let (after, _, _) = state_over(&storage, dir.path(), true).await;
        crate::keep_thumbnails_of(after.clone(), after.library.snapshot()).await;
        let mut left: Vec<_> = std::fs::read_dir(cache(dir.path()))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        (dir, after, ids, left)
    }

    /// Thumbnails follow the library through changes to its files and
    /// settings: items whose path is the same keep theirs (a root moved, a
    /// pattern edited); items no longer there, or there under another path
    /// (moved, renamed, now ignored, or laid out at another depth), lose
    /// theirs after the first scan.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn thumbnails_follow_changes_to_the_storage() {
        type Change = fn(&mut config::Storage, &std::path::Path);
        let cases: [(&str, Change, &[&str]); 6] = [
            ("nothing changed", |_, _| {}, &VIDEOS),
            (
                "the root moved",
                |storage, dir| {
                    std::fs::rename(&storage.root, dir.join("moved")).unwrap();
                    storage.root = dir.join("moved");
                },
                &VIDEOS,
            ),
            (
                "the pattern edited, the same paths",
                |storage, _| storage.pattern = "{album_name}/{title}.mp4".into(),
                &VIDEOS,
            ),
            (
                "the pattern of another depth",
                |storage, _| storage.pattern = "{year}/{album_name}/{file_name}".into(),
                &[],
            ),
            (
                "an album ignored",
                |storage, _| storage.ignore = vec!["Home".into()],
                &["Trip/a.mp4", "Trip/b.mp4"],
            ),
            (
                "videos moved and renamed",
                |storage, _| {
                    let root = &storage.root;
                    std::fs::rename(root.join("Trip/a.mp4"), root.join("Trip/d.mp4")).unwrap();
                    std::fs::rename(root.join("Trip/b.mp4"), root.join("Home/b.mp4")).unwrap();
                },
                &["Home/c.mp4"],
            ),
        ];
        for (case, change, kept) in cases {
            let (_dir, _, ids, left) = restarted_after(change).await;
            let mut expected: Vec<_> = kept.iter().map(|path| ids.item(path)).collect();
            expected.sort();
            assert_eq!(left, expected, "{case}");
        }
    }

    /// A new link key gives every item a new link id: the thumbnails kept
    /// under the old ids are removed after the first scan, and each item's
    /// is made again under its new id, never taken from an old folder.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn a_new_link_key_makes_thumbnails_anew() {
        use crate::thumbnail::testing::{cache, open};
        let (dir, after, _, left) =
            restarted_after(|storage, dir| storage.link_key = dir.join("new.key")).await;
        assert_eq!(left, [] as [String; 0]);
        let new_ids = LinkIds::load_or_create(&dir.path().join("new.key")).unwrap();
        let thumbnails = after.thumbnails.as_ref().unwrap();
        let root = after.library.snapshot();
        assert_eq!(root.item_ids().count(), VIDEOS.len());
        for path in VIDEOS {
            let id = new_ids.item(path);
            assert!(root.item(&id).is_some(), "{path}");
            let original = open(&dir.path().join("storage").join(path));
            let made = thumbnails.thumbnail(&id, original).await.unwrap();
            assert!(made.starts_with(cache(dir.path()).join(&id)), "{made:?}");
        }
    }

    /// A video's thumbnail is a JPEG for signed-in users, made the first time
    /// and kept: asked again unchanged, it costs a 304. A photo, a WebM video
    /// or a video with no thumbnail answers 404, so its tile keeps its plain
    /// square and play glyph.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn video_tiles_load_their_thumbnails() {
        let files = [
            ("Trip/a.svg", &b"<svg/>"[..]),
            ("Trip/b.mp4", crate::thumbnail::testing::JPEG),
            ("Trip/c.mp4", &VIDEO[..]),
            // Has a JPEG in it too: only the kind of file keeps it from one.
            ("Trip/d.webm", crate::thumbnail::testing::JPEG),
        ];
        let (_dir, app, ids, _log) = app_over("{album_name}/{file_name}", &files).await;
        let thumbnail = |path: &str| routes::thumbnail_url(&ids.item(path)).to_string();

        let response = send(&app, open(&thumbnail("Trip/b.mp4"), None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let cookie = session_cookie(&app).await;
        let response = send(&app, open(&thumbnail("Trip/b.mp4"), Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_of(&response, header::CONTENT_TYPE), "image/jpeg");
        let modified = header_of(&response, header::LAST_MODIFIED).to_string();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], crate::thumbnail::testing::JPEG);
        let again = open(&thumbnail("Trip/b.mp4"), Some(&cookie))
            .header(header::IF_MODIFIED_SINCE, modified);
        assert_eq!(send(&app, again).await.status(), StatusCode::NOT_MODIFIED);

        for none in ["Trip/a.svg", "Trip/c.mp4", "Trip/d.webm"] {
            let response = send(&app, open(&thumbnail(none), Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{none}");
        }

        // Where none can be made, a video's tile keeps its plain square too.
        let (_dir, off, ids, _log) = app_over_with("{album_name}/{file_name}", &files, false).await;
        let cookie = session_cookie(&off).await;
        let url = routes::thumbnail_url(&ids.item("Trip/b.mp4")).to_string();
        let response = send(&off, open(&url, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// Pages are gzipped for browsers that accept it (a large album page
    /// shrinks about sevenfold). Originals never are: they are compressed
    /// already, and videos are fetched in ranges. A gzipped page's size isn't
    /// known up front, so its log line shows `-`.
    #[tokio::test]
    async fn pages_are_gzipped_but_originals_never() {
        use std::io::Read;
        let (_root, app, links, log) = app_with_log().await;
        let cookie = session_cookie(&app).await;
        let gzip = |request: Builder| request.header(header::ACCEPT_ENCODING, "gzip");
        for (page, cookie) in [
            (links.album.as_str(), Some(cookie.as_str())),
            (routes::SIGN_IN, None),
        ] {
            let plain = body_text(send(&app, open(page, cookie)).await).await;
            let response = send(&app, gzip(open(page, cookie))).await;
            assert_eq!(response.status(), StatusCode::OK, "{page}");
            assert_eq!(
                header_of(&response, header::CONTENT_ENCODING),
                "gzip",
                "{page}"
            );
            assert_eq!(
                header_of(&response, header::VARY),
                "accept-encoding",
                "{page}"
            );
            let line = log.lock().unwrap().last().cloned().unwrap();
            assert!(line.contains(" 200 - "), "{line}");
            let gzipped = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let mut html = String::new();
            flate2::read::GzDecoder::new(&gzipped[..])
                .read_to_string(&mut html)
                .unwrap();
            assert_eq!(html, plain, "{page}");
        }

        let range = play(&links.video, Some(&cookie)).header(header::RANGE, "bytes=0-1");
        for (request, status) in [
            (open(&links.photo, Some(&cookie)), StatusCode::OK),
            (open(&links.video, Some(&cookie)), StatusCode::OK),
            (range, StatusCode::PARTIAL_CONTENT),
        ] {
            let response = send(&app, gzip(request)).await;
            assert_eq!(response.status(), status);
            assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        }
    }

    #[tokio::test]
    async fn other_sites_cannot_use_the_session() {
        let (_root, app, links) = app().await;
        let cookie = session_cookie(&app).await;

        // Our own pages may load items; other sites may not, even with the cookie.
        let ours = send(&app, embed(&links.photo, &cookie, "same-origin")).await;
        assert_eq!(ours.status(), StatusCode::OK);
        for site in ["cross-site", "same-site"] {
            let theirs = send(&app, embed(&links.photo, &cookie, site)).await;
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
        // A tool sends neither either, but can't be tricked into posting.
        let tool = Request::post(routes::SIGN_OUT)
            .header(header::COOKIE, &cookie)
            .header(header::USER_AGENT, "curl/8.7.1");
        assert_eq!(send(&app, tool).await.status(), StatusCode::SEE_OTHER);
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
        let legacy_image = Request::get(&links.photo)
            .header(header::ACCEPT, "image/*")
            .header(header::USER_AGENT, OLD_BROWSER)
            .header(header::AUTHORIZATION, basic("pw"));
        let response = send(&app, legacy_image).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "old browser image"
        );
        assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
    }

    #[tokio::test]
    async fn scripts_use_basic_auth() {
        let (_root, app, links) = app().await;
        let request = Request::get(&links.photo)
            .header(header::USER_AGENT, "curl/8.7.1")
            .header(header::AUTHORIZATION, basic("pw"));
        assert_eq!(send(&app, request).await.status(), StatusCode::OK);

        // Without credentials they get a standard challenge (RFC 9110 §11), so
        // tools such as wget or `curl --anyauth` know to send them.
        let request = Request::get(&links.photo).header(header::USER_AGENT, "Wget/1.24");
        let response = send(&app, request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(header_of(&response, header::WWW_AUTHENTICATE).starts_with("Basic realm="));
    }

    /// Signed out, by kind of client. RFC 9110 §15.5.2: a 401 must carry a
    /// challenge, so only tools and desktop players, which can answer one, get
    /// a 401. Browsers must sign in on the page instead: they get the sign-in
    /// redirect, or a 403 (§15.5.4) when loading an item, and a wrong password
    /// there is a form error.
    #[tokio::test]
    async fn only_clients_that_can_answer_a_challenge_get_a_401() {
        let (_root, app, links) = app().await;
        let old_browser_image = Request::get(&links.photo)
            .header(header::ACCEPT, "image/*")
            .header(header::USER_AGENT, OLD_BROWSER);
        let tool = |path: &str| Request::get(path).header(header::USER_AGENT, "Wget/1.24");
        let sign_in_then = |path: &str| format!("/login?next={}", path.replace('/', "%2F"));
        let cases = [
            (
                "browser opening a page",
                open("/", None),
                StatusCode::SEE_OTHER,
                sign_in_then("/"),
            ),
            (
                "browser opening a direct link",
                open(&links.video, None),
                StatusCode::SEE_OTHER,
                sign_in_then(&links.video),
            ),
            (
                "browser loading an image",
                embed(&links.photo, "", "same-origin"),
                StatusCode::FORBIDDEN,
                String::new(),
            ),
            (
                "browser playing a video",
                play(&links.video, None),
                StatusCode::FORBIDDEN,
                String::new(),
            ),
            (
                "old browser loading an image",
                old_browser_image,
                StatusCode::FORBIDDEN,
                String::new(),
            ),
            (
                "browser known only by Upgrade-Insecure-Requests",
                Request::get(&links.photo).header("upgrade-insecure-requests", "1"),
                StatusCode::FORBIDDEN,
                String::new(),
            ),
            (
                "tool",
                tool(&links.photo),
                StatusCode::UNAUTHORIZED,
                String::new(),
            ),
            (
                "tool asking for a page",
                tool("/").header(header::ACCEPT, "text/html"),
                StatusCode::SEE_OTHER,
                sign_in_then("/"),
            ),
            (
                "tool streaming part of an item",
                tool(&links.video).header(header::RANGE, "bytes=0-1"),
                StatusCode::UNAUTHORIZED,
                String::new(),
            ),
            (
                "tool with a wrong password",
                tool(&links.photo).header(header::AUTHORIZATION, basic("wrong")),
                StatusCode::UNAUTHORIZED,
                String::new(),
            ),
        ];
        for (case, request, status, location) in cases {
            let response = send(&app, request).await;
            assert_eq!(response.status(), status, "{case}");
            let challenge = if status == StatusCode::UNAUTHORIZED {
                r#"Basic realm="albums", charset="UTF-8""#
            } else {
                ""
            };
            assert_eq!(
                header_of(&response, header::WWW_AUTHENTICATE),
                challenge,
                "{case}"
            );
            assert_eq!(header_of(&response, header::LOCATION), location, "{case}");
            // Not the cross-site check's 403.
            if status == StatusCode::FORBIDDEN {
                assert_eq!(body_text(response).await, "Sign in first", "{case}");
            }
        }

        // A wrong password on the sign-in form shows the form again, still
        // set to return to the album afterwards.
        let next = links.album.replace('/', "%2F");
        let response = sign_in_as(&app, MALLORY, "wrong", &next).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(header_of(&response, header::WWW_AUTHENTICATE), "");
        assert_eq!(header_of(&response, header::LOCATION), "");
        let page = body_text(response).await;
        assert!(page.contains("Wrong username or password."), "{page}");
        let form = format!(r#"action="{}""#, routes::SIGN_IN);
        assert!(page.contains(&form), "{page}");
        let return_to = format!(r#"value="{}""#, links.album);
        assert!(page.contains(&return_to), "{page}");
    }

    /// One line per request in the Combined Log Format, with the client's
    /// address and the final status. The query string is left out (it could
    /// one day carry a token), and a user agent can't forge lines or fields.
    #[tokio::test]
    async fn requests_are_logged_in_combined_log_format() {
        let (_root, app, links, log) = app_with_log().await;
        let cookie = session_cookie(&app).await;
        log.lock().unwrap().clear();

        let player = play(&links.video, Some(&cookie))
            .header(header::RANGE, "bytes=0-1")
            .header(
                header::USER_AGENT,
                HeaderValue::from_bytes(b"Player \"x\"\t\xff\\").unwrap(),
            );
        send(&app, from(ALICE, player)).await;
        let tool = Request::get(format!("{}?next=/secret", links.album))
            .header(header::USER_AGENT, "Wget/1.24");
        send(&app, from(MALLORY, tool)).await;
        send(&app, from(ALICE, open("/", None))).await;
        let head = Request::head(&links.photo)
            .header(header::COOKIE, &cookie)
            .header(header::USER_AGENT, "curl/8.7.1");
        send(&app, from(ALICE, head)).await;
        send(&app, from(ALICE, Request::get(routes::HEALTH))).await;

        let expected = [
            format!(
                r#"192.168.1.10 - - [TIME] "GET {} HTTP/1.1" 206 2 "-" "Player \x22x\x22\x09\xff\x5c""#,
                links.video
            ),
            format!(
                r#"10.0.0.66 - - [TIME] "GET {} HTTP/1.1" 401 12 "-" "Wget/1.24""#,
                links.album
            ),
            r#"192.168.1.10 - - [TIME] "GET / HTTP/1.1" 303 0 "-" "-""#.to_string(),
            // A HEAD answer sends no body, whatever its `Content-Length` says.
            format!(
                r#"192.168.1.10 - - [TIME] "HEAD {} HTTP/1.1" 200 0 "-" "curl/8.7.1""#,
                links.photo
            ),
        ];
        let lines = log.lock().unwrap().clone();
        // `/healthz` isn't logged: the container healthcheck would flood the log.
        assert_eq!(lines.len(), expected.len(), "{lines:#?}");
        for (line, expected) in lines.iter().zip(&expected) {
            // `15/Nov/2026:20:19:04 +0000`: the time changes, its shape doesn't.
            let start = line.find(" [").unwrap() + 2;
            let time = line.get(start..start + 26).unwrap_or_default();
            let shape: String = time
                .chars()
                .map(|c| {
                    if c.is_ascii_digit() {
                        '9'
                    } else if c.is_ascii_alphabetic() {
                        'a'
                    } else {
                        c
                    }
                })
                .collect();
            assert_eq!(shape, "99/aaa/9999:99:99:99 +9999", "{line}");
            assert!(time.ends_with(" +0000"), "UTC: {line}");
            assert_eq!(&line.replacen(time, "TIME", 1), expected);
        }
    }

    /// A tile opens the item's own page in the same tab: the photo or video
    /// embedded, the way back to the album and the original file.
    #[tokio::test]
    async fn tiles_open_item_pages() {
        let (_root, app, links) = app().await;
        let cookie = session_cookie(&app).await;
        let album = body_text(send(&app, open(&links.album, Some(&cookie))).await).await;
        for page in [&links.photo_page, &links.video_page] {
            let tile = format!(r#"<a class="tile" href="{page}""#);
            assert!(album.contains(&tile), "{album}");
        }
        assert!(!album.contains("target="), "{album}");

        for (page, title, media, original) in [
            (
                &links.photo_page,
                "Photo",
                format!(r#"<img src="{}" alt="a.svg">"#, links.photo),
                &links.photo,
            ),
            (
                &links.video_page,
                "Video",
                format!(
                    r#"<video controls playsinline preload="metadata"><source src="{}">"#,
                    links.video
                ),
                &links.video,
            ),
        ] {
            let response = send(&app, open(page, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::OK, "{page}");
            let csp = header_of(&response, header::CONTENT_SECURITY_POLICY);
            assert_eq!(csp, PAGE_CSP, "{page}");
            assert_eq!(
                header_of(&response, header::CACHE_CONTROL),
                NO_STORE,
                "{page}"
            );
            let html = body_text(response).await;
            assert!(html.contains(&format!("<h1>{title}</h1>")), "{html}");
            assert!(html.contains(&media), "{html}");
            let back = format!(r#"<a href="{}">← Trip</a>"#, links.album);
            assert!(html.contains(&back), "{html}");
            let open_original =
                format!(r#"<a href="{original}" target="_blank">Open original</a>"#);
            assert!(html.contains(&open_original), "{html}");
        }
    }

    /// Album pages take album link ids, and nothing else: among several
    /// albums, each opens by its own id.
    #[tokio::test]
    async fn album_pages_take_only_album_link_ids() {
        let (_dir, app, links, _log) =
            app_with_files(&["Clips/c.mp4", "Lake/d.mp4", "Zoo/e.mp4"]).await;
        let cookie = session_cookie(&app).await;
        for name in ["Clips", "Lake", "Trip", "Zoo"] {
            let page = routes::album_url(&links.ids.album(name)).to_string();
            let response = send(&app, open(&page, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::OK, "{page}");
            let html = body_text(response).await;
            let shown = format!(r#"<p class="name">{name}</p>"#);
            assert!(html.contains(&shown), "{html}");
        }
        let photo_id = links.photo.trim_start_matches("/media/");
        for page in
            [routes::album_url("unknown"), routes::album_url(photo_id)].map(|page| page.to_string())
        {
            let response = send(&app, open(&page, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{page}");
        }
    }

    /// Item pages take the same link ids as items, and nothing else.
    #[tokio::test]
    async fn item_pages_take_only_item_link_ids() {
        let (_root, app, links) = app().await;
        let cookie = session_cookie(&app).await;
        let album_id = links.album.trim_start_matches("/albums/");
        for page in [
            routes::item_page_url("unknown"),
            routes::item_page_url(album_id),
        ]
        .map(|page| page.to_string())
        {
            let response = send(&app, open(&page, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{page}");
        }

        // Signed out: sign in, then back to the page.
        let response = send(&app, open(&links.photo_page, None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = header_of(&response, header::LOCATION);
        assert_eq!(location, routes::sign_in_url(&links.photo_page));
    }
}
