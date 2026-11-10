//! Hashing, constant-time comparison, randomness and secret files, shared by
//! credentials, sessions, link ids and TLS.

use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
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
/// two processes starting at once can't overwrite each other's secret. The data
/// is flushed to disk before returning.
pub fn create_secret_file(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()
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
