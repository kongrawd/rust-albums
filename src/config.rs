//! Settings from the local config file (TOML). Defaults live in
//! `albums.default.toml`, which is embedded in the binary and copied to the
//! config path on first run. Unknown keys are rejected to catch typos. A few
//! `ALBUMS_*` environment variables override the file, for containers.

use crate::pattern::Pattern;
use serde::{Deserialize, Deserializer};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the config file is when `$ALBUMS_CONFIG` isn't set.
const DEFAULT_CONFIG_PATH: &str = "./albums.toml";
/// The contents written to a new config file.
const DEFAULT_CONFIG_TOML: &str = include_str!("../albums.default.toml");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub storage: Storage,
    pub tls: Tls,
    pub session: Session,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub bind_addr: SocketAddr,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub backend: StorageBackend,
    pub root: PathBuf,
    pub pattern: String,
    /// File that must exist in the root, so an unmounted (empty) folder is detected.
    pub marker: String,
    #[serde(rename = "rescan_secs", deserialize_with = "seconds")]
    pub rescan_interval: Duration,
    #[serde(rename = "settle_secs", deserialize_with = "seconds")]
    pub settle_time: Duration,
    /// File and folder names skipped anywhere below the root.
    pub ignore: Vec<String>,
    /// Key for link ids (see `link_ids`); created on first run.
    pub link_key: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    /// A folder on this machine.
    Local,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub self_signed_names: Vec<String>,
    pub min_version: TlsVersion,
    #[serde(rename = "hsts_max_age_secs", deserialize_with = "seconds")]
    pub hsts_max_age: Duration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    #[serde(rename = "idle_timeout_secs", deserialize_with = "seconds")]
    pub idle_timeout: Duration,
    #[serde(rename = "max_lifetime_secs", deserialize_with = "seconds")]
    pub max_lifetime: Duration,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TlsVersion {
    #[serde(rename = "1.2")]
    Tls12,
    #[serde(rename = "1.3")]
    Tls13,
}

/// Reads a whole number of seconds (the `*_secs` keys) as a [`Duration`].
fn seconds<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    u64::deserialize(deserializer).map(Duration::from_secs)
}

/// The config file's path: `$ALBUMS_CONFIG`, or `./albums.toml`.
pub fn path() -> PathBuf {
    std::env::var_os("ALBUMS_CONFIG")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from)
}

