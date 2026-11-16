//! Decoding in a child process: `rust-albums --thumbnail` reads a video from
//! stdin, which must be a regular file, and writes its thumbnail to stdout.
//! Exit codes follow sysexits; why a file has no thumbnail goes to stderr.
//! [`Thumbnailer`] starts these children: a thumbnailer, as GNOME calls a
//! program that makes one thumbnail.
//!
//! Linux and macOS only: tested on Linux (CI) and macOS (by hand), each
//! confined its own way (see `sandbox`); built nowhere else.

use super::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// The file holds nothing a thumbnail can be made of (`EX_DATAERR`).
const UNSUPPORTED: i32 = 65;
/// The child couldn't confine itself, so it decoded nothing (`EX_OSERR`).
const UNCONFINED: i32 = 71;
/// An input/output error (`EX_IOERR`): reading the video failed (or stdin
/// isn't a regular file), or writing the thumbnail did.
const IO_ERROR: i32 = 74;

/// The signal the out-of-memory killer sends, 9 on Linux and macOS alike.
const SIGKILL: i32 = 9;
/// The most a child may write as a thumbnail. The largest thumbnail is of a
/// picture of [`MAX_PIXELS`](super::MAX_PIXELS) too narrow to shrink (400 px
/// or less on its short side), in pure noise: 6.3 MB, measured once.
const MAX_THUMBNAIL: usize = 8 << 20;
/// How long a child may take, from its start, before it is killed.
const TIME_LIMIT: Duration = Duration::from_secs(15);
/// The most of a child's message that is kept, for the log.
const MAX_MESSAGE: usize = 4 << 10;
/// How long a killed child is waited for. One stuck in the kernel (a dead
/// network mount) can outlive SIGKILL; tokio reaps it once it ends.
const REAP_LIMIT: Duration = Duration::from_secs(1);
/// The argument that makes this binary the child, read first in `main`.
pub const MODE: &str = "--thumbnail";
/// The argument that lets the child decode where it can't be confined.
pub const ALLOW_UNCONFINED: &str = "--allow-unconfined";
/// How many bytes of a file's SHA-256 tell builds apart (see [`build`]).
const BUILD_ID_BYTES: usize = 8;

/// The child's `main`: makes the thumbnail and returns the exit code. It runs
/// before anything else in the binary: no config, runtime or environment. It
/// stays single-threaded, so it runs as a container's only process under
/// `--pids-limit 1` (checked by hand in Docker; no test can count threads).
/// With `allow_unconfined`, the owner's opt-in, it decodes even where it
/// can't confine itself.
pub fn run(allow_unconfined: bool) -> i32 {
    // On Linux, be the process the out-of-memory killer picks rather than
    // the server. Any process may raise its own score. Checked by hand
    // (strace in Docker); no test can see it.
    #[cfg(target_os = "linux")]
    {
        let _ = std::fs::write("/proc/self/oom_score_adj", "1000");
        // From here on it reaches only its stdin, stdout and stderr; on
        // macOS the server started it confined. That it confines itself is
        // checked by hand (strace in Docker); tests check what it denies.
        // With the opt-in it carries on unconfined (checked by hand in
        // Docker with Landlock blocked; no test can take Landlock away).
        if let Err(err) = super::sandbox::confine()
            && !allow_unconfined
        {
            eprintln!("sandbox: {err}");
            return UNCONFINED;
        }
    }
    // On macOS the server confines it, so there is nothing to opt out of.
    #[cfg(target_os = "macos")]
    let _ = allow_unconfined;
    match thumbnail() {
        Ok(jpeg) => {
            let mut stdout = io::stdout().lock();
            match stdout.write_all(&jpeg).and_then(|()| stdout.flush()) {
                Ok(()) => 0,
                // The server stopped reading: it refused the output.
                Err(err) => {
                    eprintln!("writing the thumbnail: {err}");
                    IO_ERROR
                }
            }
        }
        Err(err) => {
            eprintln!("{err}");
            match err {
                Error::Unsupported(_) => UNSUPPORTED,
                Error::Io(_) => IO_ERROR,
            }
        }
    }
}

