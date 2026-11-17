//! The album library: turns the root into albums using a [`Pattern`]. Terms
//! (root, marker, album, item, path, link id, scan, snapshot) are as defined in
//! docs/architecture.md "Terminology".
//!
//! Scans run in the background ([`Library::run_scanner`]); requests read the
//! latest [`Snapshot`] and never walk the root. Scans are idempotent and safe
//! to retry: they only read the disk, and a successful scan replaces the
//! snapshot in one step. A scan that fails (marker missing, root unreadable,
//! I/O errors from a disconnected disk or network share) keeps the albums and
//! link ids of the last successful scan and only marks storage unavailable, so
//! an outage never looks like an empty root. Failed scans are retried sooner,
//! with backoff, until one succeeds. Only the snapshots of scans that read the
//! whole root are handed over (to the thumbnails), and only when their items
//! changed: neither an outage nor a folder it couldn't read looks like items
//! gone.
//!
//! Scans poll instead of watching for changes (notify/inotify): watching misses
//! changes on NFS/SMB and from the Windows side of WSL2, and its polling
//! fallback is another full walk anyway.

use crate::config;
use crate::link_ids::LinkIds;
use crate::pattern::{Captures, Metadata, Pattern};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

/// A scan taking longer than this marks storage unavailable (hung mount).
const SCAN_TIMEOUT: Duration = Duration::from_secs(60);
/// Waits before retrying a failed first scan, e.g. a mount that appears a
/// moment after the container starts.
const FIRST_SCAN_RETRIES: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];
/// First retry after a failed scan while running; doubles up to the scan interval.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(1);

/// The placeholder that gives an album its name.
pub const ALBUM_NAME_PLACEHOLDER: &str = "album_name";
/// The type MP4, M4V and QuickTime originals are served as: they are all MP4
/// inside (see [`content_type`]).
pub const MP4: &str = "video/mp4";

pub struct Library {
    /// Canonicalized, so containment checks compare like with like.
    root: PathBuf,
    pattern: Pattern,
    marker: String,
    settle_time: Duration,
    /// Lowercase, so matching is case-insensitive.
    ignore: HashSet<String>,
    link_ids: LinkIds,
    snapshot: RwLock<Arc<Snapshot>>,
}

/// The albums and link ids of the last successful scan, plus the state of the
/// latest scan.
pub struct Snapshot {
    pub albums: Arc<Vec<Album>>,
    pub storage: StorageState,
    /// Where each album sits in `albums`, by link id.
    albums_by_link_id: Arc<HashMap<String, usize>>,
    /// Where each item sits in `albums`, by link id.
    items_by_link_id: Arc<HashMap<String, ItemPosition>>,
    skipped: Skipped,
}

/// An item's place in a snapshot's albums, valid for that snapshot only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ItemPosition {
    album: usize,
    item: usize,
}

/// Outcome of the latest scan. The reasons are generic on purpose: they are
/// shown on unauthenticated `/healthz`; details go to the log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StorageState {
    Available,
    MarkerMissing,
    Unreadable,
    ScanTimedOut,
}

impl StorageState {
    pub fn is_available(self) -> bool {
        self == Self::Available
    }
}

impl fmt::Display for StorageState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Available => "ok",
            Self::MarkerMissing => "storage not mounted (marker file missing)",
            Self::Unreadable => "storage unreadable",
            Self::ScanTimedOut => "storage scan timed out",
        })
    }
}

/// Entries a scan passed over, logged whenever the counts change. Hidden and
/// ignored names aren't counted: they are skipped by design.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct Skipped {
    symlinks: usize,
    non_utf8_names: usize,
    unreadable: usize,
    still_copying: usize,
}

#[derive(Debug)]
struct ScanError {
    state: StorageState,
    detail: String,
}

pub struct Album {
    /// Link id, used in URLs.
    pub id: String,
    /// The `{album_name}` placeholder, else the album's folder name; composed
    /// for display like all names and metadata (see [`composed`]).
    pub name: String,
    pub metadata: Metadata,
    pub items: Vec<Item>,
}

pub struct Item {
    /// Link id, used in URLs.
    pub id: String,
    /// Item path, e.g. `year=2024/Lake Trip/001_dock.jpg`.
    pub path: String,
    /// The whole name, e.g. `001_dock.jpg`, composed for display; a
    /// `{file_name}` placeholder may capture only part of it.
    pub file_name: String,
    pub kind: MediaKind,
    pub metadata: Metadata,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
}

impl MediaKind {
    /// The kind of item `file_name` is, if any (see [`content_type`]).
    pub fn from_file_name(file_name: &str) -> Option<Self> {
        let content_type = content_type(file_name)?;
        Some(if content_type.starts_with("video/") {
            Self::Video
        } else {
            Self::Image
        })
    }
}

