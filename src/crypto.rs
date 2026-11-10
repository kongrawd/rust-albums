//! Hashing, constant-time comparison, randomness and secret files, shared by
//! credentials, sessions, link ids and TLS.

use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
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
