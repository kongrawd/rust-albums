//! TLS setup: rustls server config (using the ring provider that main.rs
//! installs) with a configurable minimum version.
//!
//! Works with PEM certificates mounted read-only into a container (certbot,
//! Kubernetes secrets): a self-signed pair is only generated when both files
//! are missing, and never overwrites anything. Certificates are read once, at
//! startup.
//!
//! Generating is safe when two processes start at once (e.g. two containers on
//! one state volume): the key is created with `create_new`, so exactly one
//! process writes a pair; the certificate is written atomically, so it is never
//! read half-written; the other process waits briefly and loads the pair that
//! was written. Both files are fsynced together with their folder, so they
//! survive a power cut. A failed write removes what it wrote, so the next start
//! can generate again.

use crate::config::{self, TlsVersion};
use crate::crypto;
use axum_server::tls_rustls::RustlsConfig;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair, SerialNumber,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use rustls::{ServerConfig, version};
use std::fs;
use std::io::{self, ErrorKind};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// First year the generated certificate is valid. Before 2019-07-01, so
/// Apple's 825-day limit for certificates issued after that date doesn't
/// apply (see [`self_signed_params`]).
const SELF_SIGNED_VALID_FROM_YEAR: i32 = 1975;
/// Year the generated certificate expires: never in practice, since it is
/// never renewed.
const SELF_SIGNED_VALID_UNTIL_YEAR: i32 = 4096;
/// Length of the generated certificate's random serial number.
const SERIAL_BYTES: usize = 16;

/// Outcome of trying to generate a self-signed pair.
enum Generation {
    Written,
    /// Another process created the key first and is writing the pair.
    ByAnotherProcess,
}

/// Builds the server TLS config, generating a self-signed pair on first run
/// (when neither file exists) so the browser exception survives restarts.
pub fn load_or_create(settings: &config::Tls) -> Result<RustlsConfig, String> {
    load_or_create_waiting(settings, crypto::CONCURRENT_CREATION_WAIT)
}

/// [`load_or_create`], waiting up to `wait` for a pair another process is
/// writing at the same moment.
fn load_or_create_waiting(settings: &config::Tls, wait: Duration) -> Result<RustlsConfig, String> {
    let wait = match (file_exists(&settings.cert)?, file_exists(&settings.key)?) {
        (true, true) => Duration::ZERO,
        (false, false) => match create_self_signed(settings)? {
            Generation::Written => Duration::ZERO,
            Generation::ByAnotherProcess => wait,
        },
        // The key is written first, so this may be another process mid-generation.
        (false, true) => wait,
        (true, false) => return Err(one_missing(&settings.cert, &settings.key)),
    };
    let server_config = crypto::load_when_ready(wait, || load(settings))?;
    Ok(RustlsConfig::from_config(server_config))
}

fn load(settings: &config::Tls) -> Result<Arc<ServerConfig>, String> {
    let cert_pem = fs::read(&settings.cert).map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            one_missing(&settings.key, &settings.cert)
        } else {
            access_error(&settings.cert, &err)
        }
    })?;
    let key_pem = fs::read(&settings.key).map_err(|err| access_error(&settings.key, &err))?;
    server_config(&cert_pem, &key_pem, settings.min_version)
}

fn server_config(
    cert_pem: &[u8],
    key_pem: &[u8],
    min_version: TlsVersion,
) -> Result<Arc<ServerConfig>, String> {
    let chain = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("TLS certificate: {e}"))?;
    if chain.is_empty() {
        return Err("TLS certificate: no PEM certificate found".into());
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| format!("TLS key: {e}"))?;
    let versions: &[&rustls::SupportedProtocolVersion] = match min_version {
        TlsVersion::Tls13 => &[&version::TLS13],
        TlsVersion::Tls12 => &[&version::TLS13, &version::TLS12],
    };
    let mut server_config = ServerConfig::builder_with_protocol_versions(versions)
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| format!("TLS certificate and key don't form a valid pair: {e}"))?;
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(server_config))
}

fn one_missing(present: &Path, missing: &Path) -> String {
    format!(
        "TLS: {} exists but {} is missing. Provide both, or remove both to generate a \
         self-signed pair (a first start that was interrupted can leave just the key)",
        present.display(),
        missing.display()
    )
}

