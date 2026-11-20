//! rust-albums: a minimal HTTPS-only album viewer over a local folder tree.
//! Plain HTTP sent to the HTTPS port is answered by `plain_http`.
//!
//! Settings come from the local config file (`./albums.toml`, or `$ALBUMS_CONFIG`),
//! created from `albums.default.toml` on first run; a few `ALBUMS_*` variables
//! override it in containers. Credentials come from the environment only:
//! `AUTH_USER`, `AUTH_PASS`. `rust-albums --healthcheck` checks a running server;
//! `rust-albums --thumbnail` is the child process the server decodes videos in.

#![forbid(unsafe_code)]

mod access_log;
mod auth;
mod config;
mod crypto;
mod healthcheck;
mod html;
mod library;
mod link_ids;
mod natural_order;
mod pattern;
mod plain_http;
mod routes;
mod session;
mod thumbnail;
mod tls;
mod web;

use axum_server::{Handle, accept::NoDelayAcceptor, tls_rustls::RustlsAcceptor};
use std::{env, error::Error, net::SocketAddr, sync::Arc, time::Duration};

pub type AppState = Arc<App>;

pub struct App {
    pub library: Arc<library::Library>,
    pub credentials: auth::Credentials,
    pub sessions: Arc<session::Sessions>,
    pub hsts_max_age: Duration,
    pub access_log: access_log::AccessLog,
    /// Video thumbnails, unless the platform can't make any.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub thumbnails: Option<thumbnail::service::Thumbnails>,
}

/// Time given to in-flight requests on shutdown.
const GRACEFUL_SHUTDOWN: Duration = Duration::from_secs(5);
/// Time given to blocking work (e.g. a scan stuck on a hung mount) after that,
/// so `docker stop` never has to fall back to SIGKILL.
const BLOCKING_SHUTDOWN: Duration = Duration::from_secs(2);
/// Threads for file reads. A hung mount ties them up; tokio's default of 512
/// could use up a 32-bit board's address space. Further reads wait in line.
const MAX_BLOCKING_THREADS: usize = 64;
/// HTTP/2 requests in flight per connection; bounds memory per browser.
const HTTP2_MAX_STREAMS: u32 = 64;
/// Ping interval that detects HTTP/2 connections that went away silently
/// (e.g. a phone that left Wi-Fi).
const HTTP2_KEEP_ALIVE: Duration = Duration::from_secs(30);

fn env_required(name: &str) -> Result<String, String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} must be set"))
}

fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == thumbnail::child::MODE)
    {
        let allow_unconfined = env::args_os()
            .nth(2)
            .is_some_and(|arg| arg == thumbnail::child::ALLOW_UNCONFINED);
        std::process::exit(thumbnail::child::run(allow_unconfined));
    }
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| "failed to install the rustls ring crypto provider")?;
    let config_path = config::path();
    let config_path = config_path.as_path();

    if env::args().nth(1).as_deref() == Some("--healthcheck") {
        if !config_path.exists() {
            return Err(format!("unhealthy: no config at {}", config_path.display()).into());
        }
        let config = config::Config::load_or_create(config_path)?;
        return healthcheck::run(&config).map_err(|e| format!("unhealthy: {e}").into());
    }

    let config = config::Config::load_or_create(config_path)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .build()?;
    // Before the runtime starts: it reads the whole binary, once.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let thumbnails = thumbnails(&config.thumbnails);
    let result = runtime.block_on(serve(
        config,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        thumbnails,
    ));
    runtime.shutdown_timeout(BLOCKING_SHUTDOWN);
    result
}

/// Video thumbnails, made by children of this very binary, or `None`, said
/// once, if they can't be started here.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn thumbnails(settings: &config::Thumbnails) -> Option<thumbnail::service::Thumbnails> {
    use thumbnail::{child, service};
    let start = || -> std::io::Result<_> {
        let made_by = child::Thumbnailer::own_binary()?;
        Ok((
            made_by.allowing_unconfined(settings.allow_unconfined),
            child::build()?,
        ))
    };
    match start() {
        Ok((made_by, build)) => {
            let dir = &settings.dir;
            let unconfined = if settings.allow_unconfined {
                ", unsandboxed where the sandbox can't be had"
            } else {
                ""
            };
            eprintln!(
                "thumbnails in {} (build {build}){unconfined}",
                dir.display()
            );
            let log = service::AuditLog::Stderr;
            Some(service::Thumbnails::new(dir.clone(), made_by, build, log))
        }
        Err(err) => {
            eprintln!("thumbnails off: {err}");
            None
        }
    }
}