/// The Content-Type an item's original is served with, or `None` if the file
/// isn't an item: only types browsers can display inline count. mime_guess
/// maps the extension, so spellings like `.jfif` need no upkeep here; the
/// other image and video types it knows (camera RAW, TIFF, HEIC, AVI, MKV)
/// stay out.
///
/// QuickTime and M4V files are MP4 inside and are served as `video/mp4`:
/// Chromium plays them only under that type, Firefox and Safari under either.
/// The type comes from the item's own name, never from a file it links to.
pub fn content_type(file_name: &str) -> Option<&'static str> {
    let path = Path::new(file_name);
    // Gzip-compressed SVG would be served without `Content-Encoding`, which
    // browsers can't show.
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svgz"))
    {
        return None;
    }
    match mime_guess::from_path(path).first_raw()? {
        image @ ("image/jpeg" | "image/png" | "image/apng" | "image/gif" | "image/webp"
        | "image/avif" | "image/jxl" | "image/bmp" | "image/svg+xml") => Some(image),
        MP4 | "video/x-m4v" | "video/quicktime" => Some(MP4),
        "video/webm" => Some("video/webm"),
        _ => None,
    }
}

impl Library {
    /// Opens the root and runs the first scan, retrying a failed one with
    /// backoff ([`FIRST_SCAN_RETRIES`]). Returns only after a successful scan,
    /// so a library is never served unscanned. A first scan that hangs fails
    /// at once: a walk blocked inside the OS can't be retried.
    pub async fn open(settings: &config::Storage, link_ids: LinkIds) -> Result<Arc<Self>, String> {
        let root = settings.root.canonicalize().map_err(|e| {
            format!(
                "storage root {}: {e} (in Docker, check the storage mount)",
                settings.root.display()
            )
        })?;
        let library = Arc::new(Self {
            root,
            pattern: Pattern::parse(&settings.pattern)?,
            marker: settings.marker.clone(),
            settle_time: settings.settle_time,
            ignore: settings
                .ignore
                .iter()
                .map(|name| name.to_lowercase())
                .collect(),
            link_ids,
            // Replaced by the first successful scan before `open` returns.
            snapshot: RwLock::new(Arc::new(Snapshot {
                albums: Arc::new(Vec::new()),
                storage: StorageState::Unreadable,
                albums_by_link_id: Arc::new(HashMap::new()),
                items_by_link_id: Arc::new(HashMap::new()),
                skipped: Skipped::default(),
            })),
        });
        let mut retries = FIRST_SCAN_RETRIES.iter();
        loop {
            let scan = library.spawn_scan();
            let outcome = tokio::time::timeout(SCAN_TIMEOUT, scan)
                .await
                .map_err(|_| {
                    format!("the first scan took over {SCAN_TIMEOUT:?}: is the mount hung?")
                })?;
            match outcome {
                Ok(Ok(())) => return Ok(library),
                Ok(Err(detail)) => match retries.next() {
                    Some(wait) => {
                        eprintln!("first scan failed, retrying in {wait:?}: {detail}");
                        tokio::time::sleep(*wait).await;
                    }
                    None => return Err(detail),
                },
                Err(panic) => return Err(format!("the first scan panicked: {panic}")),
            }
        }
    }

    /// The root, canonical.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The latest snapshot: cheap, never touches the disk.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The item path for an item link id, as of the last successful scan.
    pub fn item_path(&self, item_id: &str) -> Option<String> {
        let snapshot = self.snapshot();
        snapshot.item(item_id).map(|(_, item)| item.path.clone())
    }

    /// Scans now (blocking I/O). On failure the previous albums and link ids
    /// are kept and only the storage state changes. Running it again with the
    /// root unchanged gives the same snapshot.
    pub fn scan_now(&self) -> Result<(), String> {
        let previous = self.snapshot();
        match self.walk() {
            Ok((albums, skipped)) => {
                if skipped != previous.skipped {
                    eprintln!("scan skipped entries: {skipped:?}");
                }
                // Logged once, when albums disappear or the first scan finds none.
                if albums.is_empty()
                    && (!previous.albums.is_empty() || !previous.storage.is_available())
                {
                    eprintln!(
                        "scan found no albums in {}: check storage.pattern (its depth must match the folders)",
                        self.root.display()
                    );
                }
                self.replace_snapshot(Snapshot {
                    albums_by_link_id: Arc::new(index_albums(&albums)),
                    items_by_link_id: Arc::new(index_items(&albums)),
                    albums: Arc::new(albums),
                    storage: StorageState::Available,
                    skipped,
                });
                Ok(())
            }
            Err(error) => {
                self.set_storage_state(error.state);
                Err(error.detail)
            }
        }
    }

