//! Server-rendered HTML via `maud`: templates are checked at compile time and
//! every interpolated value is HTML-escaped automatically (the constant
//! `STYLE` is the one exception). No JavaScript.
//!
//! Page titles and headings are fixed strings ("Albums", "Album", "Sign in"):
//! one title per page type keeps the templates simple, and album names and
//! item file names are rendered only in the page body.

use crate::library::{ALBUM_NAME_PLACEHOLDER, Album, Item, MediaKind, Snapshot};
use crate::pattern::Metadata;
use crate::routes;
use maud::{DOCTYPE, Markup, PreEscaped, html};

/// The placeholder holding the item's file name. Its chip is left out because
/// the caption already shows the file name.
const FILE_NAME_PLACEHOLDER: &str = "file_name";

const STYLE: &str = "
body{font-family:system-ui,sans-serif;margin:0 auto;max-width:1200px;padding:1rem;background:#fafafa;color:#222}
a{color:inherit}.muted{color:#666;margin:.25rem 0}
.grid{display:grid;gap:1rem;grid-template-columns:repeat(auto-fill,minmax(220px,1fr))}
.card{display:block;background:#fff;border-radius:8px;padding:.5rem;box-shadow:0 1px 3px #0002;text-decoration:none;margin:0}
.card h3{margin:.5rem 0 0}.cover{aspect-ratio:4/3;background:#eee;border-radius:6px;overflow:hidden}
img,video{width:100%;display:block;border-radius:6px}.cover img{height:100%;object-fit:cover}
figcaption{font-size:.85rem;margin-top:.4rem;word-break:break-all}
.chips{display:flex;flex-wrap:wrap;gap:.3rem;margin:.4rem 0}.chip{font-size:.75rem;background:#e8eefc;border-radius:999px;padding:.1rem .5rem}
.sign-out{float:right}button{font:inherit;padding:.4rem .9rem;border-radius:6px;border:1px solid #bbb;background:#fff;cursor:pointer}
.sign-in{display:grid;gap:.75rem;max-width:20rem}.sign-in input{font:inherit;padding:.4rem;display:block;width:100%;box-sizing:border-box}
.error{color:#b00020}.album-name{font-size:1.25rem;margin:.25rem 0}
";

/// The sign-in page. `message` explains a failed attempt; `next` is where to
/// go after signing in (already checked to be local).
pub fn sign_in_page(message: Option<&str>, next: &str) -> Markup {
    page(
        "Sign in",
        false,
        html! {
            @if let Some(message) = message {
                p.error { (message) }
            }
            form.sign-in method="post" action=(routes::SIGN_IN) {
                input type="hidden" name="next" value=(next);
                label { "Username" input name="username" autocomplete="username" required autofocus; }
                label { "Password" input type="password" name="password" autocomplete="current-password" required; }
                button type="submit" { "Sign in" }
            }
        },
    )
}

/// The album list page (title "Albums").
pub fn album_list(snapshot: &Snapshot) -> Markup {
    page(
        "Albums",
        true,
        html! {
            (storage_notice(snapshot))
            @if snapshot.albums.is_empty() {
                p { "No albums found. Check the storage root and pattern." }
            }
            div.grid {
                @for album in snapshot.albums.iter() {
                    a.card href=(routes::album_url(&album.id)) {
                        div.cover {
                            @if let Some(cover) = album.items.iter().find(|item| item.kind == MediaKind::Image) {
                                img loading="lazy" src=(routes::item_url(&cover.id)) alt="";
                            }
                        }
                        h3 { (album.name) }
                        p.muted { (album.items.len()) " item(s)" }
                        (metadata_chips(&album.metadata, ALBUM_NAME_PLACEHOLDER))
                    }
                }
            }
        },
    )
}

/// One album's page (title "Album"), with its name in the body.
pub fn album_detail(snapshot: &Snapshot, album: &Album) -> Markup {
    page(
        "Album",
        true,
        html! {
            (storage_notice(snapshot))
            p { a href=(routes::HOME) { "← All albums" } }
            p.album-name { (album.name) }
            (metadata_chips(&album.metadata, ALBUM_NAME_PLACEHOLDER))
            div.grid {
                @for item in &album.items {
                    figure.card {
                        (media_tag(item))
                        figcaption {
                            (item.file_name)
                            (metadata_chips(&item.metadata, FILE_NAME_PLACEHOLDER))
                        }
                    }
                }
            }
        },
    )
}

/// Shown while the latest scan failed and the snapshot from the last
/// successful scan is displayed.
fn storage_notice(snapshot: &Snapshot) -> Markup {
    html! {
        @if !snapshot.storage.is_available() {
            p.error { "Storage is unavailable right now; showing the albums from the last successful scan." }
        }
    }
}

fn media_tag(item: &Item) -> Markup {
    let url = routes::item_url(&item.id);
    html! {
        @match item.kind {
            MediaKind::Image => a href=(url) target="_blank" {
                img loading="lazy" src=(url) alt=(item.file_name);
            },
            MediaKind::Video => video controls preload="metadata" src=(url) {},
        }
    }
}

/// Metadata as chips, leaving out the `already_shown` placeholder (the album
/// name or file name, which the page displays anyway).
fn metadata_chips(metadata: &Metadata, already_shown: &str) -> Markup {
    let chips: Vec<_> = metadata
        .iter()
        .filter(|(placeholder, _)| placeholder.as_str() != already_shown)
        .collect();
    html! {
        @if !chips.is_empty() {
            div.chips {
                @for (placeholder, value) in chips {
                    span.chip { (placeholder) ": " (value) }
                }
            }
        }
    }
}

/// The shared page layout. `title` is `'static` because titles are fixed per
/// page type, never built from data; the same string is the page heading.
/// Signed-in pages get the sign-out button.
fn page(title: &'static str, signed_in: bool, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width,initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                @if signed_in {
                    (sign_out_button())
                }
                h1 { (title) }
                (body)
            }
        }
    }
}

/// Signing out must be a POST: a link (GET) could be triggered by any page.
fn sign_out_button() -> Markup {
    html! {
        form.sign-out method="post" action=(routes::SIGN_OUT) {
            button type="submit" { "Sign out" }
        }
    }
}
