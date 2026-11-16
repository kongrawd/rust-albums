//! Helpers shared by the tests of every step: reading thumbnails back, and
//! stand-ins for the child process.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use {
    super::child::{Original, Thumbnailer},
    super::service::{AuditLog, Thumbnails},
    std::os::unix::fs::PermissionsExt,
    std::path::{Path, PathBuf},
    std::sync::OnceLock,
    std::time::Duration,
};

/// What fake children write as a thumbnail: a JPEG's start and end markers
/// around a few bytes.
pub const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0 a thumbnail \xFF\xD9";
/// The folder of fake children, written once (see [`fake`]).
#[cfg(any(target_os = "linux", target_os = "macos"))]
static FAKES: OnceLock<tempfile::TempDir> = OnceLock::new();

#[cfg(any(target_os = "linux", target_os = "macos"))]
/// Stand-ins for the child, as shell scripts. All are written before any
/// test starts one: a script still open for writing when another thread
/// forks can't be run ("text file busy").
pub fn fake(name: &str) -> PathBuf {
    let dir = FAKES.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        for (name, script) in [
            // Writes back the "video", so tests choose the output.
            ("echo", "cat"),
            ("unsupported", "echo 'damaged file' >&2; exit 65"),
            // Quotes, a backslash, a tab and a byte that isn't UTF-8.
            ("garbles", r#"printf 'say "hi" \\ \377\tend\n' >&2; exit 65"#),
            // A message that tries to add a line of its own to the log.
            (
                "forges",
                "printf 'damaged\\nthumbnail abc: made\\n' >&2; exit 65",
            ),
            ("unreadable", "echo 'Input/output error' >&2; exit 74"),
            (
                "unconfined",
                "echo 'sandbox: Landlock is not supported' >&2; exit 71",
            ),
            // A decoder bug under `panic = "abort"`.
            ("aborts", "echo 'decoder bug' >&2; kill -ABRT $$"),
            // As the out-of-memory killer does.
            ("killed", "kill -KILL $$"),
            // A panic in a test build, or another program's exit code.
            ("exits", "echo 'decoder bug' >&2; exit 101"),
            // Says which process it is, then never ends.
            ("hangs", "echo $$ > \"$0.pid\"; exec sleep 30"),
            // Never ends either, for tests that need no pid.
            ("sleeps", "exec sleep 30"),
            // Writes back the "video", slowly.
            ("unhurried", "sleep 0.3; cat"),
            // Can't be confined, slowly, and counts its runs in `.runs`.
            (
                "unconfined-counted",
                "echo run >> \"$0.runs\"; sleep 0.2; echo 'sandbox: Landlock is not supported' >&2; exit 71",
            ),
            // Its arguments, as a "JPEG".
            ("args", "printf '\\377\\330\\377%s\\377\\331' \"$*\""),
            // The same, for another test: each needs its own pid file.
            ("lingers", "echo $$ > \"$0.pid\"; exec sleep 30"),
            // Tries to change the video it was given.
            ("tampers", "printf tampered >&0 || exit 74"),
            // A JPEG start, then output without end.
            ("floods", "printf '\\377\\330\\377'; exec cat /dev/zero"),
            ("chatters", "echo 'not a picture'"),
            // A JPEG of as many bytes as its "video" says, then what else
            // it says, then success.
            (
                "sized",
                "set -- $(cat); printf '\\377\\330\\377'; head -c $(($1 - 5)) /dev/zero; printf '\\377\\331%s' \"${2:-}\"",
            ),
            // Floods too, but ignores the closed pipe and carries on.
            (
                "persists",
                "trap '' PIPE; printf '\\377\\330\\377'; cat /dev/zero; exec sleep 30",
            ),
            // A long message, then exit 65 only if all of it was written,
            // as the child's `eprintln!` fails if it can't be.
            (
                "rambles",
                "head -c 100000 /dev/zero | tr '\\0' x >&2 && exit 65",
            ),
            // Its environment, as a "JPEG".
            ("env", "printf '\\377\\330\\377'; env; printf '\\377\\331'"),
            // Counts the children running at once in `slow.most`, slowly.
            (
                "slow",
                "d=\"$0.d\"; mkdir -p \"$d\"; : > \"$d/$$\"; ls \"$d\" | wc -l >> \"$0.most\"; sleep 0.3; rm \"$d/$$\"; cat",
            ),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    });
    dir.path().join(name)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
/// Where a [`service`] over `dir` keeps its thumbnails.
pub fn cache(dir: &Path) -> PathBuf {
    dir.join("thumbnails")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
/// A thumbnail service keeping thumbnails in [`cache`]`(dir)`, made by
/// `child` given `limit`, as `build`; and its audit log.
pub fn service(
    child: PathBuf,
    limit: Duration,
    build: &str,
    dir: &Path,
) -> (Thumbnails, crate::access_log::Lines) {
    let lines = crate::access_log::Lines::default();
    let thumbnailer = Thumbnailer::new(child, limit);
    let log = AuditLog::Memory(lines.clone());
    let thumbnails = Thumbnails::new(cache(dir), thumbnailer, build.into(), log);
    (thumbnails, lines)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
/// The file at `path`, opened as the original handed to a child.
pub fn open(path: &Path) -> Original {
    Original::open(path).unwrap()
}

/// Decodes a thumbnail: its width, height and RGB pixels. It must be a
/// baseline JPEG in colour.
pub fn decode_jpeg(jpeg: &[u8]) -> (usize, usize, Vec<u8>) {
    let mut decoder = jpeg_decoder::Decoder::new(jpeg);
    let pixels = decoder.decode().unwrap();
    let info = decoder.info().unwrap();
    assert_eq!(info.pixel_format, jpeg_decoder::PixelFormat::RGB24);
    assert_eq!(
        info.coding_process,
        jpeg_decoder::CodingProcess::DctSequential
    );
    (usize::from(info.width), usize::from(info.height), pixels)
}