fn thumbnail() -> Result<Vec<u8>, Error> {
    let (mut file, metadata) = regular_file(stdin_file()?)?;
    super::video::thumbnail(&mut file, metadata.len())
}

/// An original handed to a child: a regular file, opened read-only.
///
/// The child can't write to it through its stdin. That is not a sandbox: on
/// Linux a process can reopen `/proc/self/fd/0` for writing, as the file's
/// permissions allow, and the decoders contain some `unsafe` code (SIMD,
/// threading), so a crafted video could take over the child, which runs as
/// the server's user. The child is the containment: it keeps the server
/// running. What keeps originals safe is that the photos are mounted
/// read-only in Docker.
pub struct Original {
    file: File,
    metadata: Metadata,
}

impl Original {
    /// Opens `path` read-only, if it is a regular file. Blocking, and it waits
    /// forever on a named pipe, so callers limit its time.
    pub fn open(path: &Path) -> io::Result<Self> {
        let (file, metadata) = regular_file(File::open(path)?)?;
        Ok(Self { file, metadata })
    }

    /// What the file was when opened: its size and when it was last changed
    /// tell one version of an original from the next.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
}

/// Makes thumbnails in child processes running `program --thumbnail`.
pub struct Thumbnailer {
    program: PathBuf,
    /// What comes before `--thumbnail` (the sandbox's, on macOS).
    args: Vec<OsString>,
    timeout: Duration,
    /// The owner's opt-in to decode where a child can't be confined.
    allow_unconfined: bool,
}

/// Why a child gave no thumbnail.
#[derive(Debug)]
pub enum Failure {
    /// No child could be started: the platform's fault, not the file's.
    NotStarted(io::Error),
    /// The child couldn't confine itself, so it decoded nothing: the
    /// platform's fault too. Why, as the child says.
    Unconfined(String),
    /// The file holds nothing a thumbnail can be made of: why, as the child
    /// says.
    Unsupported(String),
    /// The child couldn't read the file, so the storage may be at fault.
    Io(String),
    /// The child ended any other way: a decoder bug aborted it, or it wasn't
    /// our child. How it ended, and what it said.
    Crashed(String),
    /// Someone else killed the child: the out-of-memory killer, say, which
    /// may be the host's trouble rather than the file's. How it ended.
    Killed(String),
    /// The child took longer than the time limit, and was killed.
    TimedOut,
    /// The child wrote something other than a thumbnail: what was wrong.
    BadOutput(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted(err) => write!(f, "no child started: {err}"),
            Self::Unconfined(why) => write!(f, "unconfined: {why}"),
            Self::Unsupported(why) => write!(f, "unsupported: {why}"),
            Self::Io(why) => write!(f, "i/o error: {why}"),
            Self::Crashed(why) => write!(f, "crashed: {why}"),
            Self::Killed(why) => write!(f, "killed: {why}"),
            Self::TimedOut => f.write_str("timed out"),
            Self::BadOutput(why) => write!(f, "bad output: {why}"),
        }
    }
}