/// Only the items of `snapshot`, as [`library::Library::run_scanner`] hands
/// it over, keep their thumbnails.
async fn keep_thumbnails_of(state: AppState, snapshot: Arc<library::Snapshot>) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(thumbnails) = &state.thumbnails {
        let items = snapshot.item_ids().map(str::to_owned).collect();
        thumbnails.keep_only(items).await;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = (state, snapshot);
}

async fn serve(
    config: config::Config,
    #[cfg(any(target_os = "linux", target_os = "macos"))] thumbnails: Option<
        thumbnail::service::Thumbnails,
    >,
) -> Result<(), Box<dyn Error>> {
    let library = match config.storage.backend {
        config::StorageBackend::Local => {
            let link_ids = link_ids::LinkIds::load_or_create(&config.storage.link_key)?;
            // Fails (after a few retries) on a missing or empty mount instead
            // of serving an empty root.
            library::Library::open(&config.storage, link_ids)
                .await
                .map_err(|e| format!("storage: {e}"))?
        }
    };
    // Thumbnails are removed only in their folder: none if it overlaps the
    // library, so the library never loses a file. Checked by hand.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let thumbnails = thumbnails.and_then(|thumbnails| {
        let apart = thumbnails.apart_from(library.root());
        apart.map_err(|err| eprintln!("thumbnails off: {err}")).ok()
    });
    let credentials =
        auth::Credentials::new(&env_required("AUTH_USER")?, &env_required("AUTH_PASS")?)?;
    let tls = tls::load_or_create(&config.tls)?;
    let addr = config.server.bind_addr;

    eprintln!(
        "serving {} (pattern {}) on https://{addr} (TLS >= {:?})",
        config.storage.root.display(),
        config.storage.pattern,
        config.tls.min_version,
    );
    let sessions = Arc::new(session::Sessions::new(&config.session));
    tokio::spawn(sessions.clone().run_purger());
    let state = Arc::new(App {
        library: library.clone(),
        credentials,
        sessions,
        hsts_max_age: config.tls.hsts_max_age,
        access_log: access_log::AccessLog::Stdout,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        thumbnails,
    });
    // The snapshots the scanner hands over reach the thumbnails: checked by
    // hand on a running server (a video removed, its album unreadable, the
    // marker gone); the parts have tests.
    let scanned = {
        let state = state.clone();
        move |snapshot| keep_thumbnails_of(state.clone(), snapshot)
    };
    tokio::spawn(library.run_scanner(config.storage.rescan_interval, scanned));
    let app = web::router(state);

    let handle = Handle::new();
    tokio::spawn(shutdown_on_signal(handle.clone()));
    // TCP_NODELAY: small responses (e.g. 304s for an image grid) go out at
    // once instead of waiting for more data. Plain HTTP is answered before
    // TLS (see `plain_http`).
    let acceptor = plain_http::HttpsOnlyAcceptor::new(
        RustlsAcceptor::new(tls).acceptor(NoDelayAcceptor::new()),
    );
    let mut server = axum_server::bind(addr).acceptor(acceptor).handle(handle);
    server
        .http_builder()
        .http2()
        .max_concurrent_streams(HTTP2_MAX_STREAMS)
        .keep_alive_interval(HTTP2_KEEP_ALIVE);
    // Client addresses, for per-client sign-in throttling.
    server
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await?;
    Ok(())
}

/// Stops accepting connections on Ctrl-C or SIGTERM, giving in-flight requests
/// [`GRACEFUL_SHUTDOWN`].
async fn shutdown_on_signal(handle: Handle<SocketAddr>) {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
            sigterm.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    eprintln!("shutting down");
    handle.graceful_shutdown(Some(GRACEFUL_SHUTDOWN));
}
