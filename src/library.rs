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
use crate::{natural_order, routes};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};
use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfc_quick};
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
    /// What the album list can be sorted by (see [`sort_keys`]).
    sort_keys: Vec<SortKey>,
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

/// An item's place in its album, for its page: between the items before and
/// after it, and its number there.
pub struct ItemPlace<'a> {
    pub album: &'a Album,
    pub item: &'a Item,
    /// The item before it, if it isn't the album's first.
    pub previous: Option<&'a Item>,
    /// The item after it, if it isn't the album's last.
    pub next: Option<&'a Item>,
    /// Where it sits in `album.items`, from 0.
    index: usize,
}

impl ItemPlace<'_> {
    /// Its place in the album as people count, from 1.
    pub fn number(&self) -> usize {
        self.index + 1
    }

    /// How many items its album holds, as its grid shows them.
    pub fn count(&self) -> usize {
        self.album.items.len()
    }
}

/// How the album list is sorted, as its URL asks (see [`Snapshot::album_sort`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AlbumSort {
    pub key: SortKey,
    pub descending: bool,
}

/// What the album list can be sorted by. Values compare in [`natural_order`],
/// and ties keep folder order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SortKey {
    /// The order of their folders: the default.
    Folder,
    Name,
    /// A placeholder of the albums' folders, e.g. `year`.
    Placeholder(String),
    /// When their newest item was created (see [`Album::newest_item`]).
    Created,
}

impl Default for AlbumSort {
    fn default() -> Self {
        Self {
            key: SortKey::Folder,
            descending: false,
        }
    }
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

impl Album {
    /// When its newest item was created (see [`Item::created`]), if it has
    /// any: from the items it lists, so never from one it refused.
    pub fn newest_item(&self) -> Option<SystemTime> {
        self.items.iter().map(|item| item.created).max()
    }
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
    /// When its file was created, or last modified where creation times
    /// aren't kept: on some file systems, and with Rust's standard library on
    /// musl (so in the Docker image), which reads them only through glibc.
    pub created: SystemTime,
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
        let pattern = Pattern::parse(&settings.pattern)?;
        let library = Arc::new(Self {
            root,
            sort_keys: sort_keys(&pattern),
            pattern,
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

    /// What the album list can be sorted by: the same for every snapshot, as
    /// the pattern is.
    pub fn sort_keys(&self) -> &[SortKey] {
        &self.sort_keys
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
                self.replace_snapshot(Snapshot::new(albums, skipped));
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
            .sort_by(|a, b| by_name(a.file_name(), b.file_name()))
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !self.is_hidden_or_ignored(&entry.file_name().to_string_lossy())
            });

        let now = SystemTime::now();
        let mut skipped = Skipped::default();
        let mut albums: Vec<Album> = Vec::new();
        // Where each album sits in `albums`, by album path: albums keep the
        // order the walk finds them in.
        let mut positions: HashMap<String, usize> = HashMap::new();
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
            // The file system's details: not the item's metadata, which come
            // from placeholders.
            let file_info = entry.metadata().ok();
            let modified = file_info.as_ref().and_then(|info| info.modified().ok());
            let (Some(file_info), Some(modified)) = (file_info, modified) else {
                skipped.unreadable += 1;
                continue;
            };
            let created = file_info.created().unwrap_or(modified);
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
            let position = *positions
                .entry(album_path)
                .or_insert_with_key(|album_path| {
                    let metadata = composed_metadata(captured.album_metadata);
                    albums.push(Album {
                        id: self.link_ids.album(album_path),
                        name: metadata
                            .get(ALBUM_NAME_PLACEHOLDER)
                            .cloned()
                            .unwrap_or_else(|| composed(folders[folders.len() - 1]).into_owned()),
                        metadata,
                        items: Vec::new(),
                    });
                    albums.len() - 1
                });
            let album = &mut albums[position];
            let item_path = segments.join("/");
            album.items.push(Item {
                id: self.link_ids.item(&item_path),
                path: item_path,
                file_name: composed(file_name).into_owned(),
                kind,
                metadata: composed_metadata(captured.item_metadata),
                created,
            });
        }
        // The mount may have dropped mid-walk, leaving only part of the root.
        self.check_marker()?;
        let albums = refuse_shared_item_ids(refuse_shared_album_ids(albums));
        Ok((albums, skipped))
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
    /// The snapshot of a successful scan that found `albums`.
    fn new(albums: Vec<Album>, skipped: Skipped) -> Self {
        Self {
            albums_by_link_id: Arc::new(index_albums(&albums)),
            items_by_link_id: Arc::new(index_items(&albums)),
            albums: Arc::new(albums),
            storage: StorageState::Available,
            skipped,
        }
    }