    /// Scans every `interval`; after a failed scan, retries sooner (starting at
    /// [`RETRY_AFTER_FAILURE`], doubling up to `interval`). Only one scan runs
    /// at a time. Runs until the process exits.
    ///
    /// `changed` is given the snapshot of the first scan that read the whole
    /// root, then of each later one whose items differ from the last given; a
    /// failed scan keeps the items, and one that couldn't read a folder lacks
    /// its items. A folder never readable holds back every hand-over. It is
    /// waited for before the next scan, so no newer snapshot exists while it
    /// runs: only a request that looked its item up before can ask for an
    /// item it wasn't given. A hung thumbnails folder stalls scans too.
    pub async fn run_scanner<Changed: Future<Output = ()>>(
        self: Arc<Self>,
        interval: Duration,
        mut changed: impl FnMut(Arc<Snapshot>) -> Changed,
    ) {
        let mut given: Option<Arc<Snapshot>> = None;
        let mut wait = interval;
        loop {
            let scanned = self.snapshot();
            if scanned.skipped.unreadable == 0
                && given
                    .as_ref()
                    .is_none_or(|given| !scanned.has_the_items_of(given))
            {
                given = Some(scanned.clone());
                changed(scanned).await;
            }
            tokio::time::sleep(wait).await;
            let before = self.snapshot();
            let mut scan = self.spawn_scan();
            // A hung mount blocks the walk inside the OS and can't be cancelled, so
            // report it and keep waiting instead of starting more scans that hang too.
            let outcome = match tokio::time::timeout(SCAN_TIMEOUT, &mut scan).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    eprintln!("storage scan is taking over {SCAN_TIMEOUT:?}: is the mount hung?");
                    self.set_storage_state_if_unchanged(&before, StorageState::ScanTimedOut);
                    scan.await
                }
            };
            let succeeded = match outcome {
                Ok(Ok(())) => true,
                Ok(Err(detail)) => {
                    eprintln!("storage scan failed: {detail}");
                    false
                }
                Err(panic) => {
                    eprintln!("storage scan panicked: {panic}");
                    self.set_storage_state(StorageState::Unreadable);
                    false
                }
            };
            wait = if succeeded {
                interval
            } else if wait == interval {
                RETRY_AFTER_FAILURE.min(interval)
            } else {
                (wait * 2).min(interval)
            };
        }
    }

    /// Resolves an item path to its file on disk. Returns `None` unless it is
    /// an item: matching the pattern, a media file, not hidden or ignored, and
    /// still inside the root after following symlinks.
    pub async fn item_file(&self, item_path: &str) -> Option<PathBuf> {
        let segments: Vec<&str> = item_path.split('/').collect();
        self.classify(&segments)?;
        let file = tokio::fs::canonicalize(self.root.join(item_path))
            .await
            .ok()?;
        let is_file = tokio::fs::metadata(&file).await.is_ok_and(|m| m.is_file());
        (is_file && file.starts_with(&self.root)).then_some(file)
    }

    fn spawn_scan(self: &Arc<Self>) -> tokio::task::JoinHandle<Result<(), String>> {
        let library = self.clone();
        tokio::task::spawn_blocking(move || library.scan_now())
    }

    fn replace_snapshot(&self, next: Snapshot) {
        *self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(next);
    }

    /// Keeps the albums and link ids, changing only the storage state.
    fn set_storage_state(&self, storage: StorageState) {
        let mut current = self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        *current = Arc::new(current.with_storage(storage));
    }

    /// Like [`Self::set_storage_state`], but only if no scan has replaced the
    /// snapshot since `expected` was read, so a scan finishing just as its
    /// timeout fires keeps its result.
    fn set_storage_state_if_unchanged(&self, expected: &Arc<Snapshot>, storage: StorageState) {
        let mut current = self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if Arc::ptr_eq(&current, expected) {
            *current = Arc::new(current.with_storage(storage));
        }
    }

    /// Walks the root and groups every item into its album. Blocking I/O.
    fn walk(&self) -> Result<(Vec<Album>, Skipped), ScanError> {
        self.check_marker()?;
        let depth = self.pattern.depth();
        // No `min_depth`: walkdir doesn't pass entries above it to `filter_entry`,
        // and hidden or ignored folders must be pruned before they're entered.
        let walker = WalkDir::new(&self.root)
            .max_depth(depth)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !self.is_hidden_or_ignored(&entry.file_name().to_string_lossy())
            });

        let now = SystemTime::now();
        let mut skipped = Skipped::default();
        let mut albums: BTreeMap<String, Album> = BTreeMap::new();
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) if err.depth() == 0 || err.io_error().is_some_and(is_storage_failure) => {
                    return Err(ScanError {
                        state: StorageState::Unreadable,
                        detail: err.to_string(),
                    });
                }
                Err(_) => {
                    skipped.unreadable += 1;
                    continue;
                }
            };
            if entry.path_is_symlink() {
                skipped.symlinks += 1;
                continue;
            }
            if entry.depth() != depth || !entry.file_type().is_file() {
                continue;
            }
            let Some(segments) = self.relative_segments(entry.path()) else {
                skipped.non_utf8_names += 1;
                continue;
            };
            let Some((kind, captured)) = self.classify(&segments) else {
                continue;
            };
            let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
            let Some(modified) = modified else {
                skipped.unreadable += 1;
                continue;
            };
            // A modification time in the future (a wrong camera clock) counts as settled.
            if now
                .duration_since(modified)
                .is_ok_and(|age| age < self.settle_time)
            {
                skipped.still_copying += 1;
                continue;
            }

            let (folders, last) = segments.split_at(segments.len() - 1);
            let file_name = last[0];
            let album_path = folders.join("/");
            let album = albums.entry(album_path.clone()).or_insert_with(|| {
                let metadata = composed_metadata(captured.album_metadata);
                Album {
                    id: self.link_ids.album(&album_path),
                    name: metadata
                        .get(ALBUM_NAME_PLACEHOLDER)
                        .cloned()
                        .unwrap_or_else(|| composed(folders[folders.len() - 1])),
                    metadata,
                    items: Vec::new(),
                }
            });
            let item_path = segments.join("/");
            album.items.push(Item {
                id: self.link_ids.item(&item_path),
                path: item_path,
                file_name: composed(file_name),
                kind,
                metadata: composed_metadata(captured.item_metadata),
            });
        }
        // The mount may have dropped mid-walk, leaving only part of the root.
        self.check_marker()?;
        Ok((
            refuse_shared_album_ids(albums.into_values().collect()),
            skipped,
        ))
    }

    /// The marker must be a regular file, not a symlink (symlinks are skipped).
    fn check_marker(&self) -> Result<(), ScanError> {
        let marker = self.root.join(&self.marker);
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_file() => Ok(()),
            Ok(_) => Err(ScanError {
                state: StorageState::MarkerMissing,
                detail: format!("{} must be a regular file", marker.display()),
            }),
            Err(err) if err.kind() == ErrorKind::NotFound => Err(ScanError {
                state: StorageState::MarkerMissing,
                detail: format!(
                    "{} not found: is the storage mounted? For a new root, create it with `touch`",
                    marker.display()
                ),
            }),
            Err(err) => Err(ScanError {
                state: StorageState::Unreadable,
                detail: format!("{}: {err}", marker.display()),
            }),
        }
    }

    /// The one rule deciding whether a path is an item.
    fn classify(&self, segments: &[&str]) -> Option<(MediaKind, Captures)> {
        let safe = segments
            .iter()
            .all(|s| !s.is_empty() && !s.contains(['\\', '\0']) && !self.is_hidden_or_ignored(s));
        if !safe {
            return None;
        }
        let kind = MediaKind::from_file_name(segments.last()?)?;
        Some((kind, self.pattern.matches(segments)?))
    }

    fn is_hidden_or_ignored(&self, name: &str) -> bool {
        name.starts_with('.') || self.ignore.contains(&name.to_lowercase())
    }

    /// Path segments below the root, or `None` for non UTF-8 names.
    fn relative_segments<'a>(&self, path: &'a Path) -> Option<Vec<&'a str>> {
        let relative = path.strip_prefix(&self.root).ok()?;
        relative.iter().map(|segment| segment.to_str()).collect()
    }
}