impl Thumbnailer {
    /// Children running this very program, confined, each given
    /// [`TIME_LIMIT`]. On Linux through `/proc/self/exe`, which still finds
    /// it after an upgrade replaced the file on disk (checked by hand in
    /// Docker), and the child confines itself; on macOS under Seatbelt, as a
    /// child can't confine itself there.
    pub fn own_binary() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let (program, args) = (this_program()?, Vec::new());
        #[cfg(target_os = "macos")]
        let (program, args) = super::sandbox::seatbelt(&this_program()?);
        Ok(Self {
            program,
            args,
            timeout: TIME_LIMIT,
            allow_unconfined: false,
        })
    }

    /// Children running `program` as it is, each given `timeout`.
    pub fn new(program: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            timeout,
            allow_unconfined: false,
        }
    }

    /// The same, letting children decode where they can't be confined, if
    /// `allowed`: the `[thumbnails] allow_unconfined` setting.
    pub fn allowing_unconfined(self, allowed: bool) -> Self {
        Self {
            allow_unconfined: allowed,
            ..self
        }
    }

    /// The thumbnail of `original`, made by a child that is handed only this
    /// file and an empty environment.
    pub async fn thumbnail(&self, original: Original) -> Result<Vec<u8>, Failure> {
        #[expect(clippy::disallowed_methods, reason = "the one place children start")]
        let mut command = tokio::process::Command::new(&self.program);
        let mut child = command
            .args(&self.args)
            .arg(MODE)
            .args(self.allow_unconfined.then_some(ALLOW_UNCONFINED))
            .env_clear()
            .stdin(original.file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Also when the thumbnail is given up on before the child ends.
            .kill_on_drop(true)
            .spawn()
            .map_err(Failure::NotStarted)?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        // Both pipes are read to their end, then the child waited for, as
        // `wait_with_output` does: a child writing more than a pipe holds
        // never blocks.
        let ended = tokio::time::timeout(self.timeout, async {
            let read_stdout = async {
                let stdout = read_up_to(stdout, MAX_THUMBNAIL).await;
                // Output refused: stop the child now, not at the time limit.
                if !matches!(stdout, Ok(Some(_))) {
                    let _ = child.start_kill();
                }
                stdout
            };
            let (stdout, stderr) = tokio::join!(read_stdout, read_keeping(stderr, MAX_MESSAGE));
            let status = child.wait().await;
            Ok::<_, io::Error>((stdout?, stderr?, status?))
        })
        .await;
        let (stdout, stderr, status) = match ended {
            Ok(Ok(ended)) => ended,
            failed => {
                // The child may have ended meanwhile; the wait reaps it
                // either way, or, past `REAP_LIMIT`, tokio does.
                let _ = child.start_kill();
                let _ = tokio::time::timeout(REAP_LIMIT, child.wait()).await;
                return Err(match failed {
                    // Reading its pipes or waiting for it failed: no test can
                    // make either fail, so this is treated as the worst.
                    Ok(Err(err)) => Failure::Crashed(format!("lost its output: {err}")),
                    _ => Failure::TimedOut,
                });
            }
        };
        // Escaped, so what a child says stays on one line of the log.
        let why = || crate::access_log::escape(stderr.trim_ascii_end());
        let Some(stdout) = stdout else {
            return Err(Failure::BadOutput(format!("over {MAX_THUMBNAIL} bytes")));
        };
        match status.code() {
            Some(0) if is_jpeg(&stdout) => Ok(stdout),
            Some(0) => Err(Failure::BadOutput(format!(
                "not a JPEG ({} bytes)",
                stdout.len()
            ))),
            Some(UNSUPPORTED) => Err(Failure::Unsupported(why())),
            Some(UNCONFINED) => Err(Failure::Unconfined(why())),
            Some(IO_ERROR) => Err(Failure::Io(why())),
            _ if status.signal() == Some(SIGKILL) => Err(Failure::Killed(status.to_string())),
            _ => Err(Failure::Crashed(match why() {
                why if why.is_empty() => status.to_string(),
                why => format!("{status}: {why}"),
            })),
        }
    }
}

/// The running program's file: on Linux `/proc/self/exe`, which still finds
/// it after an upgrade replaced the file on disk.
fn this_program() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    return Ok(PathBuf::from("/proc/self/exe"));
    #[cfg(target_os = "macos")]
    #[expect(clippy::disallowed_methods, reason = "where /proc/self/exe isn't")]
    std::env::current_exe()
}

/// Which build is running: [`file_id`] of this program. Reads the whole
/// binary (a few MB), so is taken once, at start.
pub fn build() -> io::Result<String> {
    file_id(&this_program()?)
}

/// A file told by its contents: the first 8 bytes of their SHA-256, in hex.
fn file_id(path: &Path) -> io::Result<String> {
    let digest = crate::crypto::sha256(&std::fs::read(path)?);
    Ok(digest[..BUILD_ID_BYTES]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Whether `bytes` look like a whole JPEG: its start marker and a marker's
/// `FF` (3 bytes), then its end marker (2), apart, so 5 bytes at least.
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= 5 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) && bytes.ends_with(&[0xFF, 0xD9])
}

