//! The thumbnail service: each original's thumbnail is made once, in a child
//! (see `child`), and kept in a folder; what failed is remembered, so it isn't
//! tried again on every page view.
//!
//! The folder holds one folder per item, named by its link id, holding
//! `<version>.jpg` or `<version>.none-<build>`; making either removes the
//! item's older versions. After each scan the library hands over, only its
//! items keep their folders ([`Thumbnails::keep_only`]). Only names this
//! module gives are ever removed: item folders, the `<version>.jpg` and
//! `<version>.none-<build>` names in them and their temporary files, and the
//! same names as `<item id>-<version>.…`, kept before items had folders. The folder belongs to one library: another one sharing it, with
//! other items or another link key, would remove these thumbnails. It must
//! be apart from the library ([`Thumbnails::apart_from`]), and an item folder
//! that is a link is never written through, so the library never loses a
//! file to the cache.

use super::child::{Failure, Original, Thumbnailer};
use crate::crypto::TEMPORARY_INFIX;
use crate::link_ids::{ID_LEN, is_link_id};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, Metadata};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;

/// How many children may run at once: a page of new videos mustn't crowd
/// out the server, or a small board's memory.
pub const CHILDREN: usize = 2;
/// A kept thumbnail is `<item id>/<version>.jpg`.
const THUMBNAIL_EXTENSION: &str = "jpg";
/// An original this build found no thumbnail in is marked
/// `<item id>/<version>.none-<build>`.
const NONE_EXTENSION: &str = "none";
/// A temporary file this old was left behind by an interrupted write: one in
/// progress takes seconds.
const LEFT_BEHIND: Duration = Duration::from_secs(60 * 60);
/// A sweep taking longer than this is said in the log (a hung mount), and
/// still waited for.
const SLOW_SWEEP: Duration = Duration::from_secs(60);
/// The setting that lets children decode unsandboxed, as the log names it.
pub const OPT_IN: &str = "allow_unconfined in [thumbnails]";

/// Where each thumbnail's outcome is written down, one line each. As in the
/// access log, a line is one locked write, and a closed stream never panics.
pub enum AuditLog {
    /// Diagnostics go to stderr; stdout carries only the access log.
    Stderr,
    #[cfg(test)]
    Memory(crate::access_log::Lines),
}

impl AuditLog {
    fn write(&self, line: String) {
        match self {
            Self::Stderr => {
                let _ = writeln!(io::stderr().lock(), "{line}");
            }
            #[cfg(test)]
            Self::Memory(lines) => lines.lock().unwrap().push(line),
        }
    }
}

/// Thumbnails kept in a folder, each made by a [`Thumbnailer`] the first time
/// it is asked for.
pub struct Thumbnails {
    dir: PathBuf,
    thumbnailer: Thumbnailer,
    /// Which build this is (see `child::build`): a verdict on a file is
    /// sought again by another build, which may read or decode more.
    build: String,
    log: AuditLog,
    /// What timed out or was killed in this run, by item and version:
    /// maybe the host's trouble, so it is tried again after a restart.
    failed_this_run: Mutex<HashSet<(String, Version)>>,
    /// Set once the platform can't make thumbnails, for the run; those kept
    /// already are still given.
    off: AtomicBool,
    /// One permit per child that may run.
    children: Semaphore,
}

impl Thumbnails {
    /// This service, if its folder is apart from the library at `root`
    /// (canonical): else why not.
    pub fn apart_from(self, root: &Path) -> Result<Self, String> {
        apart(&self.dir, root).map(|()| self)
    }

    pub fn new(dir: PathBuf, thumbnailer: Thumbnailer, build: String, log: AuditLog) -> Self {
        Self {
            dir,
            thumbnailer,
            build,
            log,
            failed_this_run: Mutex::new(HashSet::new()),
            off: AtomicBool::new(false),
            children: Semaphore::new(CHILDREN),
        }
    }