    /// The album with this link id.
    pub fn album(&self, id: &str) -> Option<&Album> {
        self.albums.get(*self.albums_by_link_id.get(id)?)
    }

    /// The item with this link id and its album. Only link ids whose file is
    /// served count: a link id shared by two items is refused for both.
    pub fn item(&self, id: &str) -> Option<(&Album, &Item)> {
        let (album, index) = self.item_position(id)?;
        Some((album, album.items.get(index)?))
    }

    /// The item with this link id, as [`Self::item`] finds it, in its place in
    /// its album.
    pub fn item_place(&self, id: &str) -> Option<ItemPlace<'_>> {
        let (album, index) = self.item_position(id)?;
        let (before, from_item) = album.items.split_at_checked(index)?;
        let (item, after) = from_item.split_first()?;
        Some(ItemPlace {
            album,
            item,
            previous: before.last(),
            next: after.first(),
            index,
        })
    }

    /// The album of the item with this link id, if it is served, and the
    /// item's place in `album.items`.
    fn item_position(&self, id: &str) -> Option<(&Album, usize)> {
        let position = self.items_by_link_id.get(id)?;
        Some((self.albums.get(position.album)?, position.item))
    }

    /// The albums in `sort`'s order. The sort is stable and by key only, so
    /// ties keep folder order either way; values are composed already, and
    /// compare without allocating.
    pub fn sorted_albums(&self, sort: &AlbumSort) -> Vec<&Album> {
        fn value<'a>(album: &'a Album, placeholder: &str) -> &'a str {
            album.metadata.get(placeholder).map_or("", String::as_str)
        }
        let ordered = |ordering: Ordering| {
            if sort.descending {
                ordering.reverse()
            } else {
                ordering
            }
        };
        let mut albums: Vec<&Album> = self.albums.iter().collect();
        match &sort.key {
            SortKey::Folder if sort.descending => albums.reverse(),
            SortKey::Folder => {}
            SortKey::Name => {
                albums.sort_by(|a, b| ordered(natural_order::compare(&a.name, &b.name)));
            }
            SortKey::Placeholder(placeholder) => albums.sort_by(|a, b| {
                ordered(natural_order::compare(
                    value(a, placeholder),
                    value(b, placeholder),
                ))
            }),
            // Each album's date is found once, not at every comparison.
            SortKey::Created => {
                let mut dated: Vec<_> = albums
                    .iter()
                    .map(|album| (album.newest_item(), *album))
                    .collect();
                dated.sort_by(|(a, _), (b, _)| ordered(a.cmp(b)));
                albums = dated.into_iter().map(|(_, album)| album).collect();
            }
        }
        albums
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
/// refused rather than showing the wrong one. Link ids are computed from
/// NFC-normalised paths, so two folders, or two files in one folder, whose
/// names differ only in Unicode form (possible on Linux) share one.
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

/// A name as shown on the pages and as sorted: composed (NFC), as macOS and
/// SMB clients often write names decomposed (`e` plus a combining accent),
/// which some fonts draw misplaced. Item paths keep the name as on disk, so
/// files still open. Most names are composed already, which a quick check
/// tells without building another string.
fn composed(name: &str) -> Cow<'_, str> {
    match is_nfc_quick(name.chars()) {
        IsNormalized::Yes => Cow::Borrowed(name),
        IsNormalized::No | IsNormalized::Maybe => Cow::Owned(name.nfc().collect()),
    }
}

fn composed_metadata(metadata: Metadata) -> Metadata {
    metadata
        .into_iter()
        .map(|(placeholder, value)| (placeholder, composed(&value).into_owned()))
        .collect()
}

/// What the album list can be sorted by: folder, name, each placeholder of the
/// pattern's folders in its order, and creation. `{album_name}` is left out,
/// as name covers it, and so is a placeholder whose name is one of the fixed
/// sorts' words in the URL, which the log says once.
fn sort_keys(pattern: &Pattern) -> Vec<SortKey> {
    let fixed = [SortKey::Folder, SortKey::Name, SortKey::Created];
    let placeholders = pattern
        .album_placeholders()
        .filter(|placeholder| *placeholder != ALBUM_NAME_PLACEHOLDER)
        .map(|placeholder| SortKey::Placeholder(placeholder.to_string()))
        .filter(|key| {
            let word = routes::sort_word(key);
            let clashes = fixed.iter().any(|fixed| routes::sort_word(fixed) == word);
            if clashes {
                eprintln!("placeholder {{{word}}} doesn't sort the album list: \"{word}\" is one of its own sorts");
            }
            !clashes
        });
    let mut keys: Vec<_> = fixed[..2].to_vec();
    keys.extend(placeholders);
    keys.push(SortKey::Created);
    keys
}

