# Architecture

How the code is organised, where each decision is recorded, and the one deployment limit to know about. Implementation details (timings, algorithms, library choices) live in the code's doc comments: each module in `src/` starts with a summary, and `cargo doc --open` shows them all.

## Terminology

These terms mean the same thing in the code and the docs.

| Term | Meaning |
|---|---|
| **root** | The local storage folder (`[storage] root`). |
| **marker** | A file (`.albums-root`) that must exist in the root, so an unmounted, empty folder is detected. |
| **pattern** | The folder layout below the root, e.g. `year={year}/{album_name}/{file_name}`. |
| **segment** | One `/`-separated part of a path or pattern. |
| **placeholder** | A `{name}` in a pattern segment. The value it captures becomes **metadata**. |
| **album** | A folder matching the pattern's folder segments. Its **name** comes from `{album_name}`, falling back to the folder name. |
| **item** | A media file in an album, matching the pattern's last segment. |
| **path** | Always `/`-separated and relative to the root (album path, item path). |
| **link id** | The fixed-length id used in URLs instead of an album or item path. |
| **page title** | A fixed string per page type. |
| **scan** | A background walk of the root that refreshes the albums. |
| **snapshot** | The albums and link ids of the last successful scan, plus the state of the latest scan. Requests read it. |
| **session** | A browser sign-in, held in server memory and identified by a cookie token. |
| **direct link** | A URL to a page or item opened directly (typed, bookmarked, pasted). |

## Where things are decided

| What | Owner |
|---|---|
| Every setting and its default | [`albums.default.toml`](../albums.default.toml) |
| Dependencies and why they were chosen | [`Cargo.toml`](../Cargo.toml) comments and the module that uses them |
| Timings, limits and retries | Constants at the top of each module, with doc comments |
| Image build and base images | [`Dockerfile`](../Dockerfile) |
| CI, image publishing, release tags | [`.github/workflows/`](../.github/workflows), tag rules in [CONTRIBUTING.md](../CONTRIBUTING.md#releases) |
| Dependency updates | [`.github/dependabot.yml`](../.github/dependabot.yml) |

## Single replica

Sessions live in the memory of one process, so a restart signs everyone out, and several replicas behind a load balancer don't work: a session from one replica is unknown to the others, signing out on one leaves copied cookies valid on another, and the sign-in throttle and the scans are per replica. Sticky routing only partly helps, because failover, restarts and rolling deploys still sign users out, and TLS ends in the app, so a layer-4 balancer can only route by IP. Real multi-replica support would need a shared session store (e.g. SQLite on a local volume, or Redis) so that signing out takes effect everywhere at once.