    /// The thumbnail of item `id`, whose original is `original`, as a file in
    /// the folder: made now if it isn't there yet, or `None` if there is none.
    pub async fn thumbnail(&self, id: &str, original: Original) -> Option<PathBuf> {
        let started = Instant::now();
        // Each item has a folder of its own, holding its current version; an
        // id is its name, never a path out of the cache.
        let mut parts = Path::new(id).components();
        if !matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(_)), None)
        ) {
            return None;
        }
        let version = Version::of(original.metadata());
        let folder = self.dir.join(id);
        let made = folder.join(format!("{version}.{THUMBNAIL_EXTENSION}"));
        if exists(&made).await {
            return Some(made);
        }
        let none = folder.join(format!("{version}.{NONE_EXTENSION}-{}", self.build));
        let key = (id.to_owned(), version);
        if exists(&none).await || self.failed_this_run().contains(&key) {
            return None;
        }
        // Never closed, so acquiring only waits.
        let _child = self.children.acquire().await.ok()?;
        // While this one waited, another may have made it, or found that
        // the platform can't.
        if exists(&made).await {
            return Some(made);
        }
        if self.off.load(Ordering::Relaxed) {
            return None;
        }
        let made_by_child = self.thumbnailer.thumbnail(original).await;
        let elapsed = started.elapsed().as_millis();
        let outcome = |what: &dyn fmt::Display| {
            self.log
                .write(format!("thumbnail {id}: {what} in {elapsed} ms"));
        };
        match made_by_child {
            Ok(jpeg) => {
                let len = jpeg.len();
                // If the item leaves the library meanwhile, the sweep may
                // remove this write's temporary file ("made but not kept"),
                // or the write may make its folder again, which the next
                // sweep or start removes.
                match write(made.clone(), jpeg).await {
                    Ok(()) => {
                        outcome(&format_args!("made, {len} bytes"));
                        self.remove_older(id, &folder, version).await;
                        return Some(made);
                    }
                    Err(err) => outcome(&format_args!("made but not kept: {err}")),
                }
            }
            // A verdict on the file: until it or the build changes, on disk.
            Err(
                failure @ (Failure::Unsupported(_) | Failure::Crashed(_) | Failure::BadOutput(_)),
            ) => {
                outcome(&failure);
                match write(none.clone(), Vec::new()).await {
                    Ok(()) => self.remove_older(id, &folder, version).await,
                    Err(err) => outcome(&format_args!("not remembered: {err}")),
                }
            }
            // Maybe the host's trouble (a busy board, low memory): this run.
            Err(failure @ (Failure::TimedOut | Failure::Killed(_))) => {
                outcome(&failure);
                self.failed_this_run().insert(key);
            }
            // Maybe the storage's: not remembered.
            Err(failure @ Failure::Io(_)) => outcome(&failure),
            // The platform can't make thumbnails: off for the run, and no
            // file blamed.
            Err(failure @ Failure::Unconfined(_)) => {
                self.turn_off(&failure, &format!("; {OPT_IN} makes them unsandboxed"));
            }
            Err(failure @ Failure::NotStarted(_)) => self.turn_off(&failure, ""),
        }
        None
    }

    /// Removes older versions from item `id`'s `folder`, saying so if it can't.
    async fn remove_older(&self, id: &str, folder: &Path, current: Version) {
        let folder = folder.to_owned();
        if let Err(err) = blocking(move || remove_older_from(&folder, current)).await {
            self.log
                .write(format!("thumbnail {id}: older versions not removed: {err}"));
        }
    }

    /// Removes the thumbnails of items not in `items`, the item ids of a
    /// scan the library handed over, and those kept before items had folders
    /// of their own, saying how many if any, and that some can't be. Only
    /// gone items are touched, so a request in flight is affected only for
    /// one of them: it may find the thumbnail it was about to give gone (a
    /// 404), or write the folder again (see [`Self::thumbnail`]).
    pub async fn keep_only(&self, items: HashSet<String>) {
        let dir = self.dir.clone();
        let sweep = blocking(move || Ok(remove_gone(&dir, &items)));
        // A real sweep can't be made slow: its time limit is tested with a
        // stand-in.
        let swept = self.waited_for_sweep(sweep).await;
        // Not tested: only a panic in the sweep gets here.
        let Removed { count, error } = swept.unwrap_or_else(|err| Removed {
            error: Some(err),
            ..Removed::default()
        });
        if count > 0 {
            self.log
                .write(format!("thumbnails: removed {count} no longer needed"));
        }
        if let Some(err) = error {
            self.log.write(format!(
                "thumbnails: some no longer needed not removed: {err}"
            ));
        }
    }

    /// Waits for the sweep `work`, saying in the log if it takes over
    /// [`SLOW_SWEEP`].
    async fn waited_for_sweep<T>(&self, work: impl Future<Output = T>) -> T {
        let mut work = std::pin::pin!(work);
        match tokio::time::timeout(SLOW_SWEEP, &mut work).await {
            Ok(done) => done,
            Err(_) => {
                self.log.write(format!(
                    "thumbnails: removing those no longer needed is taking over {SLOW_SWEEP:?}, and scans wait: is the thumbnails folder's mount hung?"
                ));
                work.await
            }
        }
    }

    fn failed_this_run(&self) -> MutexGuard<'_, HashSet<(String, Version)>> {
        self.failed_this_run
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Turns thumbnails off for the run, saying why once.
    fn turn_off(&self, failure: &Failure, hint: &str) {
        if !self.off.swap(true, Ordering::Relaxed) {
            self.log.write(format!("thumbnails off: {failure}{hint}"));
        }
    }
}

/// Removes from an item's folder what it holds for versions changed before
/// `current`, whichever build wrote them, and temporary files [`LEFT_BEHIND`].
/// Everything else stays: the current version; a newer one, which a request
/// that opened the original before an edit mustn't remove; a write in
/// progress; and names not ours. Blocking; the first error is returned.
fn remove_older_from(folder: &Path, current: Version) -> io::Result<()> {
    let left_behind = SystemTime::now() - LEFT_BEHIND;
    let removed = remove_each(folder, |entry, name| {
        let older = match Version::named(name) {
            None => false,
            Some(_) if name.contains(TEMPORARY_INFIX) => {
                entry.metadata()?.modified()? < left_behind
            }
            Some(version) => version.changed < current.changed,
        };
        if older {
            fs::remove_file(entry.path())?;
        }
        Ok(older)
    });
    removed.error.map_or(Ok(()), Err)
}

/// Removes from the cache `dir` the folders of items not in `items`, and
/// the files kept before items had folders; anything else stays, as does a
/// gone item's folder holding something not ours. Blocking.
fn remove_gone(dir: &Path, items: &HashSet<String>) -> Removed {
    remove_each(dir, |entry, name| {
        if is_link_id(name) {
            // A link named as an item is not ours to follow.
            if items.contains(name) || !entry.file_type()?.is_dir() {
                return Ok(false);
            }
            let folder = entry.path();
            let emptied = remove_each(&folder, |entry, name| {
                let ours = Version::named(name).is_some();
                if ours {
                    fs::remove_file(entry.path())?;
                }
                Ok(ours)
            });
            emptied.error.map_or(Ok(()), Err)?;
            match fs::remove_dir(&folder) {
                Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(false),
                removed => removed.map(|()| true),
            }
        } else if kept_before_folders(name) {
            fs::remove_file(entry.path()).map(|()| true)
        } else {
            Ok(false)
        }
    })
}

/// What a removal did: how many it removed, and its first error.
#[derive(Default)]
struct Removed {
    count: usize,
    error: Option<io::Error>,
}

/// Calls `remove` with each entry of `folder` and its name, counting those
/// it removed. Names that aren't UTF-8 aren't ours, and a folder that isn't
/// there holds nothing. What can't be removed doesn't stop the rest; the
/// first error is kept.
fn remove_each(
    folder: &Path,
    mut remove: impl FnMut(&fs::DirEntry, &str) -> io::Result<bool>,
) -> Removed {
    let mut removed = Removed::default();
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return removed,
        Err(err) => {
            removed.error = Some(err);
            return removed;
        }
    };
    for entry in entries {
        let outcome = entry.and_then(|entry| {
            let name = entry.file_name();
            name.to_str().map_or(Ok(false), |name| remove(&entry, name))
        });
        match outcome {
            Ok(true) => removed.count += 1,
            Ok(false) => {}
            // Gone already: removed by a request or the sweep, or renamed into
            // place by a request. Not tested: it needs them to race.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                removed.error.get_or_insert(err);
            }
        }
    }
    removed
}