/// The order of a folder's entries in the walk: by name, in [`natural_order`].
/// walkdir sorts each folder's entries and walks depth first, so albums and
/// their items come out in that order, folder by folder: the order pages list
/// them in, and that an item page's previous and next follow. Names compare
/// [`composed`], so one sorts the same in either Unicode form, as Unicode asks
/// of equivalent text; two that differ only in form fall back to their bytes.
/// Names that aren't UTF-8 (skipped anyway) come last, in byte order, so the
/// order stays total, as sorting requires.
fn by_name(a: &OsStr, b: &OsStr) -> Ordering {
    match (a.to_str(), b.to_str()) {
        (Some(a), Some(b)) => {
            natural_order::compare(&composed(a), &composed(b)).then_with(|| a.cmp(b))
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

/// The albums without their items that share a link id (see
/// [`shared_link_ids`]), so an album lists only items that have a page: its
/// tiles' `id`s stay unique, and its item numbers and each item's previous and
/// next count only those. An album left with no items stays, empty: it is a
/// folder the scan found, and the log says why its items are gone.
fn refuse_shared_item_ids(mut albums: Vec<Album>) -> Vec<Album> {
    let items = albums.iter().flat_map(|album| &album.items);
    let shared = shared_link_ids("item", items.map(|item| item.id.as_str()));
    for album in &mut albums {
        album.items.retain(|item| !shared.contains(&item.id));
    }
    albums
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

/// Each item's position by link id. Items sharing one were already refused
/// (see [`refuse_shared_item_ids`]).
fn index_items(albums: &[Album]) -> HashMap<String, ItemPosition> {
    let albums = albums.iter().enumerate();
    albums
        .flat_map(|(album_index, album)| {
            let items = album.items.iter().enumerate();
            items.map(move |(item_index, item)| {
                let position = ItemPosition {
                    album: album_index,
                    item: item_index,
                };
                (item.id.clone(), position)
            })
        })
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

    /// Albums and items are listed as people count: numbers by value, letters
    /// regardless of case.
    #[tokio::test]
    async fn albums_and_items_come_in_natural_order() {
        let base = tempfile::tempdir().unwrap();
        fs::write(base.path().join(".albums-root"), "").unwrap();
        for album in ["Trip 10", "Trip 9", "lake"] {
            fs::create_dir_all(base.path().join(album)).unwrap();
        }
        for item in ["IMG_10.svg", "IMG_9.svg", "img_1.svg", "IMG_009b.svg"] {
            fs::write(base.path().join("lake").join(item), "x").unwrap();
        }
        fs::write(base.path().join("Trip 10/a.svg"), "x").unwrap();
        fs::write(base.path().join("Trip 9/a.svg"), "x").unwrap();
        let pattern = "{album_name}/{file_name}";
        let library = library(base.path(), &settings(base.path(), pattern)).await;
        let snapshot = library.snapshot();
        let names: Vec<_> = snapshot.albums.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["lake", "Trip 9", "Trip 10"]);
        let items = snapshot.albums[0].items.iter();
        let files: Vec<_> = items.map(|i| i.file_name.as_str()).collect();
        assert_eq!(
            files,
            ["img_1.svg", "IMG_9.svg", "IMG_009b.svg", "IMG_10.svg"]
        );
    }

    /// The album list sorts by folder, name, the pattern's folder placeholders
    /// in its order, and creation; `{album_name}` is name, and a placeholder
    /// named like a fixed sort is left out.
    #[test]
    fn the_album_list_sorts_by_the_patterns_placeholders() {
        let pattern = Pattern::parse("{year}/{name}/{date}_{album_name}/{file_name}").unwrap();
        let date = SortKey::Placeholder("date".into());
        let year = SortKey::Placeholder("year".into());
        assert_eq!(
            sort_keys(&pattern),
            [SortKey::Folder, SortKey::Name, year, date, SortKey::Created]
        );
    }

    /// Albums sort by when their newest item was created: here `Zoo`, as its
    /// second file came last. Files are written 20ms apart, past the clocks of
    /// the file systems tests run on (APFS, ext4, tmpfs; HFS+ and FAT keep
    /// coarser times), so creation and modification times agree on the order.
    #[tokio::test]
    async fn albums_sort_by_their_newest_item() {
        let base = tempfile::tempdir().unwrap();
        fs::write(base.path().join(".albums-root"), "").unwrap();
        for file in ["Zoo/a.svg", "Lake/a.svg", "attic/a.svg", "Zoo/b.svg"] {
            let file = base.path().join(file);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, "x").unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        let pattern = "{album_name}/{file_name}";
        let library = library(base.path(), &settings(base.path(), pattern)).await;
        let snapshot = library.snapshot();
        for (descending, expected) in [
            (false, ["Lake", "attic", "Zoo"]),
            (true, ["Zoo", "attic", "Lake"]),
        ] {
            let sort = AlbumSort {
                key: SortKey::Created,
                descending,
            };
            let albums = snapshot.sorted_albums(&sort);
            let names: Vec<_> = albums.iter().map(|album| album.name.as_str()).collect();
            assert_eq!(names, expected, "descending: {descending}");
        }
    }

    /// A name sorts the same whether it is stored composed (`É`) or decomposed
    /// (`E` plus an accent), as Unicode asks of equivalent text.
    #[tokio::test]
    async fn names_sort_alike_in_either_unicode_form() {
        let base = tempfile::tempdir().unwrap();
        fs::write(base.path().join(".albums-root"), "").unwrap();
        fs::create_dir_all(base.path().join("lake")).unwrap();
        for item in ["Zoo.svg", "E\u{301}cole.svg", "Eden.svg", "\u{c9}tude.svg"] {
            fs::write(base.path().join("lake").join(item), "x").unwrap();
        }
        let pattern = "{album_name}/{file_name}";
        let library = library(base.path(), &settings(base.path(), pattern)).await;
        let snapshot = library.snapshot();
        let items = snapshot.albums[0].items.iter();
        let files: Vec<_> = items.map(|i| i.file_name.as_str()).collect();
        assert_eq!(
            files,
            ["Eden.svg", "Zoo.svg", "\u{c9}cole.svg", "\u{c9}tude.svg"]
        );
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

    /// Items sharing a link id are refused, as albums are: their album lists
    /// neither, so its tiles, item numbers and previous and next all count
    /// only items that have a page. An album whose items are all refused stays,
    /// empty: it is a folder the scan found.
    #[test]
    fn items_sharing_a_link_id_are_refused() {
        // The shared items are the newest: refused, they mustn't date the album.
        let created = |id: &str| {
            let seconds = if id == "shared" { 9 } else { id.len() as u64 };
            SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
        };
        let item = |id: &str| Item {
            id: id.into(),
            path: format!("Trip/{id}.svg"),
            file_name: format!("{id}.svg"),
            kind: MediaKind::Image,
            metadata: Metadata::new(),
            created: created(id),
        };
        let album = |id: &str, items: &[&str]| Album {
            id: id.into(),
            name: id.into(),
            metadata: Metadata::new(),
            items: items.iter().map(|id| item(id)).collect(),
        };
        let albums = vec![
            album("trip", &["a", "shared", "b", "shared", "c"]),
            album("twins", &["twin", "twin"]),
        ];
        let snapshot = Snapshot::new(refuse_shared_item_ids(albums), Skipped::default());
        let items = snapshot.albums[0].items.iter();
        let ids: Vec<_> = items.map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        let twins = snapshot.album("twins").unwrap();
        assert!(twins.items.is_empty());
        // Dated by the items they list, and undated when they list none.
        let trip = snapshot.album("trip").unwrap();
        assert_eq!(trip.newest_item(), Some(created("a")));
        assert_eq!(twins.newest_item(), None);
        fn id(item: Option<&Item>) -> Option<&str> {
            item.map(|item| item.id.as_str())
        }
        for (item, number, previous, next) in [
            ("a", 1, None, Some("b")),
            ("b", 2, Some("a"), Some("c")),
            ("c", 3, Some("b"), None),
        ] {
            let place = snapshot.item_place(item).unwrap();
            assert_eq!((place.number(), place.count()), (number, 3), "{item}");
            assert_eq!(id(place.previous), previous, "{item}");
            assert_eq!(id(place.next), next, "{item}");
        }
        assert!(snapshot.item_place("shared").is_none());
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
