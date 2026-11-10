//! Every URL of the site in one place: route patterns for the router, builders
//! for links, and the check that keeps post-sign-in redirects on this site.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

pub const HOME: &str = "/";
pub const SIGN_IN: &str = "/login";
pub const SIGN_OUT: &str = "/logout";
pub const HEALTH: &str = "/healthz";
/// Router pattern matched by [`album_url`].
pub const ALBUM: &str = "/albums/{album_id}";
/// Router pattern matched by [`item_url`].
pub const ITEM: &str = "/media/{item_id}";

const ALBUM_PREFIX: &str = "/albums/";
const ITEM_PREFIX: &str = "/media/";

/// Characters left unescaped when encoding (RFC 3986 "unreserved").
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Link ids are already URL-safe (base64url), so they need no encoding.
pub fn album_url(album_id: &str) -> String {
    format!("{ALBUM_PREFIX}{album_id}")
}

pub fn item_url(item_id: &str) -> String {
    format!("{ITEM_PREFIX}{item_id}")
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
        assert!(ITEM.starts_with(ITEM_PREFIX));
        assert_eq!(album_url("q2bMf0-_x"), "/albums/q2bMf0-_x");
        assert_eq!(item_url("q2bMf0-_x"), "/media/q2bMf0-_x");
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