impl Snapshot {
    /// The album with this link id.
    pub fn album(&self, id: &str) -> Option<&Album> {
        self.albums.get(*self.albums_by_link_id.get(id)?)
    }

    /// The item with this link id and its album. Only link ids whose file is
    /// served count: a link id shared by two items is refused for both.
    pub fn item(&self, id: &str) -> Option<(&Album, &Item)> {
        let position = self.items_by_link_id.get(id)?;
        let album = self.albums.get(position.album)?;
        Some((album, album.items.get(position.item)?))
    }

    /// The link ids of the items served, as [`Self::item`] takes them, in no
    /// order.
    pub fn item_ids(&self) -> impl Iterator<Item = &str> {
        self.items_by_link_id.keys().map(String::as_str)
    }

    /// Whether this snapshot has the same items as `other`, by link id.
    fn has_the_items_of(&self, other: &Self) -> bool {
        let (mine, theirs) = (&self.items_by_link_id, &other.items_by_link_id);
        mine.len() == theirs.len() && mine.keys().all(|id| theirs.contains_key(id))
    }

    fn with_storage(&self, storage: StorageState) -> Self {
        Self {
            albums: self.albums.clone(),
            albums_by_link_id: self.albums_by_link_id.clone(),
            items_by_link_id: self.items_by_link_id.clone(),
            storage,
            skipped: self.skipped,
        }
    }
}

