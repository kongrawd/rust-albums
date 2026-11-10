# Docker

The image runs as a non-root user (uid 65532) on distroless. Photos are mounted read-only at `/photos`, and everything the server writes (the config, the self-signed certificate and the link key) goes to `/state`.

## Compose

Start with the [quick start](../README.md#docker-recommended-for-a-pi-or-nas) in the README. In `.env`, `ALBUMS_IMAGE` picks the published image; which tag to use is in [CONTRIBUTING.md](../CONTRIBUTING.md#releases).

[`compose.yaml`](../compose.yaml):

- mounts the photos so that a missing folder fails the start instead of being created empty;
- runs with a read-only root filesystem, no capabilities and `no-new-privileges`;
- shows how to mount certbot certificates.

With plain `docker run`, use `--mount type=bind,src=…,dst=/photos,readonly`, which fails on a missing source. The short `-v` syntax silently creates it.

## Settings in Docker

The image sets the listen address and the photo folder, and Compose sets the certificate names from `ALBUMS_TLS_NAMES` in `.env`; these override `albums.toml`. Put the name or IP you browse to in `ALBUMS_TLS_NAMES`.

After changing `.env`, run `docker compose up -d`: it recreates the container with the new values (`docker compose restart` keeps the old ones).

## Changing the state volume

The image has no shell, so use a throwaway container on the `state` volume (Compose names it `rust-albums_state`):

```sh
# Edit the config, then restart:
docker run --rm -it -u 65532 -v rust-albums_state:/state busybox vi /state/albums.toml
docker compose restart

# Regenerate the self-signed certificate after changing ALBUMS_TLS_NAMES:
docker run --rm -u 65532 -v rust-albums_state:/state busybox rm -r /state/certs
docker compose up -d --force-recreate
```

## Permissions

Photos must be readable by uid 65532. World-readable files (the usual 644) just work; otherwise add a group that can read them with `group_add` in `compose.yaml`. A mounted certificate key must be readable by the same user.

On SELinux hosts (Fedora, RHEL), add `selinux: z` under `bind:` for the photo mount in `compose.yaml`, or relabel the folder once with `chcon -R -t container_file_t <folder>`.

## Network shares and Docker Desktop

Directory walks are much slower there, so raise `storage.rescan_secs` in the state volume's `albums.toml`.

## Health

The image's `HEALTHCHECK` runs `rust-albums --healthcheck`, which asks `/healthz` over loopback TLS. Plain Docker and Compose only report `unhealthy` and don't restart on it; `restart: unless-stopped` covers crashes.

## Upgrading

Set `ALBUMS_IMAGE` in `.env` to how far you want to follow releases: `:latest` for every release, a minor tag (`:0.1`) for its patch releases, or an exact version (`:0.1.0`) to choose each upgrade; from v1, a major tag (`:1`) follows all 1.x releases. Back up the state volume first, since `link.key` keeps album URLs:

```sh
docker run --rm --mount type=volume,source=rust-albums_state,target=/state,readonly \
  --mount type=bind,source="$PWD",target=/backup busybox tar czf /backup/albums-state.tar.gz -C /state .
docker compose pull && docker compose up -d
```

The state volume carries over, so album URLs stay the same; restarting signs everyone out. To roll back, set the previous version in `ALBUMS_IMAGE` and run `docker compose up -d` again.

## Building locally

```sh
docker buildx create --use     # once: multi-platform builds need a docker-container builder
docker buildx build --platform linux/arm64 --load -t rust-albums .      # one platform, into local Docker
docker buildx build --platform linux/amd64,linux/arm64,linux/arm/v7 -t <registry>/rust-albums --push .
```

To run a local build, set `ALBUMS_IMAGE=rust-albums` in `.env`.