/// Whether `name` was kept before items had folders of their own:
/// `<item id>-` and a name [`Version::named`] takes.
fn kept_before_folders(name: &str) -> bool {
    name.split_at_checked(ID_LEN).is_some_and(|(id, version)| {
        is_link_id(id) && version.strip_prefix('-').and_then(Version::named).is_some()
    })
}

/// Whether `path` exists; an error reading the folder counts as no.
async fn exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

/// Writes `bytes` to `path` atomically, on a blocking thread, in an item
/// folder of its own.
async fn write(path: PathBuf, bytes: Vec<u8>) -> io::Result<()> {
    blocking(move || {
        own_folder(path.parent().expect("a file in its item's folder"))?;
        crate::crypto::write_file_atomically(&path, &bytes)
    })
    .await
}

/// Fails if `folder` is there but not a folder of its own: a link, which
/// could lead into the library, is never written through.
fn own_folder(folder: &Path) -> io::Result<()> {
    match fs::symlink_metadata(folder) {
        Ok(metadata) if !metadata.is_dir() => Err(io::Error::other(
            "the item's folder is a link or a file, not a folder of its own",
        )),
        // Not there yet; any other error, the write itself meets.
        _ => Ok(()),
    }
}

/// Whether the thumbnails folder `dir` is apart from the library at `root`
/// (canonical), where they lead: neither holds the other. Thumbnails are
/// only ever removed in `dir`, so the library then never loses a file.
fn apart(dir: &Path, root: &Path) -> Result<(), String> {
    let at = leads_to(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    if at.starts_with(root) || root.starts_with(&at) {
        return Err(format!("{} and the library overlap", dir.display()));
    }
    Ok(())
}

/// Where `path` leads: its longest part that exists, with links resolved,
/// then the rest as written. Creates nothing; a `..` past what exists is an
/// error.
fn leads_to(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let (mut existing, mut missing) = (path.as_path(), Vec::new());
    loop {
        match fs::canonicalize(existing) {
            Ok(found) => return Ok(missing.iter().rev().fold(found, |at, part| at.join(part))),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                missing.push(existing.file_name().ok_or(err)?);
                existing = existing.parent().expect("a name has a parent");
            }
            Err(err) => return Err(err),
        }
    }
}

/// Runs file work on a blocking thread.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(io::Error::other)?
}

/// Which version of an original a thumbnail is of: the file's size and time
/// of change, in nanoseconds since 1970, signed so earlier times stay apart.
/// Read from the opened original, so it is the version given; the same file
/// always gives the same version. Named `<size>-<time>`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Version {
    len: u64,
    changed: i128,
}

impl Version {
    fn of(metadata: &Metadata) -> Self {
        // Linux and macOS always know the time of change. The casts lose
        // nothing: a `SystemTime` is at most 2^64 seconds from 1970, under
        // 2^94 nanoseconds.
        let changed = metadata
            .modified()
            .map_or(0, |time| match time.duration_since(UNIX_EPOCH) {
                Ok(after) => after.as_nanos() as i128,
                Err(before) => -(before.duration().as_nanos() as i128),
            });
        Self {
            len: metadata.len(),
            changed,
        }
    }

    /// The version file `name` is kept for, if it is one this module names:
    /// `<version>.jpg` or `<version>.none-<build>`, or a temporary file of
    /// either.
    fn named(name: &str) -> Option<Self> {
        let (version, kind) = name.split_once('.')?;
        let kind = kind
            .split_once(TEMPORARY_INFIX)
            .map_or(kind, |(kind, _)| kind);
        let marker = kind.strip_prefix(NONE_EXTENSION);
        if kind != THUMBNAIL_EXTENSION && !marker.is_some_and(|build| build.starts_with('-')) {
            return None;
        }
        let (len, changed) = version.split_once('-')?;
        Some(Self {
            len: len.parse().ok()?,
            changed: changed.parse().ok()?,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.len, self.changed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{JPEG, cache, fake, open, service};
    use super::*;

    /// [`service`] with the fake child `name`, as one build.
    fn thumbnails(name: &str, dir: &Path) -> (Thumbnails, crate::access_log::Lines) {
        service(fake(name), Duration::from_secs(5), "build-1", dir)
    }

    /// An original in `dir` holding `bytes`, which the fake children read.
    fn original(dir: &Path, bytes: &[u8]) -> PathBuf {
        let path = dir.join("video.mp4");
        fs::write(&path, bytes).unwrap();
        path
    }

    /// Sets when `file` last changed.
    fn set_changed(file: &Path, time: SystemTime) {
        fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    /// When `file` last changed.
    fn changed(file: &Path) -> SystemTime {
        fs::metadata(file).unwrap().modified().unwrap()
    }

    /// A version is named by the file's size and its time of change in
    /// nanoseconds since 1970: a fixed name, so a change of format can only be
    /// deliberate. Times before 1970 keep their sign, so they stay apart.
    #[test]
    fn a_version_is_named_by_size_and_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("video.mp4");
        fs::write(&file, &JPEG[..19]).unwrap();
        let name_at = |time: SystemTime| {
            set_changed(&file, time);
            Version::of(&fs::metadata(&file).unwrap()).to_string()
        };
        let second = Duration::from_secs(1);
        assert_eq!(
            name_at(UNIX_EPOCH + 1_000_000_000 * second),
            "19-1000000000000000000"
        );
        let (epoch, before, long_before) = (
            name_at(UNIX_EPOCH),
            name_at(UNIX_EPOCH - second),
            name_at(UNIX_EPOCH - 2 * second),
        );
        assert_eq!(
            [&epoch[..], &before, &long_before],
            ["19-0", "19--1000000000", "19--2000000000"]
        );
    }

    /// The opened original (as the service reads it), its path and the
    /// folder's listing (as a scan reads it) give one version, also for a
    /// time before 1970.
    #[test]
    fn every_reading_of_a_file_gives_one_version() {
        let dir = tempfile::tempdir().unwrap();
        for (name, time) in [
            ("now.mp4", None),
            ("old.mp4", Some(UNIX_EPOCH - Duration::from_secs(1))),
        ] {
            let file = dir.path().join(name);
            fs::write(&file, JPEG).unwrap();
            if let Some(time) = time {
                set_changed(&file, time);
            }
            let listed = fs::read_dir(dir.path())
                .unwrap()
                .map(Result::unwrap)
                .find(|entry| entry.file_name() == name)
                .unwrap();
            let opened = Version::of(open(&file).metadata());
            assert_eq!(opened, Version::of(&fs::metadata(&file).unwrap()), "{name}");
            assert_eq!(opened, Version::of(&listed.metadata().unwrap()), "{name}");
        }
    }

    #[tokio::test]
    async fn makes_a_missing_thumbnail_and_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, log) = thumbnails("echo", dir.path());
        let made = thumbnails
            .thumbnail("item", open(&original(dir.path(), JPEG)))
            .await;
        let made = made.unwrap();
        // In the item's own folder: `<cache>/<item id>/<version>.jpg`.
        assert_eq!(
            made.parent(),
            Some(cache(dir.path()).join("item").as_path())
        );
        assert_eq!(made.extension().unwrap(), "jpg");
        assert_eq!(fs::read(&made).unwrap(), JPEG);
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("thumbnail item: made, "), "{lines:?}");
    }

