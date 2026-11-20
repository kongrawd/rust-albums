//! Server-rendered HTML via `maud`: templates are checked at compile time and
//! every interpolated value is HTML-escaped automatically (the constant
//! `STYLE` is the one exception). No JavaScript.
//!
//! Page titles and the page heading (`h1`) are fixed strings, one per page
//! type: that keeps the templates simple, and album names and item file names
//! are rendered only in the page body, where they are headings of their own.

use crate::library::{ALBUM_NAME_PLACEHOLDER, Album, Item, ItemPlace, MediaKind, Snapshot};
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
/// A tile an item page led back to is outlined, with room above it to show
/// where it sits, and so is a tile with the keyboard's focus: inside the tile,
/// as a ring outside would leave the screen at its edge, and in the text's
/// color, as some browsers' own ring hardly shows on a photo. The two are
/// separate rules: a browser that doesn't know `:focus-visible` drops the rule
/// it is in, and keeps its own focus ring. The pager keeps previous, place
/// and next in fixed columns, so each stays put at the album's ends, and its
/// links are 44px tall (WCAG 2.5.5 target size).
///
/// Every page is a column at least as tall as the screen (`svh`, which doesn't
/// change as mobile toolbars hide; `vh` before it for older browsers). On an
/// item page the photo or video takes the room its header, name and pager
/// leave, whatever the screen and zoom, and never less than 12rem (or a third
/// of a short screen), below which the page scrolls. It is placed absolutely
/// in that stage, so its own size can't change the stage's while it loads, and
/// `scale-down` fits it inside without ever enlarging it. Short screens,
/// zoomed ones included, get a smaller heading.
const STYLE: &str = "
html{-webkit-text-size-adjust:100%;text-size-adjust:100%}
body{box-sizing:border-box;min-height:100vh;min-height:100svh;display:flex;flex-direction:column;font-family:system-ui,-apple-system,'Segoe UI',Roboto,'Noto Sans',sans-serif,'Apple Color Emoji','Segoe UI Emoji','Noto Color Emoji';margin:0 auto;max-width:1400px;padding:1rem;overflow-wrap:anywhere}
a{color:inherit}button,input{font:inherit}img{display:block}
.top{display:flex;align-items:center;justify-content:space-between;gap:1rem}.top h1{margin:.25rem 0}
.muted{opacity:.75;margin:0}
.albums,.items{list-style:none;padding:0}
.albums{--gap:.75rem;margin:0;display:grid;gap:1rem var(--gap);grid-template-columns:repeat(auto-fill,minmax(min(9.5rem,calc((100% - var(--gap))/2)),1fr))}
.album{display:block;text-decoration:none}.album h2{font-size:1rem;margin:.4rem 0 0}
.cover{aspect-ratio:1;border-radius:10px;overflow:hidden;background:#8882}
.cover img,.tile img{width:100%;height:100%;object-fit:cover}
.items{--gap:2px;display:grid;gap:var(--gap);grid-template-columns:repeat(auto-fill,minmax(min(7.5rem,calc((100% - 2*var(--gap))/3)),1fr));margin:.75rem -1rem 0}
@media(min-width:40rem){.items{--gap:4px;margin-inline:0}}
.tile{position:relative;display:block;aspect-ratio:1;background:#8882;overflow:hidden;text-decoration:none;scroll-margin-top:20vh}
.tile:target{outline:3px solid;outline-offset:-3px}
.tile:focus-visible{outline:3px solid;outline-offset:-3px}
.pager{display:grid;grid-template-columns:1fr auto 1fr;align-items:center;gap:.75rem;margin:.5rem 0}
.pager a{display:flex;align-items:center;gap:.5rem;min-height:44px;text-decoration:none}
.pager [rel=prev]{grid-column:1;justify-self:start}.pager [rel=next]{grid-column:3;justify-self:end;flex-direction:row-reverse}
.pager img{width:44px;height:44px;object-fit:cover;border-radius:6px;background:#8882}
.place{grid-column:2;opacity:.75}
.play{position:absolute;inset:0;display:grid;place-items:center;font-size:1.5rem;opacity:.75}
.caption{position:absolute;inset:auto 0 0;padding:1.25rem .4rem .25rem;font-size:.7rem;color:#fff;background:linear-gradient(#0000,#0009);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.chips{display:flex;flex-wrap:wrap;gap:.3rem;margin:.4rem 0}.chip{font-size:.75rem;background:#8883;border-radius:999px;padding:.1rem .5rem}
.sign-in{display:grid;gap:.75rem;max-width:20rem}.sign-in input{display:block;width:100%;box-sizing:border-box}
.error{color:#b00020}.name{font-size:1.25rem;font-weight:600;margin:.25rem 0}
.item{flex:1 0 auto;display:flex;flex-direction:column}
.view{position:relative;flex:1 0 12rem;flex-basis:min(12rem,35vh);flex-basis:min(12rem,35svh);margin:.75rem 0}
.view img,.view video{position:absolute;top:0;left:0;width:100%;height:100%;object-fit:scale-down}
@media(max-height:32rem){body{padding-top:.5rem;padding-bottom:.5rem}.top h1{font-size:1.25rem;margin:0}.view{margin:.25rem 0}}
@media(prefers-color-scheme:dark){.error{color:#ff8a80}}
";

/// The sign-in page. `message` explains a failed attempt; `next` is where to
/// go after signing in (already checked to be local).
pub fn sign_in_page(message: Option<&str>, next: &str) -> Markup {
    page(
        "Sign in",
        false,
        Layout::Flow,
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
        Layout::Flow,
        html! {
            (storage_notice(snapshot))
            @if snapshot.albums.is_empty() {
                p { "No albums found. Check the storage root and pattern." }
            }
            // `role="list"`: Safari drops the semantics of lists without bullets.
            ul.albums role="list" {
                @for album in snapshot.albums.iter() {
                    li {
                        a.album href=(routes::album_url(&album.id)) {
                            div.cover {
                                @if let Some(cover) = album.items.iter().find_map(tile_picture) {
                                    (picture_img(cover, Loading::Lazy))
                                }
                            }
                            h2 { (album.name) }
                            p.muted { (item_count(album.items.len())) }
                            (metadata_chips(&album.metadata, ALBUM_NAME_PLACEHOLDER))
                        }
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
        Layout::Flow,
        html! {
            (storage_notice(snapshot))
            p { a href=(routes::HOME) { "← All albums" } }
            h2.name { (album.name) }
            (metadata_chips(&album.metadata, ALBUM_NAME_PLACEHOLDER))
            ul.items role="list" {
                @for item in &album.items {
                    li { (item_tile(item)) }
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
/// then its name, metadata and a link opening its original in a new tab, for
/// anything the browser can't show here, and last the [`pager`] to the items
/// beside it. A video shows its thumbnail until it plays, if it has one: iOS
/// shows no frame of it before that.
pub fn item_page(snapshot: &Snapshot, place: &ItemPlace) -> Markup {
    let ItemPlace { album, item, .. } = place;
    let url = routes::original_url(&item.id);
    let (title, media) = match item.kind {
        // Fetched before the pager's pictures.
        MediaKind::Image => (
            "Photo",
            html! { img src=(url) alt=(item.file_name) fetchpriority="high"; },
        ),
        MediaKind::Video => (
            "Video",
            html! {
                video controls playsinline preload="metadata" poster=[tile_picture(item)] {
                    source src=(url);
                }
            },
        ),
    };
    page(
        title,
        true,
        Layout::Item,
        html! {
            (storage_notice(snapshot))
            p { a href=(routes::album_url(&album.id).at(&item.id)) { "← " (album.name) } }
            div.view { (media) }
            div.top {
                h2.name { (item.file_name) }
                a href=(url) target="_blank" { "Open original" }
            }
            (metadata_chips(&item.metadata, FILE_NAME_PLACEHOLDER))
            (pager(place))
        },
    )
}

/// Links to the items before and after this one, each with the picture of its
/// tile ([`tile_picture`]), and the item's place in the album. Its words are fixed,
/// never names, and an end of the album simply has no link.
fn pager(place: &ItemPlace) -> Markup {
    html! {
        nav.pager aria-label="Items" {
            (pager_link(place.previous, &PREVIOUS))
            span.place { (place.number()) " of " (place.count()) }
            (pager_link(place.next, &NEXT))
        }
    }
}

/// One side of the [`pager`]: its link's `rel` and words, and when the
/// picture of the item it leads to loads.
struct PagerSide {
    rel: &'static str,
    label: &'static str,
    loading: Loading,
}

/// The item before, likely just seen: its picture loads as the pager nears
/// the screen.
const PREVIOUS: PagerSide = PagerSide {
    rel: "prev",
    label: "← Previous",
    loading: Loading::Lazy,
};

/// The item after: its picture loads with the page, so the next page has its
/// photo at hand (a photo's picture is its original).
const NEXT: PagerSide = PagerSide {
    rel: "next",
    label: "Next →",
    loading: Loading::Eager,
};

/// A link of the [`pager`] to the item on `side`, if there is one; the styles
/// put its picture on the outer side.
fn pager_link(item: Option<&Item>, side: &PagerSide) -> Markup {
    html! {
        @if let Some(item) = item {
            a rel=(side.rel) href=(routes::item_page_url(&item.id)) {
                @if let Some(src) = tile_picture(item) {
                    (picture_img(src, side.loading))
                }
                (side.label)
            }
        }
    }
}

/// A square tile opening the item's page, showing its [`tile_picture`] (a video's
/// with a play glyph over it); its metadata is the tooltip. Its `id`, the
/// item's link id, is where the item's page leads back to.
fn item_tile(item: &Item) -> Markup {
    html! {
        a.tile id=(item.id) href=(routes::item_page_url(&item.id)) title=(Tooltip(item)) {
            @if let Some(src) = tile_picture(item) {
                (picture_img(src, Loading::Lazy))
            }
            @if item.kind == MediaKind::Video {
                // U+FE0E: a text glyph, never a colour emoji.
                span.play aria-hidden="true" { "▶\u{FE0E}" }
            }
            span.caption { (item.file_name) }
        }
    }
}

/// The URL of the picture `item`'s tile shows, and so its album's cover, the
/// pager and, for a video, its page until it plays: the one place deciding it
/// (a video tile adds its play glyph on its own). A photo shows its original;
/// a video its thumbnail, if it has one: never the video, as browsers can't
/// lazy-load `<video>`.
fn tile_picture(item: &Item) -> Option<routes::Link<'_>> {
    match item.kind {
        MediaKind::Image => Some(routes::original_url(&item.id)),
        MediaKind::Video if thumbnail::has_thumbnail(&item.path) => {
            Some(routes::thumbnail_url(&item.id))
        }
        MediaKind::Video => None,
    }
}

/// When a picture loads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Loading {
    /// As it nears the screen: grids hold many.
    Lazy,
    /// With the page.
    Eager,
}

/// An item's [`tile_picture`], decorative: the link it is in is named by its
/// words (the file name, the album name, or the pager's "Previous" and
/// "Next").
fn picture_img(src: routes::Link<'_>, loading: Loading) -> Markup {
    let lazy = (loading == Loading::Lazy).then_some("lazy");
    html! { img loading=[lazy] src=(src) alt=""; }
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

/// How a page's content is laid out below its header (see [`STYLE`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// One block after another, as long as it needs.
    Flow,
    /// An item page's: its photo or video takes the room the rest leaves.
    Item,
}

/// The shared page layout. `title` is `'static` because titles are fixed per
/// page type, never built from data; the same string is the page heading.
/// Signed-in pages get the sign-out button beside it.
fn page(title: &'static str, signed_in: bool, layout: Layout, body: Markup) -> Markup {
    let class = (layout == Layout::Item).then_some("item");
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
                main class=[class] { (body) }
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
