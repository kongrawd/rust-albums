# Configuration

## The config file

Settings live in `./albums.toml`, or the path in `$ALBUMS_CONFIG`. The first run creates it from [`albums.default.toml`](../albums.default.toml), which explains every setting and its default; an existing file is never rewritten. Restart after editing.

- Unknown keys are rejected, so typos don't go unnoticed.
- Relative paths are resolved against the config file's folder, not the working directory.
- Secrets never go in the file. Credentials come from the environment only: `AUTH_USER` (must not contain `:`) and `AUTH_PASS`.

## Environment overrides

Mainly for containers. Empty values are ignored, and relative paths are resolved like those in the file.

| Variable | Overrides |
|---|---|
| `ALBUMS_CONFIG` | Path of the config file itself |
| `ALBUMS_BIND_ADDR` | `server.bind_addr` |
| `ALBUMS_STORAGE_ROOT` | `storage.root` |
| `ALBUMS_TLS_CERT`, `ALBUMS_TLS_KEY` | `tls.cert`, `tls.key` |
| `ALBUMS_TLS_NAMES` | `tls.self_signed_names` (comma-separated) |

`TOKIO_WORKER_THREADS=1` limits the server to one worker thread on boards with few cores.

## Certificates

- **Your own certificate** (PEM files, e.g. from certbot or a Kubernetes secret): point `tls.cert` and `tls.key` (or `ALBUMS_TLS_CERT` and `ALBUMS_TLS_KEY`) at them. They can be read-only and are never written.
- **Self-signed:** generated only when *both* files are missing, and never replaces an existing file. If just one of the two exists, startup fails with a message saying which.
- **Renewals:** certificates are read at startup, so restart the server (or container) after a renewal, e.g. from certbot's `--deploy-hook`.
- **certbot:** mount the whole `/etc/letsencrypt` folder: `live/` holds symlinks into `archive/`.
- **Permissions:** the server must be able to read the key; in Docker, see [docker.md](docker.md#permissions).
- **New names for the self-signed certificate:** it is generated once, so after changing `self_signed_names` (in Docker, `ALBUMS_TLS_NAMES`), delete the certificate and key and restart. In Docker, see [docker.md](docker.md#changing-the-state-volume).