/// Errors that mean the storage itself is failing (disconnected disk, network
/// share down), as opposed to one unreadable file.
fn is_storage_failure(err: &io::Error) -> bool {
    const EIO: i32 = 5; // same value on Linux and macOS
    matches!(
        err.kind(),
        ErrorKind::TimedOut | ErrorKind::NotConnected | ErrorKind::StaleNetworkFileHandle
    ) || err.raw_os_error() == Some(EIO)
}

/// Link ids that appear more than once, logged: such albums or items are
/// refused rather than showing the wrong one. Album ids are computed from
/// NFC-normalised paths, so two folders whose names differ only in Unicode
/// form (possible on Linux) share one; item ids practically never do.
fn shared_link_ids<'a>(kind: &str, ids: impl Iterator<Item = &'a str>) -> HashSet<String> {
    let mut seen = HashSet::new();
    let shared: HashSet<String> = ids
        .filter(|id| !seen.insert(*id))
        .map(str::to_string)
        .collect();
    for id in &shared {
        eprintln!("{kind} link id {id} is shared by several {kind}s; they are refused");
    }
    shared
}

/// A name as shown on the pages: composed (NFC), as macOS and SMB clients often
/// write names decomposed (`e` plus a combining accent), which some fonts draw
/// misplaced. Item paths keep the name as on disk, so files still open.
fn composed(name: &str) -> String {
    name.nfc().collect()
}

fn composed_metadata(metadata: Metadata) -> Metadata {
    metadata
        .into_iter()
        .map(|(placeholder, value)| (placeholder, composed(&value)))
        .collect()
}

fn refuse_shared_album_ids(albums: Vec<Album>) -> Vec<Album> {
    let shared = shared_link_ids("album", albums.iter().map(|album| album.id.as_str()));
    albums
        .into_iter()
        .filter(|album| !shared.contains(&album.id))
        .collect()
}

/// Each album's position by link id. Albums sharing one were already refused
/// (see [`refuse_shared_album_ids`]).
fn index_albums(albums: &[Album]) -> HashMap<String, usize> {
    let positions = albums.iter().enumerate();
    positions
        .map(|(position, album)| (album.id.clone(), position))
        .collect()
}

