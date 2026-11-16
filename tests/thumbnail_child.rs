//! `rust-albums --thumbnail`: the child process the server decodes videos in.
//! It reads the video from stdin, which must be a regular file, and writes
//! the thumbnail to stdout.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const PORTRAIT: &str = "src/thumbnail/testdata/portrait.mov";

/// Runs the child as the server does, from the repository's folder.
fn thumbnail(stdin: impl Into<Stdio>) -> Output {
    child(Path::new(ROOT), stdin).output().unwrap()
}

/// The child as the server starts it: `stdin` as its video, no environment,
/// from the working folder `dir`.
fn child(dir: &Path, stdin: impl Into<Stdio>) -> Command {
    #[expect(clippy::disallowed_methods, reason = "runs the binary under test")]
    let mut command = Command::new(env!("CARGO_BIN_EXE_rust-albums"));
    command
        .arg("--thumbnail")
        .env_clear()
        .current_dir(dir)
        .stdin(stdin);
    command
}

fn testdata(path: &str) -> File {
    File::open(Path::new(ROOT).join(path)).unwrap()
}

#[test]
fn writes_the_thumbnail_of_the_video_on_stdin() {
    let output = thumbnail(testdata(PORTRAIT));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let mut decoder = jpeg_decoder::Decoder::new(&output.stdout[..]);
    decoder.decode().unwrap();
    let info = decoder.info().unwrap();
    assert_eq!((info.width, info.height), (64, 96), "upright");
}

/// On macOS the server starts the child under its Seatbelt profile (on
/// Linux the child confines itself, in every test here): under it, the child
/// still makes the thumbnail. The command is built as `sandbox::seatbelt`
/// builds it, which tests can't reach from here.
#[cfg(target_os = "macos")]
#[test]
fn writes_the_thumbnail_under_the_sandbox_profile() {
    let profile = std::fs::read_to_string(Path::new(ROOT).join("src/thumbnail/child.sb")).unwrap();
    let program = env!("CARGO_BIN_EXE_rust-albums");
    #[expect(clippy::disallowed_methods, reason = "runs the binary under test")]
    let mut command = Command::new("/usr/bin/sandbox-exec");
    let output = command
        .args(["-p", &profile, "-D", &format!("PROGRAM={program}"), program])
        .arg("--thumbnail")
        .env_clear()
        .stdin(testdata(PORTRAIT))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let mut decoder = jpeg_decoder::Decoder::new(&output.stdout[..]);
    decoder.decode().unwrap();
    let info = decoder.info().unwrap();
    assert_eq!((info.width, info.height), (64, 96));
}

/// A file with nothing to make a thumbnail of exits with sysexits'
/// `EX_DATAERR` and says why on stderr.
#[test]
fn says_why_a_file_has_no_thumbnail() {
    let audio = tempfile::tempfile().unwrap();
    (&audio).write_all(b"\0\0\0\x0cftypM4A ").unwrap();
    let output = thumbnail(audio);
    assert_eq!(output.status.code(), Some(65), "{output:?}");
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "no index (moov box)\n"
    );
}

/// The child only reads the video and writes the thumbnail: no config is
/// loaded or created, nothing is written beside it, and the video is left
/// as it was.
#[test]
fn has_no_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("portrait.mov");
    std::fs::copy(Path::new(ROOT).join(PORTRAIT), &video).unwrap();
    let before = (
        std::fs::read(&video).unwrap(),
        video.metadata().unwrap().modified().unwrap(),
    );
    let output = child(dir.path(), File::open(&video).unwrap())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["portrait.mov"]);
    let after = (
        std::fs::read(&video).unwrap(),
        video.metadata().unwrap().modified().unwrap(),
    );
    assert!(before == after, "the video changed");
}

/// A thumbnail that can't be written out, as the server stopped reading, is
/// a failure the child reports, not a crash.
#[test]
fn says_when_the_thumbnail_cannot_be_written() {
    // A pipe nobody reads: every write to it fails (the child ignores
    // SIGPIPE, as Rust programs do).
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = child(Path::new(ROOT), testdata(PORTRAIT))
        .stdout(writer)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(74), "{output:?}");
    let says = String::from_utf8_lossy(&output.stderr);
    assert!(says.starts_with("writing the thumbnail: "), "{says}");
}

/// Only a regular file is read: a pipe or a terminal is refused, as an I/O
/// error.
#[test]
fn refuses_stdin_that_is_not_a_file() {
    for stdin in [Stdio::null(), Stdio::piped()] {
        let output = thumbnail(stdin);
        assert_eq!(output.status.code(), Some(74), "{output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "not a regular file\n"
        );
    }
}
