//! rust-albums: a minimal HTTPS-only album viewer over a local folder tree.

mod pattern;
mod crypto;
mod config;
mod link_ids;
mod library;
mod tls;
mod session;
mod routes;
mod auth;

use std::{sync::Arc, time::Duration};

pub type AppState = Arc<App>;

pub struct App {
    pub library: Arc<library::Library>,
    pub credentials: auth::Credentials,
    pub sessions: Arc<session::Sessions>,
    pub hsts_max_age: Duration,
}

fn main() {}