/// Whether `path` exists. Only a missing file counts as missing: a symlink to
/// a file that isn't there, or a path that can't be accessed, is an error.
fn file_exists(path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == ErrorKind::NotFound => match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
                "TLS: {} is a symlink to a file that isn't there. With certbot, mount the \
                 whole /etc/letsencrypt folder, not just live/",
                path.display()
            )),
            _ => Ok(false),
        },
        Err(err) => Err(access_error(path, &err)),
    }
}

fn access_error(path: &Path, err: &io::Error) -> String {
    if err.kind() == ErrorKind::PermissionDenied {
        format!(
            "TLS: can't read {}: permission denied. Make it readable by the user the server \
             runs as (in Docker: the container user, see docs/docker.md)",
            path.display()
        )
    } else {
        format!("TLS: {}: {err}", path.display())
    }
}

/// The generated certificate, set explicitly rather than left to rcgen's
/// defaults, so that browsers on every platform let you accept it:
///
/// - Names only in the subjectAltName: Chrome and Apple ignore the CommonName.
/// - The serverAuth extended key usage, which Apple requires.
/// - Valid from [`SELF_SIGNED_VALID_FROM_YEAR`] to
///   [`SELF_SIGNED_VALID_UNTIL_YEAR`]. Apple refuses a certificate issued
///   (NotBefore) after 2019-07-01 that is valid for more than 825 days, an
///   error Chrome on macOS shows as "revoked" with no way to proceed. It is
///   never renewed, so one long-lived certificate also keeps the browser's
///   exception.
/// - A random serial: Firefox refuses, with no way to proceed, a regenerated
///   certificate whose issuer and serial match one it already accepted.
fn self_signed_params(names: &[String]) -> Result<CertificateParams, String> {
    let mut params = CertificateParams::new(names.to_vec())
        .map_err(|e| format!("TLS: self-signed certificate names {names:?}: {e}"))?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "rust-albums self-signed");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = rcgen::date_time_ymd(SELF_SIGNED_VALID_FROM_YEAR, 1, 1);
    params.not_after = rcgen::date_time_ymd(SELF_SIGNED_VALID_UNTIL_YEAR, 1, 1);
    let mut serial = crypto::random::<SERIAL_BYTES>();
    serial[0] &= 0x7f; // positive
    params.serial_number = Some(SerialNumber::from_slice(&serial));
    Ok(params)
}