    /// A thumbnail already kept is given as it is: no child starts, so no
    /// second outcome is written down.
    #[tokio::test]
    async fn a_kept_thumbnail_starts_no_child() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, log) = thumbnails("echo", dir.path());
        let video = original(dir.path(), JPEG);
        let first = thumbnails.thumbnail("item", open(&video)).await.unwrap();
        let again = thumbnails.thumbnail("item", open(&video)).await.unwrap();
        assert_eq!(first, again);
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "{lines:?}");
    }

    /// A restarted server finds the thumbnails it kept: the same file gives
    /// the same name, so no child starts.
    #[tokio::test]
    async fn a_restart_finds_the_thumbnails_kept() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let (before, _) = thumbnails("echo", dir.path());
        let kept = before.thumbnail("item", open(&video)).await.unwrap();
        let (restarted, log) = thumbnails("echo", dir.path());
        assert_eq!(restarted.thumbnail("item", open(&video)).await, Some(kept));
        assert!(log.lock().unwrap().is_empty(), "a child started");
    }

    /// An edited original is a new one: its thumbnail is made again.
    #[tokio::test]
    async fn an_edited_original_gets_a_new_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, log) = thumbnails("echo", dir.path());
        let video = original(dir.path(), JPEG);
        let first = thumbnails.thumbnail("item", open(&video)).await.unwrap();
        let edited = b"\xFF\xD8\xFF\xE0 an edited thumbnail \xFF\xD9";
        fs::write(&video, edited).unwrap();
        let again = thumbnails.thumbnail("item", open(&video)).await.unwrap();
        assert_ne!(first, again);
        assert_eq!(fs::read(&again).unwrap(), edited);
        assert_eq!(log.lock().unwrap().clone().len(), 2);
    }

    /// Each part of an original's version tells it apart: a later time of
    /// change with the same size, another size at the same time, and the same
    /// file as another item each get a thumbnail of their own.
    #[tokio::test]
    async fn each_version_of_an_original_gets_its_own_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, _) = thumbnails("echo", dir.path());
        let video = original(dir.path(), JPEG);
        let then = changed(&video);
        let first = thumbnails.thumbnail("item", open(&video)).await.unwrap();

        set_changed(&video, then + Duration::from_secs(1));
        let later = thumbnails.thumbnail("item", open(&video)).await.unwrap();

        fs::write(&video, b"\xFF\xD8\xFF\xE0 a longer thumbnail \xFF\xD9").unwrap();
        set_changed(&video, then);
        let longer = thumbnails.thumbnail("item", open(&video)).await.unwrap();

        let other_item = thumbnails.thumbnail("other", open(&video)).await.unwrap();
        let kept = [&first, &later, &longer, &other_item];
        let distinct: HashSet<_> = kept.iter().collect();
        assert_eq!(distinct.len(), kept.len(), "{kept:?}");
    }

    /// When the folder can't be written, nothing is kept, and the log says
    /// so: a thumbnail made but not kept, a verdict not remembered.
    #[tokio::test]
    async fn says_when_the_folder_cannot_be_written() {
        for (name, says) in [
            ("echo", ["thumbnail item: made but not kept: "].as_slice()),
            (
                "unsupported",
                &[
                    "thumbnail item: unsupported: ",
                    "thumbnail item: not remembered: ",
                ],
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(cache(dir.path()), b"a file where the folder should be").unwrap();
            let (thumbnails, log) = thumbnails(name, dir.path());
            let video = original(dir.path(), JPEG);
            assert_eq!(thumbnails.thumbnail("item", open(&video)).await, None);
            let lines = log.lock().unwrap().clone();
            assert_eq!(lines.len(), says.len(), "{name}: {lines:?}");
            for (line, start) in lines.iter().zip(says) {
                assert!(line.starts_with(start), "{name}: {lines:?}");
            }
        }
    }

    /// Writing an item's thumbnail or verdict removes what its folder holds
    /// for versions changed before it, whichever build wrote them, and writes
    /// left behind. A write still in progress (a recent `.tmp-` file) and
    /// names not ours stay.
    #[tokio::test]
    async fn a_new_version_replaces_the_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let (thumbnails, _) = thumbnails("echo", dir.path());
        let then = changed(&video);
        let first = thumbnails.thumbnail("item", open(&video)).await.unwrap();
        let folder = first.parent().unwrap().to_owned();
        let old_build = folder.join("1-2.none-build-0");
        let recent_write = folder.join("3-4.jpg.tmp-0123456789abcdef");
        let left_behind = folder.join("5-6.jpg.tmp-fedcba9876543210");
        let not_ours = [
            "notes.txt",
            "notes.jpg",
            "notes.jpg.tmp-fedcba9876543210",
            "1-2.png",
            "1-2.nonesuch",
        ]
        .map(|name| folder.join(name));
        for file in [&old_build, &recent_write, &left_behind]
            .into_iter()
            .chain(&not_ours)
        {
            fs::write(file, b"").unwrap();
        }
        let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
        set_changed(&left_behind, two_hours_ago);
        for file in &not_ours {
            set_changed(file, two_hours_ago);
        }

        fs::write(&video, b"\xFF\xD8\xFF\xE0 an edited thumbnail \xFF\xD9").unwrap();
        set_changed(&video, then + Duration::from_secs(1));
        let edited = thumbnails.thumbnail("item", open(&video)).await.unwrap();
        let mut held: Vec<_> = fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        held.sort();
        let mut expected = [vec![edited, recent_write], not_ours.to_vec()].concat();
        expected.sort();
        assert_eq!(held, expected);
    }

    /// A verdict on a new version replaces the older version's thumbnail
    /// too: only the current version is kept.
    #[tokio::test]
    async fn a_verdict_replaces_an_older_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let (before, _) = thumbnails("echo", dir.path());
        let then = changed(&video);
        let thumbnail = before.thumbnail("item", open(&video)).await.unwrap();
        fs::write(&video, b"no longer a video").unwrap();
        set_changed(&video, then + Duration::from_secs(1));
        let (now, _) = thumbnails("unsupported", dir.path());
        assert_eq!(now.thumbnail("item", open(&video)).await, None);
        let held: Vec<_> = fs::read_dir(thumbnail.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(
            held[0].to_string_lossy().contains(".none-build-1"),
            "{held:?}"
        );
    }

    /// A request that opened the original before it was edited, and ends
    /// last, keeps the newer version's thumbnail: another request may be
    /// serving it.
    #[tokio::test]
    async fn a_late_request_keeps_the_newer_version() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let (thumbnails, _) = thumbnails("echo", dir.path());
        let then = changed(&video);
        let before_the_edit = open(&video);
        set_changed(&video, then + Duration::from_secs(1));
        let newer = thumbnails.thumbnail("item", open(&video)).await.unwrap();
        let older = thumbnails.thumbnail("item", before_the_edit).await.unwrap();
        assert_ne!(older, newer);
        assert!(newer.exists());
    }

    /// What another build wrote for the current version stays: builds that
    /// share a folder keep their own verdicts.
    #[tokio::test]
    async fn another_builds_verdict_on_the_current_version_stays() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let folder = cache(dir.path()).join("item");
        fs::create_dir_all(&folder).unwrap();
        let verdict = folder.join(format!(
            "{}.none-build-0",
            Version::of(&fs::metadata(&video).unwrap())
        ));
        fs::write(&verdict, b"").unwrap();
        let (thumbnails, _) = thumbnails("echo", dir.path());
        assert!(thumbnails.thumbnail("item", open(&video)).await.is_some());
        assert!(verdict.exists());
    }

    /// What can't be removed is said in the log; the rest is still removed,
    /// and the new thumbnail given. A folder named as an old thumbnail can't
    /// be removed as a file.
    #[tokio::test]
    async fn says_when_older_versions_cannot_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let (thumbnails, log) = thumbnails("echo", dir.path());
        let then = changed(&video);
        let first = thumbnails.thumbnail("item", open(&video)).await.unwrap();
        let folder = first.parent().unwrap().to_owned();
        let stuck = ["1-2.jpg", "7-8.jpg"].map(|name| folder.join(name));
        for folder in &stuck {
            fs::create_dir(folder).unwrap();
        }
        for name in ["3-4.jpg", "5-6.none-build-1", "9-10.jpg"] {
            fs::write(folder.join(name), b"").unwrap();
        }
        fs::write(&video, b"\xFF\xD8\xFF\xE0 an edited thumbnail \xFF\xD9").unwrap();
        set_changed(&video, then + Duration::from_secs(1));
        let made = thumbnails.thumbnail("item", open(&video)).await.unwrap();
        let mut held: Vec<_> = fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        held.sort();
        let mut expected = [stuck.to_vec(), vec![made]].concat();
        expected.sort();
        assert_eq!(held, expected);
        let lines = log.lock().unwrap().clone();
        let says = lines
            .iter()
            .any(|line| line.starts_with("thumbnail item: older versions not removed: "));
        assert!(says, "{lines:?}");
    }

    /// An id names one folder in the cache, never a path out of it: ids
    /// that aren't a plain name get no thumbnail, and nothing outside the
    /// cache is written or removed.
    #[tokio::test]
    async fn an_id_never_leads_out_of_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let planted = dir.path().join("1-2.jpg");
        fs::write(&planted, b"not the cache's").unwrap();
        let elsewhere = dir.path().join("elsewhere");
        let (thumbnails, log) = thumbnails("echo", dir.path());
        for id in ["..", ".", "", "item/..", "a/b", elsewhere.to_str().unwrap()] {
            assert_eq!(thumbnails.thumbnail(id, open(&video)).await, None, "{id:?}");
        }
        let mut held: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        held.sort();
        assert_eq!(held, ["1-2.jpg", "video.mp4"]);
        assert!(log.lock().unwrap().is_empty(), "a child started");
    }

    /// Only the item's own folder is tidied: another item's older versions,
    /// and a file named as one at the top of the cache, stay.
    #[tokio::test]
    async fn other_folders_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let (thumbnails, _) = thumbnails("echo", dir.path());
        let then = changed(&video);
        let other = thumbnails.thumbnail("other", open(&video)).await.unwrap();
        thumbnails.thumbnail("item", open(&video)).await.unwrap();
        let untouched = [
            other.with_file_name("1-2.jpg"),
            cache(dir.path()).join("1-2.jpg"),
        ];
        for file in &untouched {
            fs::write(file, b"").unwrap();
        }
        set_changed(&video, then + Duration::from_secs(1));
        thumbnails.thumbnail("item", open(&video)).await.unwrap();
        for file in &untouched {
            assert!(file.exists(), "{file:?}");
        }
    }

    /// A link named as an older version is removed as a link: what it
    /// points to, outside the cache, stays.
    #[tokio::test]
    async fn a_link_is_removed_not_what_it_points_to() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let (thumbnails, _) = thumbnails("echo", dir.path());
        let then = changed(&video);
        let first = thumbnails.thumbnail("item", open(&video)).await.unwrap();
        let target = dir.path().join("photo.jpg");
        fs::write(&target, b"a photo").unwrap();
        let link = first.with_file_name("1-2.jpg");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        set_changed(&video, then + Duration::from_secs(1));
        thumbnails.thumbnail("item", open(&video)).await.unwrap();
        assert!(fs::symlink_metadata(&link).is_err(), "the link stays");
        assert_eq!(fs::read(&target).unwrap(), b"a photo");
    }

    /// Item ids, shaped as the library's are.
    const KEPT: &str = "kept-kept-kept-kept-kp";
    const GONE: &str = "gone_gone_gone_gone_gn";

    /// The names `dir` holds, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// After a scan, the thumbnails of items no longer in the library are
    /// removed, folder and all, and the log says how many; those of items
    /// still there stay. A second sweep finds nothing and says nothing.
    #[tokio::test]
    async fn removes_the_thumbnails_of_items_gone() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let (thumbnails, log) = thumbnails("echo", dir.path());
        let kept = thumbnails.thumbnail(KEPT, open(&video)).await.unwrap();
        thumbnails.thumbnail(GONE, open(&video)).await.unwrap();
        log.lock().unwrap().clear();
        let items = HashSet::from([KEPT.to_owned()]);
        thumbnails.keep_only(items.clone()).await;
        assert_eq!(names(&cache(dir.path())), [KEPT]);
        assert!(kept.exists());
        thumbnails.keep_only(items).await;
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines, ["thumbnails: removed 1 no longer needed"]);
    }

    /// The sweep removes only what is ours: the files kept before items had
    /// folders (`<item id>-<version>.jpg` and the like), whichever item, and
    /// the folders of items gone. Other names and kinds stay, as do a link
    /// named as an item and what it points to, and what isn't ours in a gone
    /// item's folder.
    #[tokio::test]
    async fn the_sweep_removes_only_what_is_ours() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        let gone = cache.join(GONE);
        fs::create_dir_all(&gone).unwrap();
        for name in ["1-2.jpg", "1-2.png", "notes.txt"] {
            fs::write(gone.join(name), b"").unwrap();
        }
        for name in [
            format!("{KEPT}-19-123.jpg"),
            format!("{GONE}-19-123.none-build-0"),
            format!("{KEPT}-19-123.jpg.tmp-0123456789abcdef"),
            "notes.txt".into(),
            format!("{GONE}.txt"),
            "kept-kept-kept-kept-k-19-123.jpg".into(),
            "kept kept kept kept kp-19-123.jpg".into(),
            format!("{KEPT}-19-123.png"),
        ] {
            fs::write(cache.join(name), b"").unwrap();
        }
        fs::create_dir(cache.join("backups")).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(elsewhere.join("1-2.jpg"), b"").unwrap();
        let link = "link_link_link_link_lk";
        std::os::unix::fs::symlink(&elsewhere, cache.join(link)).unwrap();
        let (thumbnails, log) = thumbnails("echo", dir.path());
        thumbnails.keep_only(HashSet::from([KEPT.to_owned()])).await;
        assert_eq!(
            names(&cache),
            [
                "backups",
                GONE,
                &format!("{GONE}.txt"),
                "kept kept kept kept kp-19-123.jpg",
                "kept-kept-kept-kept-k-19-123.jpg",
                &format!("{KEPT}-19-123.png"),
                link,
                "notes.txt",
            ]
        );
        assert_eq!(names(&gone), ["1-2.png", "notes.txt"]);
        assert_eq!(names(&elsewhere), ["1-2.jpg"]);
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines, ["thumbnails: removed 3 no longer needed"]);
    }

    /// A cache from before items had folders: its thumbnail of the very
    /// same version isn't given, a new one is made in the item's folder, and
    /// the first sweep removes the old file.
    #[tokio::test]
    async fn an_old_cache_layout_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let version = Version::of(&fs::metadata(&video).unwrap());
        let cache = cache(dir.path());
        fs::create_dir_all(&cache).unwrap();
        let old = cache.join(format!("{KEPT}-{version}.jpg"));
        fs::write(&old, b"the old layout's").unwrap();
        let (thumbnails, log) = thumbnails("echo", dir.path());
        let made = thumbnails.thumbnail(KEPT, open(&video)).await.unwrap();
        assert_eq!(made.parent(), Some(cache.join(KEPT).as_path()));
        assert_eq!(fs::read(&made).unwrap(), JPEG);
        thumbnails.keep_only(HashSet::from([KEPT.to_owned()])).await;
        assert_eq!(names(&cache), [KEPT]);
        let lines = log.lock().unwrap().clone();
        assert!(
            lines[0].starts_with(&format!("thumbnail {KEPT}: made, ")),
            "{lines:?}"
        );
        assert_eq!(lines[1..], ["thumbnails: removed 1 no longer needed"]);
    }

    /// With no thumbnail kept yet there is no folder, and nothing to say.
    #[tokio::test]
    async fn a_sweep_before_any_thumbnail_says_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, log) = thumbnails("echo", dir.path());
        thumbnails.keep_only(HashSet::new()).await;
        assert!(log.lock().unwrap().is_empty());
        assert!(!cache(dir.path()).exists());
    }

    /// What the sweep can't remove is said in the log, and doesn't stop the
    /// rest. A folder named as a thumbnail can't be removed as a file.
    #[tokio::test]
    async fn says_when_the_sweep_cannot_remove_something() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        let stuck = cache.join("stuck_stuck_stuck_stck");
        fs::create_dir_all(stuck.join("1-2.jpg")).unwrap();
        for id in [GONE, "aaaaaaaaaaaaaaaaaaaaaa", "zzzzzzzzzzzzzzzzzzzzzz"] {
            fs::create_dir_all(cache.join(id)).unwrap();
            fs::write(cache.join(id).join("1-2.jpg"), b"").unwrap();
        }
        let (thumbnails, log) = thumbnails("echo", dir.path());
        thumbnails.keep_only(HashSet::new()).await;
        assert_eq!(names(&cache), ["stuck_stuck_stuck_stck"]);
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], "thumbnails: removed 3 no longer needed");
        assert!(
            lines[1].starts_with("thumbnails: some no longer needed not removed: "),
            "{lines:?}"
        );
    }

    /// A gone item's folder the sweep can't read stays, and the log says so.
    #[tokio::test]
    async fn says_when_the_sweep_cannot_read_a_folder() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let gone = cache(dir.path()).join(GONE);
        fs::create_dir_all(&gone).unwrap();
        fs::write(gone.join("1-2.jpg"), b"").unwrap();
        fs::set_permissions(&gone, fs::Permissions::from_mode(0o000)).unwrap();
        let (thumbnails, log) = thumbnails("echo", dir.path());
        thumbnails.keep_only(HashSet::new()).await;
        fs::set_permissions(&gone, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(names(&gone), ["1-2.jpg"]);
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("thumbnails: some no longer needed not removed: "),
            "{lines:?}"
        );
    }

    /// A sweep that takes long, as on a hung mount, is said in the log, and
    /// still waited for; one that doesn't says nothing.
    #[tokio::test(start_paused = true)]
    async fn says_when_a_sweep_takes_long() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, log) = thumbnails("echo", dir.path());
        let taking = |time| async move {
            tokio::time::sleep(time).await;
            time
        };
        let quick = SLOW_SWEEP - Duration::from_secs(1);
        assert_eq!(thumbnails.waited_for_sweep(taking(quick)).await, quick);
        assert!(log.lock().unwrap().is_empty());
        let slow = SLOW_SWEEP * 2;
        assert_eq!(thumbnails.waited_for_sweep(taking(slow)).await, slow);
        let lines = log.lock().unwrap().clone();
        assert_eq!(
            lines,
            [
                "thumbnails: removing those no longer needed is taking over 60s, and scans wait: is the thumbnails folder's mount hung?"
            ]
        );
    }

    /// Names that aren't UTF-8 aren't ours, even when they start as a
    /// version does: they stay, and so does the folder holding them.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn names_not_in_utf8_are_not_ours() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let gone = cache(dir.path()).join(GONE);
        fs::create_dir_all(&gone).unwrap();
        let odd = gone.join(std::ffi::OsStr::from_bytes(b"1-2.\xff"));
        for file in [&gone.join("1-2.jpg"), &odd] {
            fs::write(file, b"").unwrap();
        }
        let (thumbnails, _) = thumbnails("echo", dir.path());
        thumbnails.keep_only(HashSet::new()).await;
        let held: Vec<_> = fs::read_dir(&gone)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(held, [odd]);
    }

    /// The thumbnails folder must be apart from the library, neither holding
    /// the other, also through a link or before it exists, so the library
    /// never loses a file to the cache. Nothing is created to find out.
    #[test]
    fn the_folder_must_be_apart_from_the_library() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let photos = base.join("photos");
        fs::create_dir(&photos).unwrap();
        std::os::unix::fs::symlink(&photos, base.join("link")).unwrap();
        let apart = |dir: PathBuf| apart(&dir, &photos).is_ok();
        assert!(apart(base.join("thumbnails")));
        assert!(apart(base.join("state/thumbnails")));
        assert!(apart(base.join("photos-thumbnails")));
        for overlapping in [
            photos.clone(),
            photos.join("thumbnails"),
            photos.join("album/thumbnails"),
            base.clone(),
            base.join("link/thumbnails"),
            base.join("missing/../photos/thumbnails"),
        ] {
            assert!(!apart(overlapping.clone()), "{overlapping:?}");
        }
        let mut held: Vec<_> = fs::read_dir(&base)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        held.sort();
        assert_eq!(held, ["link", "photos"]);
        assert_eq!(fs::read_dir(&photos).unwrap().count(), 0);
    }

    /// A service whose folder is inside the library is refused, saying why;
    /// one apart from it is kept.
    #[test]
    fn a_service_inside_the_library_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let (inside, _) = thumbnails("echo", &base);
        let refused = inside.apart_from(&base).err().unwrap();
        assert!(refused.ends_with(" and the library overlap"), "{refused}");
        let photos = base.join("photos");
        fs::create_dir(&photos).unwrap();
        let (beside, _) = thumbnails("echo", &base);
        assert!(beside.apart_from(&photos).is_ok());
    }

    /// An item folder that is a link isn't used, so a link into the library
    /// can't have a file there replaced or removed: the thumbnail is made
    /// but not kept, and what the link leads to stays as it was.
    #[tokio::test]
    async fn an_item_folder_that_is_a_link_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let album = dir.path().join("album");
        fs::create_dir(&album).unwrap();
        fs::write(album.join("1-2.jpg"), b"a photo").unwrap();
        fs::create_dir_all(cache(dir.path())).unwrap();
        std::os::unix::fs::symlink(&album, cache(dir.path()).join("item")).unwrap();
        let (thumbnails, log) = thumbnails("echo", dir.path());
        assert_eq!(thumbnails.thumbnail("item", open(&video)).await, None);
        assert_eq!(names(&album), ["1-2.jpg"]);
        assert_eq!(fs::read(album.join("1-2.jpg")).unwrap(), b"a photo");
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("thumbnail item: made but not kept: "),
            "{lines:?}"
        );
    }

    /// A file with nothing to show is written down once, and not tried again
    /// until it changes.
    #[tokio::test]
    async fn an_unsupported_original_is_not_tried_again() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, log) = thumbnails("unsupported", dir.path());
        let video = original(dir.path(), b"");
        assert_eq!(thumbnails.thumbnail("item", open(&video)).await, None);
        assert_eq!(thumbnails.thumbnail("item", open(&video)).await, None);
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("thumbnail item: unsupported: damaged file in "),
            "{lines:?}"
        );
    }

    /// A verdict on a file (nothing to show, a crash, nonsense) is kept until
    /// the build changes: a new build may read or decode more, so it tries
    /// again.
    #[tokio::test]
    async fn a_verdict_is_tried_again_by_another_build_only() {
        for (name, says) in [
            ("unsupported", "thumbnail item: unsupported: "),
            ("aborts", "thumbnail item: crashed: "),
            ("chatters", "thumbnail item: bad output: "),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let video = original(dir.path(), b"");
            let (thumbnails, log) = thumbnails(name, dir.path());
            assert_eq!(thumbnails.thumbnail("item", open(&video)).await, None);
            assert_eq!(thumbnails.thumbnail("item", open(&video)).await, None);
            assert_eq!(log.lock().unwrap().clone().len(), 1, "{name}");
            let (rebuilt, log) = service(fake(name), Duration::from_secs(5), "build-2", dir.path());
            assert_eq!(rebuilt.thumbnail("item", open(&video)).await, None);
            let lines = log.lock().unwrap().clone();
            assert_eq!(lines.len(), 1, "{name}: {lines:?}");
            assert!(lines[0].starts_with(says), "{lines:?}");
        }
    }

    /// A timeout or a kill may be the host's trouble (a busy board, low
    /// memory), not the file's: it isn't tried again in this run, but is
    /// after a restart.
    #[tokio::test]
    async fn a_timeout_or_a_kill_is_tried_again_after_a_restart() {
        for (child, limit, says) in [
            (
                fake("killed"),
                Duration::from_secs(5),
                "thumbnail item: killed: ",
            ),
            (
                fake("sleeps"),
                Duration::from_millis(500),
                "thumbnail item: timed out ",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let video = original(dir.path(), b"");
            let start = || service(child.clone(), limit, "build-1", dir.path());
            let (thumbnails, log) = start();
            assert_eq!(thumbnails.thumbnail("item", open(&video)).await, None);
            assert_eq!(thumbnails.thumbnail("item", open(&video)).await, None);
            let lines = log.lock().unwrap().clone();
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].starts_with(says), "{lines:?}");
            let (restarted, log) = start();
            assert_eq!(restarted.thumbnail("item", open(&video)).await, None);
            assert_eq!(log.lock().unwrap().clone().len(), 1);
        }
    }

    /// A read error may be the storage's: it is tried again next time.
    #[tokio::test]
    async fn a_read_error_is_tried_again() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, log) = thumbnails("unreadable", dir.path());
        let video = original(dir.path(), b"");
        assert_eq!(thumbnails.thumbnail("item", open(&video)).await, None);
        assert_eq!(thumbnails.thumbnail("item", open(&video)).await, None);
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[1].starts_with("thumbnail item: i/o error: "),
            "{lines:?}"
        );
    }

    /// When the platform can't make thumbnails (no child starts, or none can
    /// be confined), they are off for the run: written down once, nothing
    /// tried again, and no file blamed.
    #[tokio::test]
    async fn the_platform_failing_turns_thumbnails_off() {
        for (child, why) in [
            (fake("unconfined"), "thumbnails off: unconfined: "),
            (
                PathBuf::from("/nonexistent/rust-albums"),
                "thumbnails off: no child started: ",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (thumbnails, log) = service(child, Duration::from_secs(5), "build-1", dir.path());
            let video = original(dir.path(), b"");
            assert_eq!(thumbnails.thumbnail("a", open(&video)).await, None);
            assert_eq!(thumbnails.thumbnail("b", open(&video)).await, None);
            let lines = log.lock().unwrap().clone();
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].starts_with(why), "{lines:?}");
            // Without a sandbox, it says what the owner can do.
            let hint = OPT_IN;
            assert_eq!(
                lines[0].contains(hint),
                why.contains("unconfined"),
                "{lines:?}"
            );
            let kept: Vec<_> = fs::read_dir(cache(dir.path()))
                .into_iter()
                .flatten()
                .collect();
            assert!(kept.is_empty(), "{kept:?}");
        }
    }

    /// Requests waiting for a child when the platform is found unable start
    /// none: only those already running tried.
    #[tokio::test]
    async fn requests_waiting_start_no_child_once_thumbnails_are_off() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, log) = thumbnails("unconfined-counted", dir.path());
        let video = original(dir.path(), JPEG);
        let made = tokio::join!(
            thumbnails.thumbnail("a", open(&video)),
            thumbnails.thumbnail("b", open(&video)),
            thumbnails.thumbnail("c", open(&video)),
            thumbnails.thumbnail("d", open(&video)),
        );
        assert_eq!(made, (None, None, None, None));
        let runs = fs::read_to_string(fake("unconfined-counted.runs")).unwrap();
        assert_eq!(runs.lines().count(), CHILDREN, "{runs}");
        assert_eq!(log.lock().unwrap().clone().len(), 1);
    }

    /// Thumbnails kept already are still given once the platform turns new
    /// ones off.
    #[tokio::test]
    async fn kept_thumbnails_are_given_when_thumbnails_are_off() {
        let dir = tempfile::tempdir().unwrap();
        let video = original(dir.path(), JPEG);
        let (before, _) = thumbnails("echo", dir.path());
        let kept = before.thumbnail("kept", open(&video)).await.unwrap();
        let (now, _) = thumbnails("unconfined", dir.path());
        assert_eq!(now.thumbnail("new", open(&video)).await, None);
        assert_eq!(now.thumbnail("kept", open(&video)).await, Some(kept));
    }

    /// A request that waited for a child while another made its thumbnail
    /// takes that one: three asking for one item at once start only as many
    /// children as may run.
    #[tokio::test]
    async fn a_request_that_waited_takes_the_thumbnail_made_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, log) = thumbnails("unhurried", dir.path());
        let video = original(dir.path(), JPEG);
        let made = tokio::join!(
            thumbnails.thumbnail("item", open(&video)),
            thumbnails.thumbnail("item", open(&video)),
            thumbnails.thumbnail("item", open(&video)),
        );
        assert!(
            made.0.is_some() && made.1 == made.0 && made.2 == made.0,
            "{made:?}"
        );
        assert_eq!(log.lock().unwrap().clone().len(), CHILDREN);
    }

    /// No more than [`CHILDREN`] run at once, however many thumbnails are
    /// asked for: a page of new videos can't crowd out the server.
    #[tokio::test]
    async fn runs_no_more_children_at_once_than_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, _) = thumbnails("slow", dir.path());
        let videos: Vec<_> = (0..4)
            .map(|i| {
                let path = dir.path().join(format!("video{i}.mp4"));
                fs::write(&path, JPEG).unwrap();
                path
            })
            .collect();
        let made = tokio::join!(
            thumbnails.thumbnail("item0", open(&videos[0])),
            thumbnails.thumbnail("item1", open(&videos[1])),
            thumbnails.thumbnail("item2", open(&videos[2])),
            thumbnails.thumbnail("item3", open(&videos[3])),
        );
        assert!(made.0.is_some() && made.1.is_some() && made.2.is_some() && made.3.is_some());
        let most = fs::read_to_string(fake("slow.most")).unwrap();
        let most = most.lines().map(|n| n.trim().parse::<u32>().unwrap()).max();
        assert_eq!(most, Some(CHILDREN as u32));
    }
}