/// All of `pipe`, or `None` once it holds more than `limit` bytes.
async fn read_up_to(pipe: impl AsyncRead + Unpin, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    pipe.take(limit as u64 + 1).read_to_end(&mut bytes).await?;
    Ok((bytes.len() <= limit).then_some(bytes))
}

/// The first `keep` bytes of `pipe`; the rest is read and dropped, so the
/// child never blocks writing it.
async fn read_keeping(mut pipe: impl AsyncRead + Unpin, keep: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut pipe)
        .take(keep as u64)
        .read_to_end(&mut bytes)
        .await?;
    tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await?;
    Ok(bytes)
}

/// `file` and its metadata, if it is a regular file: the type of what was
/// opened, not of what a path names.
fn regular_file(file: File) -> io::Result<(File, Metadata)> {
    let metadata = file.metadata()?;
    if metadata.is_file() {
        Ok((file, metadata))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ))
    }
}

/// Stdin as the file the server opened for this child. A `File` must own its
/// handle, so it gets a duplicate of fd 0: owning fd 0 itself needs `unsafe`.
fn stdin_file() -> io::Result<File> {
    let handle = io::stdin().as_fd().try_clone_to_owned()?;
    Ok(File::from(handle))
}

#[cfg(test)]
mod tests {
    use super::super::testing::{JPEG, fake};
    use super::*;

    /// Whether the fake child `name` (which wrote its pid to `name.pid`) still
    /// exists, zombie or not: `kill -0` finds both.
    fn is_running(name: &str) -> bool {
        let pid = std::fs::read_to_string(fake(&format!("{name}.pid"))).unwrap();
        #[expect(clippy::disallowed_methods, reason = "checks a child is gone")]
        let mut kill = std::process::Command::new("/bin/sh");
        let found = kill.args(["-c", &format!("kill -0 {} 2>/dev/null", pid.trim())]);
        found.status().unwrap().success()
    }

    /// The failure of `thumbnailer` on `original`, which must come well
    /// before a 30 s time limit: a refused child is stopped at once.
    async fn failure_soon(thumbnailer: &Thumbnailer, original: Original) -> Failure {
        let started = std::time::Instant::now();
        let failure = thumbnailer.thumbnail(original).await.unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        failure
    }

    /// A file holding `bytes`, opened as the original handed to the child. The
    /// file's name is gone by the time the child starts.
    fn original(bytes: &[u8]) -> Original {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        Original::open(file.path()).unwrap()
    }

    #[tokio::test]
    async fn returns_the_thumbnail_the_child_writes() {
        let thumbnailer = Thumbnailer::new(fake("echo"), Duration::from_secs(5));
        assert_eq!(thumbnailer.thumbnail(original(JPEG)).await.unwrap(), JPEG);
    }

    #[tokio::test]
    async fn passes_on_why_a_file_is_unsupported() {
        let thumbnailer = Thumbnailer::new(fake("unsupported"), Duration::from_secs(5));
        let failure = thumbnailer.thumbnail(original(b"")).await.unwrap_err();
        assert!(
            matches!(&failure, Failure::Unsupported(why) if why == "damaged file"),
            "{failure:?}"
        );
    }

    #[tokio::test]
    async fn tells_a_read_error_from_an_unsupported_file() {
        let thumbnailer = Thumbnailer::new(fake("unreadable"), Duration::from_secs(5));
        let failure = thumbnailer.thumbnail(original(b"")).await.unwrap_err();
        assert!(
            matches!(&failure, Failure::Io(why) if why == "Input/output error"),
            "{failure:?}"
        );
        assert_eq!(failure.to_string(), "i/o error: Input/output error");
    }

