# Troubleshooting

Start with the server's log (`docker compose logs albums` in Docker): startup errors name the file or setting involved, and failed scans log their reason. `/healthz` answers `ok` while storage is available, or 503 with a generic reason.

## Startup

| Problem | Fix |
|---|---|
| "…/.albums-root not found: is the storage mounted?" | The [marker file](storage.md#the-marker-file) is missing: create `.albums-root` in the photo root. In Docker, check that `PHOTOS_DIR` is the folder you think it is. |
| "storage root …: No such file or directory" | `storage.root` (or `ALBUMS_STORAGE_ROOT`) points to a folder that doesn't exist. |
| Compose: "bind source path does not exist" | `PHOTOS_DIR` in `.env` points to a folder that doesn't exist. |
| Compose: "required variable … is missing a value" | Set that variable (`AUTH_USER`, `AUTH_PASS`, `PHOTOS_DIR` or `ALBUMS_IMAGE`) in `.env`. |
| "manifest unknown" or "denied" when pulling | The tag doesn't exist yet or the package is still private; see [CONTRIBUTING.md](../CONTRIBUTING.md#releases). |
| From source: "AUTH_USER must be set" | Set `AUTH_USER` and `AUTH_PASS` in the environment. |
| A TLS error naming the certificate or key | The message says which file is missing or unreadable; see [configuration.md](configuration.md#certificates). |
| "permission denied" in Docker | The container must be able to read the photos and any mounted key; see [docker.md](docker.md#permissions). |
| "config …: unknown field" | A typo in `albums.toml`; the message names the key. |

## While running

| Problem | Fix |
|---|---|
| Pages say "Storage is unavailable" | The disk or share dropped out (`/healthz` names the reason). The last albums stay visible, and it recovers on its own once the storage is back. |
| New photos don't show | Wait for the next scan, after the files have stopped changing (`rescan_secs`, `settle_secs`). Files must match the pattern; a scan that finds no albums logs a hint. |
| A video doesn't play | The browser can't decode its codec. HEVC (H.265) plays in Safari but only on some Chrome and Edge setups; H.264 or VP9 play almost everywhere. |
| Certificate warning, or the wrong name | Expected with the self-signed certificate. For a new name, see [configuration.md](configuration.md#certificates). |
| Other devices can't connect | Use `bind_addr = "0.0.0.0:8443"` (the Docker image already does) and open the port in the firewall. |
| `curl` fails with a certificate error | The certificate is self-signed: use `curl -k`, or `--cacert certs/cert.pem`. |
| Signed out unexpectedly | Sessions end after the idle timeout or the maximum lifetime (`[session]`), and on every restart. |
| "Too many attempts" (429) | Failed sign-ins slow down that client. Wait a moment and try again. |
| Docker shows `unhealthy` | `/healthz` is failing, usually because storage is unavailable. Docker only reports it; check the log. |