impl Config {
    /// Reads the config at `path`, first writing the defaults there if it
    /// doesn't exist, then applies `ALBUMS_*` environment overrides.
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        let describe = |err: &dyn std::fmt::Display| format!("config {}: {err}", path.display());
        if !path.exists() {
            println!("writing default config -> {}", path.display());
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).map_err(|e| describe(&e))?;
            }
            fs::write(path, DEFAULT_CONFIG_TOML).map_err(|e| describe(&e))?;
        }
        let text = fs::read_to_string(path).map_err(|e| describe(&e))?;
        let mut config: Self = toml::from_str(&text).map_err(|e| describe(&e))?;
        config.apply_overrides(|name| std::env::var(name).ok())?;
        config.validate().map_err(|e| describe(&e))?;
        // A config in the working directory needs no rewriting.
        let config_dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty() && *dir != Path::new("."));
        if let Some(dir) = config_dir {
            config.resolve_relative_paths(dir);
        }
        Ok(config)
    }

    /// Container-friendly overrides; `lookup` is `std::env::var` outside tests.
    /// Blank values count as unset.
    fn apply_overrides(&mut self, lookup: impl Fn(&str) -> Option<String>) -> Result<(), String> {
        let set = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        if let Some(addr) = set("ALBUMS_BIND_ADDR") {
            self.server.bind_addr = addr
                .parse()
                .map_err(|e| format!("ALBUMS_BIND_ADDR {addr:?}: {e}"))?;
        }
        if let Some(root) = set("ALBUMS_STORAGE_ROOT") {
            self.storage.root = root.into();
        }
        if let Some(cert) = set("ALBUMS_TLS_CERT") {
            self.tls.cert = cert.into();
        }
        if let Some(key) = set("ALBUMS_TLS_KEY") {
            self.tls.key = key.into();
        }
        if let Some(names) = set("ALBUMS_TLS_NAMES") {
            self.tls.self_signed_names = names
                .split(',')
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect();
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.session.idle_timeout.is_zero() || self.session.max_lifetime.is_zero() {
            return Err(
                "session.idle_timeout_secs and session.max_lifetime_secs must be greater than 0"
                    .into(),
            );
        }
        if self.storage.rescan_interval.is_zero() {
            return Err("storage.rescan_secs must be greater than 0".into());
        }
        let marker = &self.storage.marker;
        if marker.is_empty() || marker.contains(['/', '\\']) {
            return Err("storage.marker must be a plain file name, e.g. \".albums-root\"".into());
        }
        Pattern::parse(&self.storage.pattern).map_err(|e| format!("storage.pattern: {e}"))?;
        if self.tls.self_signed_names.is_empty() {
            return Err("tls.self_signed_names must name at least one host".into());
        }
        Ok(())
    }

    /// Makes relative paths, from the file or from `ALBUMS_*` overrides,
    /// independent of the working directory (e.g. under systemd).
    fn resolve_relative_paths(&mut self, config_dir: &Path) {
        for path in [
            &mut self.storage.root,
            &mut self.storage.link_key,
            &mut self.tls.cert,
            &mut self.tls.key,
        ] {
            if path.is_relative() {
                let relative = path.strip_prefix(".").unwrap_or(path);
                *path = config_dir.join(relative);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn default_config_is_strict_https() {
        let config = parse(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(config.tls.min_version, TlsVersion::Tls13);
        assert!(config.tls.hsts_max_age >= Duration::from_secs(31_536_000));
        assert!(matches!(config.storage.backend, StorageBackend::Local));
        assert_eq!(config.storage.rescan_interval, Duration::from_secs(30));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_unknown_keys_and_backends() {
        let typo = DEFAULT_CONFIG_TOML.replace("min_version", "min_verison");
        assert!(parse(&typo).is_err());
        let remote = DEFAULT_CONFIG_TOML.replace(r#"backend = "local""#, r#"backend = "s3""#);
        assert!(parse(&remote).is_err());
    }

    #[test]
    fn environment_overrides_for_containers() {
        let mut config = parse(DEFAULT_CONFIG_TOML).unwrap();
        let env = |name: &str| match name {
            "ALBUMS_BIND_ADDR" => Some("0.0.0.0:8443".to_string()),
            "ALBUMS_STORAGE_ROOT" => Some("/photos".to_string()),
            "ALBUMS_TLS_CERT" => Some("/certs/fullchain.pem".to_string()),
            "ALBUMS_TLS_NAMES" => Some("pi.local, 192.168.1.20,".to_string()),
            "ALBUMS_TLS_KEY" => Some("  ".to_string()), // blank = not set
            _ => None,
        };
        config.apply_overrides(env).unwrap();
        assert_eq!(config.server.bind_addr.to_string(), "0.0.0.0:8443");
        assert_eq!(config.storage.root, Path::new("/photos"));
        assert_eq!(config.tls.cert, Path::new("/certs/fullchain.pem"));
        assert_eq!(config.tls.key, Path::new("./certs/key.pem"));
        assert_eq!(config.tls.self_signed_names, ["pi.local", "192.168.1.20"]);

        let bad = |name: &str| (name == "ALBUMS_BIND_ADDR").then(|| "localhost".to_string());
        let error = parse(DEFAULT_CONFIG_TOML)
            .unwrap()
            .apply_overrides(bad)
            .unwrap_err();
        assert!(error.contains("ALBUMS_BIND_ADDR"), "{error}");
    }

    #[test]
    fn rejects_invalid_storage_settings() {
        let mut config = parse(DEFAULT_CONFIG_TOML).unwrap();
        config.storage.marker = "../etc/passwd".into();
        assert!(config.validate().is_err());

        let mut config = parse(DEFAULT_CONFIG_TOML).unwrap();
        config.storage.rescan_interval = Duration::ZERO;
        assert!(config.validate().is_err());

        let mut config = parse(DEFAULT_CONFIG_TOML).unwrap();
        config.storage.pattern = "{album_name}/{album_name}".into();
        let error = config.validate().unwrap_err();
        assert!(error.starts_with("storage.pattern:"), "{error}");
    }
}