    /// A child that couldn't confine itself decoded nothing: the platform's
    /// fault, as a child that couldn't start, never the file's.
    #[tokio::test]
    async fn tells_when_the_child_could_not_be_confined() {
        let thumbnailer = Thumbnailer::new(fake("unconfined"), Duration::from_secs(5));
        let failure = thumbnailer.thumbnail(original(b"")).await.unwrap_err();
        assert!(matches!(failure, Failure::Unconfined(_)), "{failure:?}");
        assert_eq!(
            failure.to_string(),
            "unconfined: sandbox: Landlock is not supported"
        );
    }

    /// Any end but those the child's exit codes say is a crash, whether the
    /// decoder aborted or another program exited; a kill is told apart below.
    #[tokio::test]
    async fn tells_a_crash_from_an_unsupported_file() {
        for (fake_name, says) in [("aborts", "decoder bug"), ("exits", "decoder bug")] {
            let thumbnailer = Thumbnailer::new(fake(fake_name), Duration::from_secs(5));
            let failure = thumbnailer.thumbnail(original(JPEG)).await.unwrap_err();
            assert!(
                matches!(&failure, Failure::Crashed(why) if why.ends_with(says)),
                "{fake_name}: {failure:?}"
            );
            let line = failure.to_string();
            assert!(
                line.starts_with("crashed: ") && line.ends_with(says),
                "{line}"
            );
        }
    }

