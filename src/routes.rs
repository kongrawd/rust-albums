//! Every URL of the site in one place: route patterns for the router, builders
//! for links, and the check that keeps post-sign-in redirects on this site.

use crate::library::{AlbumSort, SortKey};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use std::fmt;

pub const HOME: &str = "/";
pub const SIGN_IN: &str = "/login";
pub const SIGN_OUT: &str = "/logout";
pub const HEALTH: &str = "/healthz";
/// Router pattern matched by [`album_url`].
pub const ALBUM: &str = "/albums/{album_id}";
/// Router pattern matched by [`original_url`].
pub const ORIGINAL: &str = "/media/{item_id}";
/// Router pattern matched by [`item_page_url`]: the same link id as [`ORIGINAL`].
pub const ITEM_PAGE: &str = "/items/{item_id}";
/// Router pattern matched by [`thumbnail_url`]: the same link id as [`ORIGINAL`].
pub const THUMBNAIL: &str = "/thumbnails/{item_id}";

const ALBUM_PREFIX: &str = "/albums/";
const ORIGINAL_PREFIX: &str = "/media/";
const ITEM_PAGE_PREFIX: &str = "/items/";
const THUMBNAIL_PREFIX: &str = "/thumbnails/";

/// Characters left unescaped when encoding (RFC 3986 "unreserved").
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The URL of an album, an item's page or an original, by link id, with the
/// link id of an item on that page as its fragment, if any. It is written out
/// through `Display`, so pages render it without building a `String` per link
/// (`to_string` gives one where needed). Link ids are already URL-safe
/// (base64url), so they need no encoding.
#[derive(Clone, Copy)]
pub struct Link<'a> {
    prefix: &'static str,
    id: &'a str,
    fragment: Option<&'a str>,
}

impl fmt::Display for Link<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.prefix)?;
        f.write_str(self.id)?;
        match self.fragment {
            Some(fragment) => {
                f.write_str("#")?;
                f.write_str(fragment)
            }
            None => Ok(()),
        }
    }
}

impl<'a> Link<'a> {
    /// This page scrolled to the element whose `id` is `item_id`, an item's
    /// link id: an album at one of its tiles. Never a name, which would reach
    /// the browser's history.
    pub fn at(self, item_id: &'a str) -> Self {
        Self {
            fragment: Some(item_id),
            ..self
        }
    }
}

/// The link to the URL `prefix` + `id`, without a fragment.
const fn link<'a>(prefix: &'static str, id: &'a str) -> Link<'a> {
    Link {
        prefix,
        id,
        fragment: None,
    }
}

pub fn album_url(album_id: &str) -> Link<'_> {
    link(ALBUM_PREFIX, album_id)
}

pub fn original_url(item_id: &str) -> Link<'_> {
    link(ORIGINAL_PREFIX, item_id)
}

/// The page showing an item, as opposed to its original ([`original_url`]).
pub fn item_page_url(item_id: &str) -> Link<'_> {
    link(ITEM_PAGE_PREFIX, item_id)
}

/// An item's thumbnail, for items that have one (see `thumbnail::has_thumbnail`).
pub fn thumbnail_url(item_id: &str) -> Link<'_> {
    link(THUMBNAIL_PREFIX, item_id)
}

/// The album list's query: its parameters and the words in them, written
/// here only, for the links that set them and for [`album_sort`], which reads
/// them back.
const SORT: &str = "sort";
const ORDER: &str = "order";
const DESCENDING: &str = "desc";
const BY_FOLDER: &str = "folder";
const BY_NAME: &str = "name";
const BY_CREATED: &str = "created";

/// A sort key as the album list's URL gives it: a fixed word, or a placeholder
/// by its own name (ASCII letters, digits and `_`, so it needs no encoding).
/// Never an album's name or a value.
pub fn sort_word(key: &SortKey) -> &str {
    match key {
        SortKey::Folder => BY_FOLDER,
        SortKey::Name => BY_NAME,
        SortKey::Placeholder(placeholder) => placeholder,
        SortKey::Created => BY_CREATED,
    }
}

