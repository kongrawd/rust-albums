# Storage

Albums come from a folder on this machine, the **root**. The server scans it in the background and shows what the latest scan found. Terms are defined in [architecture.md](architecture.md#terminology).

## Patterns

The pattern describes the folders below the root. Each `/`-separated **segment** can mix literal text and `{placeholders}`:

- The last segment is the item (the file). The segments before it identify the album.
- Placeholders in folder segments become album metadata. Placeholders in the file segment become item metadata.
- `{album_name}` and `{file_name}` are shown as the album's name and the file's name, not as extra metadata. Without `{album_name}`, the album's name is its folder name.
- Paths that don't match the pattern, hidden files (`.foo`), names in `storage.ignore`, symlinks and non-media files are skipped.

| Pattern | Example path | Metadata |
|---|---|---|
| `{album_name}/{file_name}` (default) | `Summer Trip/beach.jpg` | album_name |
| `{year}/{album_name}/{file_name}` | `2024/Lake/a.jpg` | year, album_name |
| `year={year}/{date}_{album_name}/{seq}_{file_name}` | `year=2024/2024-07-01_Lake/001_dock.jpg` | year, date, album_name; item: seq, file_name |

Supported formats: jpg, jpeg, png, gif, webp, avif, bmp and svg images; mp4, webm, mov and m4v videos. Videos are served as they are, so the browser must support their codec (see [troubleshooting](troubleshooting.md#while-running)).

## The marker file

Put an empty `.albums-root` file (`storage.marker`) in the root once. It is how the server tells "no photos" apart from "the disk isn't mounted": without it, the server refuses to start, and if it disappears while running, storage is reported as unavailable. That catches the classic mistakes: `docker -v` creating a missing host folder empty, or an unplugged disk leaving its empty mountpoint behind.

## When storage drops out

The library is designed for storage that can disappear: USB disks on a Pi, NAS shares, Docker bind mounts.

- **At startup** the server retries briefly, for a mount that appears a moment late, then fails with the reason.
- **While running,** if the disk or share drops out, the albums from the last successful scan stay visible. Pages show a notice, `/healthz` reports it, and everything recovers by itself once the storage is back.
- **A stuck disk never hangs the pages:** a scan or file read that takes too long is reported instead, and stopping the server doesn't wait for it.
- **New files** appear with the next scan, once they have stopped changing for `settle_secs`, so copies in progress aren't shown half-written.
- **Skipped files** (symlinks, unreadable files or names, files still being copied) are counted in the log.

## Album and photo URLs

Album and item URLs use short ids made from the item's path and `link.key`, so they need no escaping and stay the same across restarts and setting changes. Renaming or moving a file or album, or a new `link.key`, changes them. Back up `link.key` with your config.