    /// The owner's opt-in reaches the child as a flag, and only when given:
    /// without it, a child that can't be confined refuses.
    #[tokio::test]
    async fn passes_the_opt_in_to_decode_unconfined() {
        for (allowed, args) in [
            (false, MODE.to_string()),
            (true, format!("{MODE} {ALLOW_UNCONFINED}")),
        ] {
            let thumbnailer = Thumbnailer::new(fake("args"), Duration::from_secs(5));
            let output = thumbnailer
                .allowing_unconfined(allowed)
                .thumbnail(original(b""))
                .await
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&output[3..output.len() - 2]), args);
        }
    }

    /// A file is told by its contents: the start of their SHA-256 (that of
    /// "abc" from FIPS 180-2), the same for the same bytes.
    #[test]
    fn a_file_is_told_by_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let (abc, other) = (dir.path().join("abc"), dir.path().join("other"));
        std::fs::write(&abc, b"abc").unwrap();
        std::fs::write(&other, b"abd").unwrap();
        assert_eq!(file_id(&abc).unwrap(), "ba7816bf8f01cfea");
        assert_ne!(file_id(&other).unwrap(), file_id(&abc).unwrap());
    }

    /// A child killed by someone else (the out-of-memory killer, say) may be
    /// the host's trouble rather than the file's: it is told from a crash.
    #[tokio::test]
    async fn tells_a_kill_from_a_crash() {
        let thumbnailer = Thumbnailer::new(fake("killed"), Duration::from_secs(5));
        let failure = thumbnailer.thumbnail(original(JPEG)).await.unwrap_err();
        assert!(matches!(failure, Failure::Killed(_)), "{failure:?}");
        assert!(failure.to_string().starts_with("killed: "), "{failure}");
    }

    /// A child that never ends is killed at the time limit, and waited for:
    /// no zombie is left behind, which `kill -0` would still find.
    #[tokio::test]
    async fn kills_a_child_that_takes_too_long() {
        // Long enough for the child to say which process it is first.
        let thumbnailer = Thumbnailer::new(fake("hangs"), Duration::from_secs(2));
        let failure = failure_soon(&thumbnailer, original(JPEG)).await;
        assert!(matches!(failure, Failure::TimedOut), "{failure:?}");
        assert_eq!(failure.to_string(), "timed out");
        assert!(!is_running("hangs"), "the child is still there");
    }

    /// Output larger than any thumbnail is refused, and the child stopped at
    /// once rather than at the time limit.
    #[tokio::test]
    async fn refuses_output_larger_than_a_thumbnail() {
        let thumbnailer = Thumbnailer::new(fake("floods"), Duration::from_secs(30));
        let failure = failure_soon(&thumbnailer, original(b"")).await;
        assert!(matches!(failure, Failure::BadOutput(_)), "{failure:?}");
        assert_eq!(failure.to_string(), "bad output: over 8388608 bytes");
    }

    /// A child that carries on after its output was refused is killed at
    /// once, not left to the time limit.
    #[tokio::test]
    async fn kills_a_child_whose_output_was_refused() {
        let thumbnailer = Thumbnailer::new(fake("persists"), Duration::from_secs(30));
        let failure = failure_soon(&thumbnailer, original(b"")).await;
        assert!(matches!(failure, Failure::BadOutput(_)), "{failure:?}");
    }

    /// Thumbnails up to 8 MiB pass; one byte more is refused, even from a
    /// child that says it succeeded, and even after a whole JPEG.
    #[tokio::test]
    async fn refuses_output_from_one_byte_over_the_limit() {
        let thumbnailer = Thumbnailer::new(fake("sized"), Duration::from_secs(10));
        let largest = thumbnailer.thumbnail(original(b"8388608")).await.unwrap();
        assert_eq!(largest.len(), 8 << 20);
        for over in [&b"8388609"[..], b"8388608 x"] {
            let failure = thumbnailer.thumbnail(original(over)).await.unwrap_err();
            assert!(
                matches!(failure, Failure::BadOutput(_)),
                "{over:?}: {failure:?}"
            );
        }
    }

    /// Only a JPEG counts as a thumbnail, so nothing else is ever cached.
    #[tokio::test]
    async fn refuses_output_that_is_not_a_jpeg() {
        for not_jpeg in [
            &b""[..],
            b"\xFF\xD8\xFF cut off",
            b"\xFF\xD9",
            b"\xFF\xD8\xFF\xD9",
        ] {
            let thumbnailer = Thumbnailer::new(fake("echo"), Duration::from_secs(5));
            let failure = thumbnailer.thumbnail(original(not_jpeg)).await.unwrap_err();
            assert!(
                matches!(failure, Failure::BadOutput(_)),
                "{not_jpeg:?}: {failure:?}"
            );
        }
        let thumbnailer = Thumbnailer::new(fake("chatters"), Duration::from_secs(5));
        let failure = thumbnailer.thumbnail(original(b"")).await.unwrap_err();
        assert!(matches!(failure, Failure::BadOutput(_)), "{failure:?}");
        assert_eq!(failure.to_string(), "bad output: not a JPEG (14 bytes)");
    }

    /// The child gets an empty environment: no credentials (`AUTH_PASS`)
    /// or settings reach the decoder. The shell adds only its own.
    #[tokio::test]
    async fn the_child_gets_no_environment() {
        let thumbnailer = Thumbnailer::new(fake("env"), Duration::from_secs(5));
        let output = thumbnailer.thumbnail(original(b"")).await.unwrap();
        let env = String::from_utf8_lossy(&output[3..output.len() - 2]).into_owned();
        let names: Vec<_> = env
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .filter(|name| !["PWD", "SHLVL", "_"].contains(name))
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    /// Failures are logged as one line each, whatever a child writes: its
    /// message is escaped as the access log escapes, so it can't add lines.
    #[tokio::test]
    async fn a_child_cannot_forge_log_lines() {
        let thumbnailer = Thumbnailer::new(fake("forges"), Duration::from_secs(5));
        let failure = thumbnailer.thumbnail(original(b"")).await.unwrap_err();
        assert_eq!(
            failure.to_string(),
            r"unsupported: damaged\x0athumbnail abc: made"
        );
    }

    /// A child that can't start is the platform's fault, never the file's.
    #[tokio::test]
    async fn tells_when_no_child_could_start() {
        let thumbnailer = Thumbnailer::new("/nonexistent/rust-albums", Duration::from_secs(5));
        let failure = thumbnailer.thumbnail(original(JPEG)).await.unwrap_err();
        assert!(
            matches!(&failure, Failure::NotStarted(err) if err.kind() == io::ErrorKind::NotFound),
            "{failure:?}"
        );
        assert!(
            failure.to_string().starts_with("no child started: "),
            "{failure}"
        );
    }

    /// A thumbnail given up on (the server shutting down, say) kills its child
    /// rather than leaving it to run to the time limit.
    #[tokio::test]
    async fn a_thumbnail_given_up_on_kills_its_child() {
        let thumbnailer = Thumbnailer::new(fake("lingers"), Duration::from_secs(60));
        let given_up = tokio::time::timeout(
            Duration::from_secs(2),
            thumbnailer.thumbnail(original(JPEG)),
        );
        assert!(given_up.await.is_err());
        // Reaping a dropped child is up to tokio, soon but not at once, and
        // only while the runtime runs: so the wait is asynchronous.
        let mut gone = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !is_running("lingers") {
                gone = true;
                break;
            }
        }
        assert!(gone, "the child is still there");
    }

    /// The child is handed the original read-only: writing to its stdin fails.
    #[tokio::test]
    async fn the_child_cannot_change_the_original() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), JPEG).unwrap();
        let thumbnailer = Thumbnailer::new(fake("tampers"), Duration::from_secs(5));
        let failure = thumbnailer
            .thumbnail(Original::open(file.path()).unwrap())
            .await
            .unwrap_err();
        assert!(matches!(failure, Failure::Io(_)), "{failure:?}");
        assert_eq!(std::fs::read(file.path()).unwrap(), JPEG);
    }

    /// Each of the server's children gets 15 seconds, as asked.
    #[test]
    fn the_server_gives_each_child_15_seconds() {
        let thumbnailer = Thumbnailer::own_binary().unwrap();
        assert_eq!(thumbnailer.timeout, Duration::from_secs(15));
    }

    /// On macOS the server starts its children confined, through
    /// `sandbox-exec` by its full path, around this very program.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_server_starts_its_children_confined() {
        let thumbnailer = Thumbnailer::own_binary().unwrap();
        assert_eq!(thumbnailer.program, Path::new("/usr/bin/sandbox-exec"));
        #[expect(clippy::disallowed_methods, reason = "compares with this program")]
        let this_program = std::env::current_exe().unwrap();
        assert_eq!(
            thumbnailer.args.last(),
            Some(&this_program.into_os_string())
        );
    }

    /// The server's children are the server's own binary: in tests, the test
    /// binary, whose harness refuses `--thumbnail`.
    #[tokio::test]
    async fn the_server_starts_its_own_binary() {
        let failure = Thumbnailer::own_binary()
            .unwrap()
            .thumbnail(original(JPEG))
            .await
            .unwrap_err();
        assert!(
            matches!(&failure, Failure::Crashed(why) if why.contains("Unrecognized option: 'thumbnail'")),
            "{failure:?}"
        );
    }

    /// Only the start of a long message is kept for the log; the rest is
    /// read and dropped, so the child isn't held up writing it.
    #[tokio::test]
    async fn keeps_only_the_start_of_a_long_message() {
        let thumbnailer = Thumbnailer::new(fake("rambles"), Duration::from_secs(5));
        let failure = thumbnailer.thumbnail(original(b"")).await.unwrap_err();
        assert!(
            matches!(&failure, Failure::Unsupported(why) if *why == "x".repeat(4096)),
            "{failure:?}"
        );
    }

    /// Only a regular file is an original: a folder is refused when opened, not
    /// passed to a child as if the storage had failed.
    #[test]
    fn only_a_regular_file_is_opened_as_an_original() {
        let dir = tempfile::tempdir().unwrap();
        let err = Original::open(dir.path()).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "not a regular file");
    }

    /// Quotes, backslashes, control and non-UTF-8 bytes from a child are
    /// escaped as the access log escapes them.
    #[tokio::test]
    async fn escapes_whatever_a_child_says() {
        let thumbnailer = Thumbnailer::new(fake("garbles"), Duration::from_secs(5));
        let failure = thumbnailer.thumbnail(original(b"")).await.unwrap_err();
        assert_eq!(
            failure.to_string(),
            r"unsupported: say \x22hi\x22 \x5c \xff\x09end"
        );
    }
}