fn create_self_signed(settings: &config::Tls) -> Result<Generation, String> {
    let generation_error =
        |e: rcgen::Error| format!("TLS: generating a self-signed certificate: {e}");
    let params = self_signed_params(&settings.self_signed_names)?;
    let signing_key = KeyPair::generate().map_err(generation_error)?;
    let cert = params.self_signed(&signing_key).map_err(generation_error)?;
    let write_error = |path: &Path, err: io::Error| {
        let hint = if err.kind() == ErrorKind::ReadOnlyFilesystem {
            " (read-only mount: provide a certificate and key, or make the folder writable)"
        } else {
            ""
        };
        format!("TLS: writing {}: {err}{hint}", path.display())
    };
    let key_pem = signing_key.serialize_pem();
    match crypto::create_secret_file(&settings.key, key_pem.as_bytes()) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            return Ok(Generation::ByAnotherProcess);
        }
        Err(err) => return Err(write_error(&settings.key, err)),
    }
    // Written atomically, so it is never read half-written.
    if let Err(err) = crypto::write_file_atomically(&settings.cert, cert.pem().as_bytes()) {
        // Undo, so the next start can generate again instead of finding a lone key.
        let _ = fs::remove_file(&settings.key);
        return Err(write_error(&settings.cert, err));
    }
    println!(
        "generated self-signed certificate for {:?} -> {} (key {})",
        settings.self_signed_names,
        settings.cert.display(),
        settings.key.display()
    );
    Ok(Generation::Written)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT_WAIT: Duration = Duration::from_millis(300);

    fn settings(dir: &Path) -> config::Tls {
        config::Tls {
            cert: dir.join("certs/cert.pem"),
            key: dir.join("certs/key.pem"),
            self_signed_names: vec!["localhost".into()],
            min_version: TlsVersion::Tls13,
            hsts_max_age: Duration::ZERO,
        }
    }

    fn install_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn generated_certificates_follow_browser_rules() {
        let names = vec!["albums.local".to_string(), "192.168.1.20".to_string()];
        let params = self_signed_params(&names).unwrap();
        assert!(
            params.not_before < rcgen::date_time_ymd(2019, 7, 1),
            "Apple limits certificates issued after 2019-07-01 to 825 days"
        );
        assert_eq!(
            params.extended_key_usages,
            [ExtendedKeyUsagePurpose::ServerAuth]
        );
        assert!(matches!(
            params.subject_alt_names[1],
            rcgen::SanType::IpAddress(_)
        ));
        let again = self_signed_params(&names).unwrap();
        assert_ne!(
            again.serial_number, params.serial_number,
            "Firefox refuses a reused serial"
        );
    }

    #[test]
    fn generates_only_when_both_files_are_missing() {
        install_provider();
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path());
        load_or_create(&settings).unwrap();
        let cert = fs::read(&settings.cert).unwrap();

        // Loading again reuses the pair instead of regenerating it.
        load_or_create(&settings).unwrap();
        assert_eq!(fs::read(&settings.cert).unwrap(), cert);

        // A missing key is an error at once; the existing certificate is left alone.
        fs::remove_file(&settings.key).unwrap();
        let error = load_or_create(&settings).err().unwrap();
        assert!(error.contains("is missing"), "{error}");
        assert_eq!(fs::read(&settings.cert).unwrap(), cert);
    }

    #[test]
    fn explains_dangling_symlinks_and_bad_pairs() {
        install_provider();
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path());
        fs::create_dir_all(dir.path().join("certs")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("../archive/cert1.pem", &settings.cert).unwrap();
            std::os::unix::fs::symlink("../archive/key1.pem", &settings.key).unwrap();
            let error = load_or_create(&settings).err().unwrap();
            assert!(error.contains("/etc/letsencrypt"), "{error}");
            fs::remove_file(&settings.cert).unwrap();
            fs::remove_file(&settings.key).unwrap();
        }
        fs::write(&settings.cert, "not a certificate").unwrap();
        fs::write(&settings.key, "not a key").unwrap();
        assert!(load_or_create(&settings).is_err());
    }

    #[test]
    fn refuses_a_key_that_does_not_match_the_certificate() {
        install_provider();
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (first, second) = (settings(one.path()), settings(two.path()));
        load_or_create(&first).unwrap();
        load_or_create(&second).unwrap();
        fs::copy(&second.key, &first.key).unwrap();
        let error = load_or_create(&first).err().unwrap();
        assert!(error.contains("don't form a valid pair"), "{error}");
    }

    #[test]
    fn concurrent_first_starts_share_one_pair() {
        install_provider();
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path());
        std::thread::scope(|scope| {
            let starts: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| load_or_create(&settings).map(|_| ())))
                .collect();
            for start in starts {
                start.join().unwrap().unwrap();
            }
        });
        let leftovers: Vec<_> = fs::read_dir(dir.path().join("certs"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers.len(),
            2,
            "only cert.pem and key.pem: {leftovers:?}"
        );
    }

    #[test]
    fn a_key_without_its_certificate_is_an_error_after_waiting() {
        install_provider();
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path());
        load_or_create(&settings).unwrap();
        fs::remove_file(&settings.cert).unwrap();
        let started = std::time::Instant::now();
        let error = load_or_create_waiting(&settings, SHORT_WAIT).err().unwrap();
        assert!(error.contains("interrupted"), "{error}");
        assert!(started.elapsed() >= SHORT_WAIT);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_certificate_write_removes_the_key_again() {
        use std::os::unix::fs::PermissionsExt;
        install_provider();
        let dir = tempfile::tempdir().unwrap();
        let mut settings = settings(dir.path());
        let read_only = dir.path().join("read-only");
        fs::create_dir_all(&read_only).unwrap();
        fs::set_permissions(&read_only, fs::Permissions::from_mode(0o555)).unwrap();
        settings.cert = read_only.join("cert.pem");

        assert!(load_or_create(&settings).is_err());
        assert!(!settings.key.exists(), "the key was rolled back");

        // Retrying is safe: once the folder is writable, the next start generates.
        fs::set_permissions(&read_only, fs::Permissions::from_mode(0o755)).unwrap();
        load_or_create(&settings).unwrap();
    }
}