/// The album list sorted by `key`, ascending or not.
pub fn album_list_url(key: &SortKey, descending: bool) -> AlbumListLink<'_> {
    AlbumListLink { key, descending }
}

/// The URL [`album_list_url`] gives, written out through `Display`, as
/// [`Link`] is.
#[derive(Clone, Copy)]
pub struct AlbumListLink<'a> {
    key: &'a SortKey,
    descending: bool,
}

impl fmt::Display for AlbumListLink<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{HOME}?{SORT}={}", sort_word(self.key))?;
        if self.descending {
            write!(f, "&{ORDER}={DESCENDING}")?;
        }
        Ok(())
    }
}

/// The sort the album list's `query` asks for, among `keys`: by the first
/// `sort` word that names one of them, descending if `order` says so. Anything
/// else, a query that doesn't decode included, sorts by folder: a page never
/// fails over its order.
pub fn album_sort(query: Option<&str>, keys: &[SortKey]) -> AlbumSort {
    let (mut word, mut descending) = (None, false);
    for (name, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match &*name {
            SORT if word.is_none() => word = Some(value),
            ORDER => descending = value == DESCENDING,
            _ => {}
        }
    }
    let key = word.and_then(|word| keys.iter().find(|key| sort_word(key) == word));
    key.map_or_else(AlbumSort::default, |key| AlbumSort {
        key: key.clone(),
        descending,
    })
}

/// The sign-in form, set to return to `target` (a local URL) afterwards.
pub fn sign_in_url(target: &str) -> String {
    format!("{SIGN_IN}?next={}", utf8_percent_encode(target, UNRESERVED))
}

/// Where to go after signing in: `next` if it is a URL on this site, else home.
/// Rejects anything a browser could resolve to another host (open redirect):
/// absolute URLs, `//host`, `/\host`, and control characters.
pub fn return_target(next: Option<&str>) -> &str {
    next.filter(|target| {
        target.starts_with('/')
            && !target.starts_with("//")
            && !target.contains('\\')
            && !target.chars().any(char::is_control)
            && !target.starts_with(SIGN_IN)
            && !target.starts_with(SIGN_OUT)
    })
    .unwrap_or(HOME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_match_router_patterns() {
        assert!(ALBUM.starts_with(ALBUM_PREFIX));
        assert!(ORIGINAL.starts_with(ORIGINAL_PREFIX));
        assert!(ITEM_PAGE.starts_with(ITEM_PAGE_PREFIX));
        assert!(THUMBNAIL.starts_with(THUMBNAIL_PREFIX));
        assert_eq!(album_url("q2bMf0-_x").to_string(), "/albums/q2bMf0-_x");
        let year = SortKey::Placeholder("year".into());
        assert_eq!(album_list_url(&year, false).to_string(), "/?sort=year");
        assert_eq!(
            album_list_url(&SortKey::Created, true).to_string(),
            "/?sort=created&order=desc"
        );
        assert_eq!(
            album_url("q2bMf0-_x").at("Zr9-_a").to_string(),
            "/albums/q2bMf0-_x#Zr9-_a"
        );
        assert_eq!(original_url("q2bMf0-_x").to_string(), "/media/q2bMf0-_x");
        assert_eq!(item_page_url("q2bMf0-_x").to_string(), "/items/q2bMf0-_x");
        assert_eq!(
            thumbnail_url("q2bMf0-_x").to_string(),
            "/thumbnails/q2bMf0-_x"
        );
        assert_eq!(
            sign_in_url("/albums/a?x=1"),
            "/login?next=%2Falbums%2Fa%3Fx%3D1"
        );
    }

    #[test]
    fn return_target_stays_on_this_site() {
        assert_eq!(
            return_target(Some("/albums/q2bMf0?x=1")),
            "/albums/q2bMf0?x=1"
        );
        assert_eq!(return_target(Some("/media/q2bMf0")), "/media/q2bMf0");
        for hostile in [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
            "/\tevil",
            "evil",
            "",
            "/login?next=/",
            "/logout",
        ] {
            assert_eq!(return_target(Some(hostile)), HOME, "{hostile:?}");
        }
        assert_eq!(return_target(None), HOME);
    }
}