/// Each item's position by link id, without shared link ids.
fn index_items(albums: &[Album]) -> HashMap<String, ItemPosition> {
    let items = || {
        albums.iter().enumerate().flat_map(|(album_index, album)| {
            album
                .items
                .iter()
                .enumerate()
                .map(move |(item_index, item)| {
                    let position = ItemPosition {
                        album: album_index,
                        item: item_index,
                    };
                    (position, item)
                })
        })
    };
    let shared = shared_link_ids("item", items().map(|(_, item)| item.id.as_str()));
    items()
        .filter(|(_, item)| !shared.contains(&item.id))
        .map(|(position, item)| (item.id.clone(), position))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn settings(root: &Path, pattern: &str) -> config::Storage {
        config::Storage {
            backend: config::StorageBackend::Local,
            root: root.to_path_buf(),
            pattern: pattern.into(),
            marker: ".albums-root".into(),
            rescan_interval: Duration::from_secs(30),
            settle_time: Duration::ZERO,
            ignore: vec!["@eaDir".into(), "$RECYCLE.BIN".into()],
            link_key: root.join("unused.key"),
        }
    }

    async fn library(base: &Path, settings: &config::Storage) -> Arc<Library> {
        let link_ids = LinkIds::load_or_create(&base.join("link.key")).unwrap();
        Library::open(settings, link_ids).await.unwrap()
    }

    /// Builds `<tmp>/root/` with the marker, `Trip/{a.svg, .hidden.svg, notes.txt,
    /// escape.svg -> ../outside.svg}`, `@eaDir/x.svg` and `$Recycle.Bin/y.svg`.
    /// The `TempDir` deletes everything when dropped, even if the test panics.
    async fn fixture() -> (tempfile::TempDir, Arc<Library>) {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        let trip = root.join("Trip");
        fs::create_dir_all(&trip).unwrap();
        fs::write(root.join(".albums-root"), "").unwrap();
        for file in ["a.svg", ".hidden.svg", "notes.txt"] {
            fs::write(trip.join(file), "x").unwrap();
        }
        for ignored in ["@eaDir", "$Recycle.Bin"] {
            fs::create_dir_all(root.join(ignored)).unwrap();
            fs::write(root.join(ignored).join("x.svg"), "x").unwrap();
        }
        let outside = base.path().join("outside.svg");
        fs::write(&outside, "secret").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, trip.join("escape.svg")).unwrap();
            // Inside an ignored folder: pruned, so never counted.
            std::os::unix::fs::symlink(&outside, root.join("@eaDir/link.svg")).unwrap();
        }
        let library = library(base.path(), &settings(&root, "{album_name}/{file_name}")).await;
        (base, library)
    }

    /// Files of the types browsers show are items, under any of their usual
    /// extensions; other images and videos (camera RAW, TIFF, HEIC, AVI, SVGZ)
    /// are not.
    #[test]
    fn media_types_browsers_show_are_items() {
        for (name, kind) in [
            ("a.jpg", MediaKind::Image),
            ("IMG_0001.JPEG", MediaKind::Image),
            ("a.jfif", MediaKind::Image),
            ("a.jpe", MediaKind::Image),
            ("a.png", MediaKind::Image),
            ("a.apng", MediaKind::Image),
            ("a.gif", MediaKind::Image),
            ("a.webp", MediaKind::Image),
            ("a.avif", MediaKind::Image),
            ("a.bmp", MediaKind::Image),
            ("a.svg", MediaKind::Image),
            ("a.jxl", MediaKind::Image),
            ("a.mp4", MediaKind::Video),
            ("a.m4v", MediaKind::Video),
            ("IMG_0002.MOV", MediaKind::Video),
            ("a.qt", MediaKind::Video),
            ("a.webm", MediaKind::Video),
            ("2024.07.01 lake.mp4", MediaKind::Video),
        ] {
            assert!(MediaKind::from_file_name(name) == Some(kind), "{name}");
            // Parsed for every request of an original, so it must never fail.
            let served = content_type(name).unwrap();
            assert!(served.parse::<mime_guess::Mime>().is_ok(), "{name}");
        }
        for name in [
            "a.dng",
            "a.CR2",
            "a.nef",
            "a.arw",
            "a.tif",
            "a.tiff",
            "a.heic",
            "a.avi",
            "a.mkv",
            "a.ts",
            "a.svgz", // gzip-compressed SVG, served without saying so
            "notes.txt",
            "README",
            ".jpg",
            "a.jpg.part",
        ] {
            assert!(MediaKind::from_file_name(name).is_none(), "{name}");
        }
    }

    #[tokio::test]
    async fn lists_only_items_and_prunes_ignored_folders() {
        let (_base, library) = fixture().await;
        let snapshot = library.snapshot();
        assert!(snapshot.storage.is_available());
        assert_eq!(
            snapshot.albums.len(),
            1,
            "ignored folders, case-insensitively"
        );
        assert_eq!(snapshot.albums[0].name, "Trip");
        let paths: Vec<_> = snapshot.albums[0]
            .items
            .iter()
            .map(|i| i.path.as_str())
            .collect();
        assert_eq!(paths, ["Trip/a.svg"]);
        #[cfg(unix)]
        assert_eq!(
            snapshot.skipped.symlinks, 1,
            "the symlink in @eaDir was pruned"
        );
    }

    #[tokio::test]
    async fn scans_are_idempotent() {
        let (_base, library) = fixture().await;
        let ids = |snapshot: &Snapshot| -> Vec<(String, Vec<String>)> {
            let albums = snapshot.albums.iter();
            albums
                .map(|a| (a.id.clone(), a.items.iter().map(|i| i.id.clone()).collect()))
                .collect()
        };
        let first = library.snapshot();
        library.scan_now().unwrap();
        library.scan_now().unwrap();
        let again = library.snapshot();
        assert_eq!(ids(&again), ids(&first));
        assert_eq!(again.albums_by_link_id, first.albums_by_link_id);
        assert_eq!(again.items_by_link_id, first.items_by_link_id);
        assert_eq!(again.storage, first.storage);
    }

    #[tokio::test]
    async fn album_names_and_metadata_come_from_placeholders() {
        let base = tempfile::tempdir().unwrap();
        let album = base.path().join("year=2024/Lake");
        fs::create_dir_all(&album).unwrap();
        fs::write(base.path().join(".albums-root"), "").unwrap();
        fs::write(album.join("001_a.svg"), "x").unwrap();
        let pattern = "year={year}/{album_name}/{seq}_{file_name}";
        let library = library(base.path(), &settings(base.path(), pattern)).await;
        let albums = library.snapshot().albums.clone();
        assert_eq!(albums[0].name, "Lake");
        assert_eq!(albums[0].metadata["year"], "2024");
        let item = &albums[0].items[0];
        assert_eq!(item.file_name, "001_a.svg");
        assert_eq!(item.metadata["seq"], "001");
        assert_eq!(item.metadata["file_name"], "a.svg");
    }

    /// macOS and SMB clients often write names decomposed (NFD): `e` plus a
    /// combining accent, which some fonts draw misplaced.
    #[tokio::test]
    async fn names_are_shown_composed_but_paths_stay_as_on_disk() {
        let base = tempfile::tempdir().unwrap();
        let album = base.path().join("Cafe\u{301}");
        fs::create_dir_all(&album).unwrap();
        fs::write(base.path().join(".albums-root"), "").unwrap();
        fs::write(album.join("cre\u{300}me.svg"), "x").unwrap();
        let pattern = "{album_name}/{file_name}";
        let library = library(base.path(), &settings(base.path(), pattern)).await;
        let albums = library.snapshot().albums.clone();
        assert_eq!(albums[0].name, "Caf\u{e9}");
        assert_eq!(albums[0].metadata["album_name"], "Caf\u{e9}");
        let item = &albums[0].items[0];
        assert_eq!(item.file_name, "cr\u{e8}me.svg");
        assert_eq!(item.metadata["file_name"], "cr\u{e8}me.svg");
        assert_eq!(item.path, "Cafe\u{301}/cre\u{300}me.svg");
        assert!(library.item_file(&item.path).await.is_some());
    }

    #[tokio::test]
    async fn storage_outages_keep_the_last_good_albums() {
        let (base, library) = fixture().await;
        let item_id = library.snapshot().albums[0].items[0].id.clone();

        // Unmounted: the root's folder is still there but empty (marker gone).
        fs::remove_file(base.path().join("root/.albums-root")).unwrap();
        let error = library.scan_now().unwrap_err();
        assert!(error.contains("is the storage mounted"), "{error}");
        let snapshot = library.snapshot();
        assert_eq!(snapshot.storage, StorageState::MarkerMissing);
        assert_eq!(snapshot.albums.len(), 1, "albums kept");
        assert!(library.item_path(&item_id).is_some(), "link ids kept");

        // Disconnected: the root itself is gone.
        let root = base.path().join("root");
        fs::rename(&root, base.path().join("unplugged")).unwrap();
        assert!(library.scan_now().is_err());
        assert!(!library.snapshot().storage.is_available());
        assert!(library.item_path(&item_id).is_some());

        // Back: everything works again on the next scan (retrying is safe).
        fs::rename(base.path().join("unplugged"), &root).unwrap();
        fs::write(root.join(".albums-root"), "").unwrap();
        library.scan_now().unwrap();
        assert!(library.snapshot().storage.is_available());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_marker_must_be_a_regular_file() {
        let (base, library) = fixture().await;
        let root = base.path().join("root");
        fs::remove_file(root.join(".albums-root")).unwrap();
        fs::write(base.path().join("elsewhere"), "").unwrap();
        std::os::unix::fs::symlink(base.path().join("elsewhere"), root.join(".albums-root"))
            .unwrap();
        let error = library.scan_now().unwrap_err();
        assert!(error.contains("must be a regular file"), "{error}");
    }

    #[tokio::test]
    async fn a_timeout_never_overwrites_a_newer_result() {
        let (_base, library) = fixture().await;
        let before = library.snapshot();
        library.scan_now().unwrap(); // a scan finished just before its timeout fired
        library.set_storage_state_if_unchanged(&before, StorageState::ScanTimedOut);
        assert!(library.snapshot().storage.is_available());

        let current = library.snapshot();
        library.set_storage_state_if_unchanged(&current, StorageState::ScanTimedOut);
        assert_eq!(library.snapshot().storage, StorageState::ScanTimedOut);
    }

    #[tokio::test]
    async fn first_scan_waits_for_a_late_mount() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        fs::create_dir_all(root.join("Trip")).unwrap();
        fs::write(root.join("Trip/a.svg"), "x").unwrap();
        let settings = settings(&root, "{album_name}/{file_name}");
        let mount_later = {
            let marker = root.join(".albums-root");
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                fs::write(marker, "").unwrap();
            })
        };
        let library = library(base.path(), &settings).await;
        mount_later.await.unwrap();
        assert_eq!(library.snapshot().albums.len(), 1);
    }

    #[tokio::test]
    async fn settle_time_skips_files_being_copied_but_not_future_dates() {
        let (base, _) = fixture().await;
        let root = base.path().join("root");
        let mut settings = settings(&root, "{album_name}/{file_name}");
        settings.settle_time = Duration::from_secs(3600);
        let future = fs::File::create(root.join("Trip/future.svg")).unwrap();
        future
            .set_modified(SystemTime::now() + Duration::from_secs(365 * 24 * 3600))
            .unwrap();
        let library = library(base.path(), &settings).await;
        let snapshot = library.snapshot();
        let paths: Vec<_> = snapshot.albums[0]
            .items
            .iter()
            .map(|i| i.path.as_str())
            .collect();
        assert_eq!(
            paths,
            ["Trip/future.svg"],
            "a.svg was just written; future.svg is shown"
        );
        assert_eq!(snapshot.skipped.still_copying, 1);
    }

    /// The scanner hands over the first scan's snapshot, then each one whose
    /// items changed, but only from scans that read the whole root: neither
    /// an outage nor an album it couldn't read looks like items gone. Scans
    /// that change nothing hand over none.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_scanner_hands_over_each_change_of_items() {
        use std::os::unix::fs::PermissionsExt;
        let (base, library) = fixture().await;
        let trip = base.path().join("root/Trip");
        let home = base.path().join("root/Home");
        fs::create_dir(&home).unwrap();
        fs::write(home.join("c.svg"), "x").unwrap();
        library.scan_now().unwrap();
        let marker = base.path().join("root/.albums-root");
        let (sender, mut changes) = tokio::sync::mpsc::unbounded_channel();
        let scanner = tokio::spawn(library.clone().run_scanner(
            Duration::from_millis(20),
            move |snapshot: Arc<Snapshot>| {
                let mut ids: Vec<_> = snapshot.item_ids().map(str::to_owned).collect();
                ids.sort();
                sender.send(ids).unwrap();
                std::future::ready(())
            },
        ));
        let ids = |paths: &[&str]| {
            let mut ids: Vec<_> = paths
                .iter()
                .map(|path| library.link_ids.item(path))
                .collect();
            ids.sort();
            ids
        };
        let next = async |changes: &mut tokio::sync::mpsc::UnboundedReceiver<_>| {
            tokio::time::timeout(Duration::from_secs(5), changes.recv())
                .await
                .expect("a change handed over")
                .unwrap()
        };
        // Waits for a snapshot that `holds`, then a few more scans.
        let after = async |holds: &dyn Fn(&Snapshot) -> bool| {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !holds(&library.snapshot()) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the scanner got there");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(next(&mut changes).await, ids(&["Trip/a.svg", "Home/c.svg"]));

        fs::remove_file(&marker).unwrap();
        after(&|snapshot| !snapshot.storage.is_available()).await;
        fs::write(&marker, "").unwrap();
        after(&|snapshot| snapshot.storage.is_available()).await;
        assert!(changes.try_recv().is_err(), "an outage handed over");

        fs::set_permissions(&home, fs::Permissions::from_mode(0o000)).unwrap();
        after(&|snapshot| snapshot.skipped.unreadable > 0).await;
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        after(&|snapshot| snapshot.skipped.unreadable == 0).await;
        assert!(changes.try_recv().is_err(), "an unread album handed over");

        fs::write(trip.join("b.svg"), "x").unwrap();
        let all = ["Trip/a.svg", "Trip/b.svg", "Home/c.svg"];
        assert_eq!(next(&mut changes).await, ids(&all));
        fs::remove_file(trip.join("a.svg")).unwrap();
        assert_eq!(next(&mut changes).await, ids(&all[1..]));
        // As many items as before, but another one.
        fs::rename(trip.join("b.svg"), trip.join("d.svg")).unwrap();
        assert_eq!(next(&mut changes).await, ids(&["Trip/d.svg", "Home/c.svg"]));
        scanner.abort();
    }

    #[tokio::test]
    async fn new_files_appear_with_the_next_scan() {
        let (base, library) = fixture().await;
        fs::write(base.path().join("root/Trip/b.svg"), "x").unwrap();
        let b = library.link_ids.item("Trip/b.svg");
        assert_eq!(library.item_path(&b), None);
        library.scan_now().unwrap();
        assert_eq!(library.item_path(&b).as_deref(), Some("Trip/b.svg"));
        assert_eq!(library.item_path("not-a-link-id"), None);
    }

    #[test]
    fn albums_sharing_a_link_id_are_refused() {
        let album = |id: &str, name: &str| Album {
            id: id.into(),
            name: name.into(),
            metadata: Metadata::new(),
            items: Vec::new(),
        };
        let kept = refuse_shared_album_ids(vec![
            album("a", "Caf\u{e9}"),
            album("b", "Lake"),
            album("a", "Cafe\u{301}"),
        ]);
        let names: Vec<_> = kept.iter().map(|album| album.name.as_str()).collect();
        assert_eq!(names, ["Lake"]);
    }

    #[tokio::test]
    async fn item_file_rejects_non_items() {
        let (_base, library) = fixture().await;
        assert!(library.item_file("Trip/a.svg").await.is_some());
        for path in [
            "Trip/escape.svg",    // symlink out of the root
            "Trip/.hidden.svg",   // hidden
            "Trip/notes.txt",     // not media
            "Trip/missing.svg",   // does not exist
            "@eaDir/x.svg",       // ignored folder
            "$Recycle.Bin/y.svg", // ignored as "$RECYCLE.BIN", whatever the case
            "../outside.svg",     // traversal
            "Trip/../../outside.svg",
            "/etc/a.svg",
            "a.svg", // wrong depth for the pattern
        ] {
            assert!(
                library.item_file(path).await.is_none(),
                "{path} should be rejected"
            );
        }
    }
}
