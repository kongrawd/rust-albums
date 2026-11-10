# rust-albums (POC)

A small, self-hosted photo and video album viewer. Point it at a folder of photos and it serves them as albums over **HTTPS only**, behind a sign-in. It is a single ~3 MB binary and runs on a Raspberry Pi, a NAS or any Linux, macOS or Docker host (amd64, arm64 and armv7).

- **Albums from folders:** you describe your folder layout with a pattern such as `{year}/{album_name}/{file_name}`, and each `{…}` part becomes metadata.
- **Sign in, sign out:** one account, set in the environment. Signing out really signs you out, and every page and file link checks your session.
- **HTTPS out of the box:** it creates a self-signed certificate on first run, or uses your own PEM files (e.g. from certbot).
- **Built for flaky storage:** USB disks and network shares can drop out; the albums stay visible and recover when the storage comes back.

## Quick start

### Docker (recommended for a Pi or NAS)

```sh
git clone https://github.com/kongrawd/rust-albums && cd rust-albums
cp .env.example .env              # then edit .env: AUTH_USER, AUTH_PASS, PHOTOS_DIR, ALBUMS_TLS_NAMES, ALBUMS_IMAGE
touch /srv/photos/.albums-root    # once, in your PHOTOS_DIR: the marker file
docker compose up -d
```

Open `https://<a name from ALBUMS_TLS_NAMES>:8443` (or your `ALBUMS_PORT`), accept the self-signed certificate and sign in. More in [docs/docker.md](docs/docker.md).

### From source

Needs the Rust version in `Cargo.toml` (`rust-version`) or newer.

```sh
git clone https://github.com/kongrawd/rust-albums && cd rust-albums
AUTH_USER=admin AUTH_PASS=change-me cargo run --release
```

Open <https://localhost:8443>, accept the self-signed certificate and sign in. The `storage/` folder holds a few sample albums, marker file included.

The first run creates `albums.toml` (your settings), `certs/` (the self-signed certificate) and `link.key` (the key behind album and photo URLs) in the current folder. All three are git-ignored. Keep `link.key`: a new key changes every album URL.

To reach it from other devices, set `bind_addr = "0.0.0.0:8443"` in `albums.toml`, add the machine's name or IP to `self_signed_names`, delete `certs/` so the certificate is created again, and restart.

### The marker file

The empty `.albums-root` file tells the server that your photo folder is really there; it won't start without it. See [docs/storage.md](docs/storage.md#the-marker-file).

## Organising your photos

The **pattern** describes the folders below your photo root: the last part is the file, the parts before it make an album. For example, `{year}/{album_name}/{file_name}` shows `2024/Lake/a.jpg` as album "Lake", tagged year 2024. More patterns and the supported formats: [docs/storage.md](docs/storage.md).

## Settings you may want to change

Edit `albums.toml` (created on first run), then restart. Each setting is explained there, with its default.

| Setting | Change it to… |
|---|---|
| `server.bind_addr` | serve your network |
| `storage.root` | point to your photo folder |
| `storage.pattern` | match your folders |
| `storage.rescan_secs` | scan less often on network shares |
| `tls.self_signed_names` | add the name or IP you browse to |
| `session.idle_timeout_secs` | stay signed in longer or shorter |

In Docker, the listen address, photo folder and certificate names come from the image and `.env` instead (see [docs/docker.md](docs/docker.md#settings-in-docker)). The username and password always come from the environment: `AUTH_USER` and `AUTH_PASS`. Environment overrides: [docs/configuration.md](docs/configuration.md).

## Using it

- **Signing in:** browsers use the sign-in page. A link you open while signed out takes you to sign in, then on to that link.
- **Signing out:** the button at the top of every page.
- **Scripts and tools:** `curl -k -u user:pass https://localhost:8443/` works without the sign-in page (`-k` accepts the self-signed certificate).
- **New photos** appear after the next scan, once they have finished copying.

How sign-in, sign-out and links behave: [docs/security.md](docs/security.md).

## Troubleshooting

Common problems, from a missing marker file to videos that don't play: [docs/troubleshooting.md](docs/troubleshooting.md).

## POC limitations

- **Run one copy only:** sign-ins are kept in memory ([details](docs/architecture.md#single-replica)).
- One account, shared by everyone who knows the credentials.
- No thumbnails: originals are loaded lazily as you scroll.
- Renaming or moving a file or album changes its URL, so old links to it stop working.
- Certificates are read at startup ([renewals](docs/configuration.md#certificates)).

## Documentation

- [docs/configuration.md](docs/configuration.md): the config file, environment overrides, certificates
- [docs/storage.md](docs/storage.md): patterns, the marker file, storage outages, album URLs
- [docs/security.md](docs/security.md): HTTPS, signing in and out, direct links, routes
- [docs/docker.md](docs/docker.md): Compose, settings, permissions, upgrading
- [docs/troubleshooting.md](docs/troubleshooting.md): startup errors and common problems
- [docs/architecture.md](docs/architecture.md): terminology, single replica
- [CONTRIBUTING.md](CONTRIBUTING.md): development, pull requests, releases
