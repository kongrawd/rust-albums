//! The thumbnail service: each original's thumbnail is made once, in a child
//! (see `child`), and kept in a folder; what failed is remembered, so it isn't
//! tried again on every page view.

use super::child::{Failure, Original, Thumbnailer};
use std::collections::HashSet;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Instant, UNIX_EPOCH};
use tokio::sync::Semaphore;

/// How many children may run at once: a page of new videos mustn't crowd
/// out the server, or a small board's memory.
pub const CHILDREN: usize = 2;
/// A kept thumbnail is `<key>.jpg`.
const THUMBNAIL_EXTENSION: &str = "jpg";
/// An original this build found no thumbnail in is marked `<key>.none-<build>`.
const NONE_EXTENSION: &str = "none";
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
    /// What timed out or was killed in this run, by key: maybe the host's
    /// trouble, so it is tried again after a restart.
    failed_this_run: Mutex<HashSet<String>>,
    /// Set once the platform can't make thumbnails, for the run; those kept
    /// already are still given.
    off: AtomicBool,
    /// One permit per child that may run.
    children: Semaphore,
}

impl Thumbnails {
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
        let key = key(id, &original);
        let made = self.file(&key, THUMBNAIL_EXTENSION);
        if exists(&made).await {
            return Some(made);
        }
        let none = self.file(&key, &format!("{NONE_EXTENSION}-{}", self.build));
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
                match write(made.clone(), jpeg).await {
                    Ok(()) => {
                        outcome(&format_args!("made, {len} bytes"));
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
                if let Err(err) = write(none, Vec::new()).await {
                    outcome(&format_args!("not remembered: {err}"));
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

    /// The file `<key>.<extension>` in the folder.
    fn file(&self, key: &str, extension: &str) -> PathBuf {
        self.dir.join(format!("{key}.{extension}"))
    }

    fn failed_this_run(&self) -> MutexGuard<'_, HashSet<String>> {
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

/// Whether `path` exists; an error reading the folder counts as no.
async fn exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

/// Writes `bytes` to `path` atomically, on a blocking thread.
async fn write(path: PathBuf, bytes: Vec<u8>) -> io::Result<()> {
    let write =
        tokio::task::spawn_blocking(move || crate::crypto::write_file_atomically(&path, &bytes));
    write.await.map_err(io::Error::other)?
}

/// The name an original's thumbnail is kept under: its item's link id, and
/// the file's size and time of change, so an edited original gets a new one.
/// A time before 1970 counts as 0 (Linux and macOS always know the time).
fn key(id: &str, original: &Original) -> String {
    let metadata = original.metadata();
    let changed = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
    let changed = changed.map_or(0, |since| since.as_nanos());
    format!("{id}-{}-{changed}", metadata.len())
}

#[cfg(test)]
mod tests {
    use super::super::testing::{JPEG, cache, fake, open, service};
    use super::*;
    use std::fs;
    use std::time::Duration;

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

    #[tokio::test]
    async fn makes_a_missing_thumbnail_and_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let (thumbnails, log) = thumbnails("echo", dir.path());
        let made = thumbnails
            .thumbnail("item", open(&original(dir.path(), JPEG)))
            .await;
        let made = made.unwrap();
        assert!(made.starts_with(cache(dir.path())), "{made:?}");
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
        let set_changed = |time| {
            fs::File::options()
                .write(true)
                .open(&video)
                .unwrap()
                .set_modified(time)
                .unwrap()
        };
        let then = fs::metadata(&video).unwrap().modified().unwrap();
        let first = thumbnails.thumbnail("item", open(&video)).await.unwrap();

        set_changed(then + std::time::Duration::from_secs(1));
        let later = thumbnails.thumbnail("item", open(&video)).await.unwrap();

        fs::write(&video, b"\xFF\xD8\xFF\xE0 a longer thumbnail \xFF\xD9").unwrap();
        set_changed(then);
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
