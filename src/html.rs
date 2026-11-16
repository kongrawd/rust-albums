//! Server-rendered HTML via `maud`: templates are checked at compile time and
//! every interpolated value is HTML-escaped automatically (the constant
//! `STYLE` is the one exception). No JavaScript.
//!
//! Page titles and headings are fixed strings, one per page type: that keeps
//! the templates simple, and album names and item file names are rendered
//! only in the page body.

use crate::library::{ALBUM_NAME_PLACEHOLDER, Album, Item, MediaKind, Snapshot};
use crate::pattern::Metadata;
use crate::{routes, thumbnail};
use maud::{DOCTYPE, Escaper, Markup, PreEscaped, Render, html};
use std::fmt::{self, Write as _};

/// The placeholder holding the item's file name. Its chip is left out because
/// the tile or page already shows the file name.
const FILE_NAME_PLACEHOLDER: &str = "file_name";

/// System defaults wherever possible: the page and form controls use the
/// browser's own colors (light or dark, per `color-scheme`) and the platform UI
/// font. Controls keep their native look; only their font is set, as half-styled
/// controls break (iOS draws submit buttons with white text on its own fill).
/// The remaining colors are translucent, so they work on light and dark pages.
///
/// Albums and items are square tiles in as many columns as fit, never fewer
/// than two albums or three items per row (the minimum width comes from
/// `--gap`), so phones stay compact. Items run edge to edge on narrow screens.
const STYLE: &str = "
html{-webkit-text-size-adjust:100%;text-size-adjust:100%}
body{font-family:system-ui,-apple-system,'Segoe UI',Roboto,'Noto Sans',sans-serif,'Apple Color Emoji','Segoe UI Emoji','Noto Color Emoji';margin:0 auto;max-width:1400px;padding:1rem;overflow-wrap:anywhere}
a{color:inherit}button,input{font:inherit}img{display:block}
.top{display:flex;align-items:center;justify-content:space-between;gap:1rem}.top h1{margin:.25rem 0}
.muted{opacity:.75;margin:0}
.albums{--gap:.75rem;display:grid;gap:1rem var(--gap);grid-template-columns:repeat(auto-fill,minmax(min(9.5rem,calc((100% - var(--gap))/2)),1fr))}
.album{text-decoration:none}.album h2{font-size:1rem;margin:.4rem 0 0}
.cover{aspect-ratio:1;border-radius:10px;overflow:hidden;background:#8882}
.cover img,.tile img{width:100%;height:100%;object-fit:cover}
.items{--gap:2px;display:grid;gap:var(--gap);grid-template-columns:repeat(auto-fill,minmax(min(7.5rem,calc((100% - 2*var(--gap))/3)),1fr));margin:.75rem -1rem 0}
@media(min-width:40rem){.items{--gap:4px;margin-inline:0}}
.tile{position:relative;display:block;aspect-ratio:1;background:#8882;overflow:hidden;text-decoration:none}
.play{position:absolute;inset:0;display:grid;place-items:center;font-size:1.5rem;opacity:.75}
.caption{position:absolute;inset:auto 0 0;padding:1.25rem .4rem .25rem;font-size:.7rem;color:#fff;background:linear-gradient(#0000,#0009);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.chips{display:flex;flex-wrap:wrap;gap:.3rem;margin:.4rem 0}.chip{font-size:.75rem;background:#8883;border-radius:999px;padding:.1rem .5rem}
.sign-in{display:grid;gap:.75rem;max-width:20rem}.sign-in input{display:block;width:100%;box-sizing:border-box}
.error{color:#b00020}.name{font-size:1.25rem;font-weight:600;margin:.25rem 0}
.view img,.view video{display:block;max-width:100%;max-height:80vh;margin:.75rem auto}
@media(prefers-color-scheme:dark){.error{color:#ff8a80}}
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
            div.albums {
                @for album in snapshot.albums.iter() {
                    a.album href=(routes::album_url(&album.id)) {
                        div.cover {
                            @if let Some(cover) = album.items.iter().find_map(preview) {
                                img loading="lazy" src=(cover) alt="";
                            }
                        }
                        h2 { (album.name) }
                        p.muted { (item_count(album.items.len())) }
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
            p.name { (album.name) }
            (metadata_chips(&album.metadata, ALBUM_NAME_PLACEHOLDER))
            div.items {
                @for item in &album.items {
                    (item_tile(item))
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

fn item_count(count: usize) -> String {
    match count {
        1 => "1 item".to_string(),
        count => format!("{count} items"),
    }
}

/// One item's page (title "Photo" or "Video"): the photo or the playable video,
/// with a link opening its original in a new tab, for anything the browser
/// can't show here.
pub fn item_page(snapshot: &Snapshot, album: &Album, item: &Item) -> Markup {
    let url = routes::original_url(&item.id);
    let (title, media) = match item.kind {
        MediaKind::Image => ("Photo", html! { img src=(url) alt=(item.file_name); }),
        MediaKind::Video => (
            "Video",
            html! { video controls playsinline preload="metadata" { source src=(url); } },
        ),
    };
    page(
        title,
        true,
        html! {
            (storage_notice(snapshot))
            p { a href=(routes::album_url(&album.id)) { "← " (album.name) } }
            div.top {
                p.name { (item.file_name) }
                a href=(url) target="_blank" { "Open original" }
            }
            (metadata_chips(&item.metadata, FILE_NAME_PLACEHOLDER))
            div.view { (media) }
        },
    )
}

/// A square tile opening the item's page, showing its [`preview`] (a video's
/// with a play glyph over it); its metadata is the tooltip.
fn item_tile(item: &Item) -> Markup {
    html! {
        a.tile href=(routes::item_page_url(&item.id)) title=(Tooltip(item)) {
            @if let Some(src) = preview(item) {
                img loading="lazy" src=(src) alt="";
            }
            @if item.kind == MediaKind::Video {
                // U+FE0E: a text glyph, never a colour emoji.
                span.play aria-hidden="true" { "▶\u{FE0E}" }
            }
            span.caption { (item.file_name) }
        }
    }
}

/// The URL of the image that tiles and album covers show for `item`, the one
/// place deciding it (a video tile adds its play glyph on its own). A photo
/// shows its original; a video its thumbnail, if it has one: never the video,
/// as browsers can't lazy-load `<video>`.
fn preview(item: &Item) -> Option<routes::Link<'_>> {
    match item.kind {
        MediaKind::Image => Some(routes::original_url(&item.id)),
        MediaKind::Video if thumbnail::has_thumbnail(&item.path) => {
            Some(routes::thumbnail_url(&item.id))
        }
        MediaKind::Video => None,
    }
}

/// A tile's tooltip: the file name, then its metadata labels. Written straight
/// into the page, escaped, so a grid builds no `String` per tile.
struct Tooltip<'a>(&'a Item);

impl Render for Tooltip<'_> {
    fn render_to(&self, buffer: &mut String) {
        let mut tooltip = Escaper::new(buffer);
        let written = tooltip.write_str(&self.0.file_name).and_then(|()| {
            metadata_labels(&self.0.metadata, FILE_NAME_PLACEHOLDER)
                .try_for_each(|label| write!(tooltip, " · {label}"))
        });
        written.expect("writing to a String can't fail");
    }
}

/// A metadata value as chips and tooltips show it: `placeholder: value`.
struct Label<'a> {
    placeholder: &'a str,
    value: &'a str,
}

impl fmt::Display for Label<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.placeholder, self.value)
    }
}

/// The metadata's labels, leaving out the `already_shown` placeholder (the
/// album name or file name, which the page displays anyway).
fn metadata_labels<'a>(
    metadata: &'a Metadata,
    already_shown: &'a str,
) -> impl Iterator<Item = Label<'a>> + 'a {
    metadata
        .iter()
        .filter(move |(placeholder, _)| placeholder.as_str() != already_shown)
        .map(|(placeholder, value)| Label { placeholder, value })
}

fn metadata_chips(metadata: &Metadata, already_shown: &str) -> Markup {
    let chips: Vec<_> = metadata_labels(metadata, already_shown).collect();
    html! {
        @if !chips.is_empty() {
            div.chips {
                @for chip in chips {
                    span.chip { (chip) }
                }
            }
        }
    }
}

/// The shared page layout. `title` is `'static` because titles are fixed per
/// page type, never built from data; the same string is the page heading.
/// Signed-in pages get the sign-out button beside it.
fn page(title: &'static str, signed_in: bool, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width,initial-scale=1";
                meta name="color-scheme" content="light dark";
                title { (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                header.top {
                    h1 { (title) }
                    @if signed_in {
                        (sign_out_button())
                    }
                }
                (body)
            }
        }
    }
}

/// Signing out must be a POST: a link (GET) could be triggered by any page.
fn sign_out_button() -> Markup {
    html! {
        form method="post" action=(routes::SIGN_OUT) {
            button type="submit" { "Sign out" }
        }
    }
}
