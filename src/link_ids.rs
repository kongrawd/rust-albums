//! Link ids: fixed-length, URL-safe ids for albums and items, used in URLs
//! instead of album and item paths. Paths can contain spaces, `?`, `#`, `%`,
//! non-ASCII and very long names, all of which need escaping and differ in
//! Unicode form between filesystems; a link id never does.
//!
//! id = base64url(HMAC-SHA256(key, label ‖ NFC(path))[..16]), 22 characters.
//! - Fixed length and URL-safe: base64url, no escaping needed.
//! - Stable: the key is kept on disk, so ids survive restarts and pattern
//!   edits; the same path always gets the same id, also when the root moves
//!   between macOS (NFD names) and Linux. Renaming or moving a file gives it a
//!   new id. Losing the key changes all ids, but loses no data.
//! - Labelled: album ids and item ids are computed with different labels.
//! - Validated by construction: a link id is never turned back into a path;
//!   it is only looked up among the ids the latest scan produced.
//!
//! The key is created once with mode 0600; a warning is logged if an existing
//! key is readable by other users.
//!
//! HMAC truncated to 128 bits (RFC 2104 §5), base64url without padding (RFC 4648 §5).

use crate::crypto;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::hmac;
use std::io::ErrorKind;
use std::time::Duration;
use std::{fmt::Display, fs, path::Path};
use unicode_normalization::UnicodeNormalization;

const KEY_BYTES: usize = 32;
const ID_BYTES: usize = 16;
/// Version 1 of the id construction; a new version changes every id.
const ALBUM_LABEL: &[u8] = b"rust-albums/v1/album\0";
const ITEM_LABEL: &[u8] = b"rust-albums/v1/item\0";
/// How long to wait for a key another process is writing at the same moment.
const CONCURRENT_CREATE_WAIT: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub struct LinkIds {
    key: hmac::Key,
}

impl LinkIds {
    /// Reads the key at `path`, first creating a random one there if it doesn't
    /// exist. Creation never replaces an existing file, so when two processes
    /// start together both end up with the same key.
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        let describe = |err: &dyn Display| format!("link key {}: {err}", path.display());
        match crypto::create_secret_file(path, &crypto::random::<KEY_BYTES>()) {
            Ok(()) => println!("generated link key -> {}", path.display()),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
            Err(err) => return Err(describe(&err)),
        }
        let mut key = fs::read(path).map_err(|e| describe(&e))?;
        if key.is_empty() {
            // Possibly another process created the file and is still writing it.
            std::thread::sleep(CONCURRENT_CREATE_WAIT);
            key = fs::read(path).map_err(|e| describe(&e))?;
        }
        if key.is_empty() {
            return Err(describe(
                &"file is empty, probably from an interrupted first start; delete it to \
                  generate a new key (all album and item URLs will change)",
            ));
        }
        if key.len() != KEY_BYTES {
            return Err(describe(&format!(
                "expected {KEY_BYTES} bytes, found {}",
                key.len()
            )));
        }
        warn_if_readable_by_others(path);
        Ok(Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, &key),
        })
    }

    pub fn album(&self, album_path: &str) -> String {
        self.id(ALBUM_LABEL, album_path)
    }

    pub fn item(&self, item_path: &str) -> String {
        self.id(ITEM_LABEL, item_path)
    }

    fn id(&self, label: &[u8], path: &str) -> String {
        let mut context = hmac::Context::with_key(&self.key);
        context.update(label);
        // NFC, so "é" as one code point (Linux, Windows) and as "e" + accent
        // (macOS) give the same id. The raw path is still used to open the file.
        let normalized: String = path.nfc().collect();
        context.update(normalized.as_bytes());
        URL_SAFE_NO_PAD.encode(&context.sign().as_ref()[..ID_BYTES])
    }
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "warning: link key {} is readable by other users (mode {mode:o}); \
                 run `chmod 600` on it",
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_fixed_length_url_safe_stable_and_labelled() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("keys/link.key");
        let ids = LinkIds::load_or_create(&key_path).unwrap();

        let id = ids.item("Summer Trip/beach.jpg");
        assert_eq!(id.len(), 22);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(
            ids.item("Caf\u{e9}/a.jpg"),
            ids.item("Cafe\u{301}/a.jpg"),
            "NFC and NFD spellings of the same name share an id"
        );
        assert_ne!(
            ids.album("Trip"),
            ids.item("Trip"),
            "album and item labels differ"
        );
        assert_ne!(ids.item("Trip/a.jpg"), ids.item("Trip/b.jpg"));

        // The persisted key gives the same ids after a restart.
        let reloaded = LinkIds::load_or_create(&key_path).unwrap();
        assert_eq!(reloaded.item("Summer Trip/beach.jpg"), id);

        // Another installation (another key) gives unrelated ids.
        let other = LinkIds::load_or_create(&dir.path().join("other.key")).unwrap();
        assert_ne!(other.item("Summer Trip/beach.jpg"), id);
    }

    #[test]
    fn key_file_is_private_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("link.key");
        LinkIds::load_or_create(&key_path).unwrap();
        assert_eq!(fs::read(&key_path).unwrap().len(), KEY_BYTES);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&key_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A key readable by others still loads (with a warning on stderr).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(LinkIds::load_or_create(&key_path).is_ok());
        }

        // A wrong-sized key is refused, never silently replaced.
        fs::write(&key_path, b"too short").unwrap();
        let error = LinkIds::load_or_create(&key_path).err().unwrap();
        assert!(error.contains("expected 32 bytes"), "{error}");
        assert_eq!(fs::read(&key_path).unwrap(), b"too short");

        // An empty key (interrupted first start) gets an actionable message.
        fs::write(&key_path, b"").unwrap();
        let error = LinkIds::load_or_create(&key_path).err().unwrap();
        assert!(error.contains("delete it"), "{error}");
    }

    #[test]
    fn concurrent_first_starts_share_one_key() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("link.key");
        let ids: Vec<String> = std::thread::scope(|scope| {
            let starts: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        LinkIds::load_or_create(&key_path)
                            .unwrap()
                            .item("Trip/a.jpg")
                    })
                })
                .collect();
            starts
                .into_iter()
                .map(|start| start.join().unwrap())
                .collect()
        });
        assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
    }
}
