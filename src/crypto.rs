//! Hashing, constant-time comparison, randomness, secret files and atomic file
//! writes, shared by credentials, sessions, link ids, TLS and the config.

use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use std::io::Write;
use std::time::{Duration, Instant};
use std::{fs, io, path::Path};
use subtle::ConstantTimeEq;

pub type Digest = [u8; 32];

pub fn sha256(data: &[u8]) -> Digest {
    digest(&SHA256, data)
        .as_ref()
        .try_into()
        .expect("SHA-256 is 32 bytes")
}

/// Compares digests without revealing, through timing, where they differ.
/// Uses `subtle`: ring's `constant_time` is deprecated and makes "no promises
/// regarding side channels".
pub fn digests_equal(a: &Digest, b: &Digest) -> bool {
    a.ct_eq(b).into()
}

/// `N` bytes from the operating system's secure random number generator.
pub fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("OS random number generator failed");
    bytes
}

/// Creates a file readable only by the owner (mode 0600 on unix), creating its
/// folder. Fails with `AlreadyExists` instead of replacing an existing file, so
/// two processes starting at once can't overwrite each other's secret. The
/// data and the new name are flushed to disk before returning.
pub fn create_secret_file(path: &Path, data: &[u8]) -> io::Result<()> {
    create_parent(path)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    sync_parent(path)
}

/// Writes `data` to `path` atomically and durably, replacing any file there:
/// the data goes to a temporary file beside it, which is fsynced and renamed
/// into place, then the folder is fsynced. Readers see the old file or the new
/// one, never a partial file. A failed write removes the temporary file and
/// leaves `path` untouched, so the write can simply be retried.
pub fn write_file_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
    create_parent(path)?;
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    // A random suffix: every container's main process is PID 1, so a PID could
    // collide on a shared volume.
    let suffix = u64::from_le_bytes(random::<8>());
    let temporary = path.with_file_name(format!("{file_name}.tmp-{suffix:016x}"));
    let written = fs::File::create(&temporary)
        .and_then(|mut file| file.write_all(data).and_then(|()| file.sync_all()))
        .and_then(|()| fs::rename(&temporary, path));
    if let Err(err) = written {
        let _ = fs::remove_file(&temporary);
        return Err(err);
    }
    sync_parent(path)
}

fn create_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => fs::create_dir_all(dir),
        _ => Ok(()),
    }
}

/// Flushes the folder holding `path`, so a name just created or renamed there
/// survives a power cut. Filesystems that can't sync folders are skipped.
fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        match fs::File::open(dir).and_then(|folder| folder.sync_all()) {
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
                ) => {}
            other => return other,
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// How long to wait for secret files another process is creating at the same
/// moment (e.g. two containers doing their first start on one state volume).
pub const CONCURRENT_CREATION_WAIT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Calls `load` until it succeeds or `wait` has passed, returning the last
/// error. Pairs with [`create_secret_file`]: the process that gets
/// `AlreadyExists` waits here until the creating process has finished.
/// A `wait` of zero tries once.
pub fn load_when_ready<T>(
    wait: Duration,
    mut load: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let deadline = Instant::now() + wait;
    loop {
        match load() {
            Ok(loaded) => return Ok(loaded),
            Err(_) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_file_atomically_replaces_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/albums.toml");
        write_file_atomically(&path, b"first").unwrap();
        write_file_atomically(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["albums.toml"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_atomic_write_leaves_nothing_behind() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let read_only = dir.path().join("read-only");
        fs::create_dir(&read_only).unwrap();
        fs::set_permissions(&read_only, fs::Permissions::from_mode(0o555)).unwrap();
        assert!(write_file_atomically(&read_only.join("albums.toml"), b"x").is_err());
        assert_eq!(fs::read_dir(&read_only).unwrap().count(), 0);
        fs::set_permissions(&read_only, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn load_when_ready_retries_until_success_or_deadline() {
        let mut attempts = 0;
        let loaded = load_when_ready(Duration::from_secs(1), || {
            attempts += 1;
            if attempts < 3 {
                Err("not yet".to_string())
            } else {
                Ok(attempts)
            }
        });
        assert_eq!(loaded, Ok(3));

        let mut attempts = 0;
        let failed: Result<(), String> = load_when_ready(Duration::ZERO, || {
            attempts += 1;
            Err("never".to_string())
        });
        assert_eq!((failed, attempts), (Err("never".to_string()), 1));
    }
}
