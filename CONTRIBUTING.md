# Contributing

## Development

Set up as in the README's [from source](README.md#from-source) steps; the sample `storage/` folder already has its marker file. Before pushing, run what CI runs:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Keep to the vocabulary in [docs/architecture.md](docs/architecture.md#terminology). Put implementation details in the code's doc comments, and update the user docs in the same pull request as the behaviour they describe.

## Pull requests

1. Branch from `main` and keep each pull request to one change.
2. Write commit messages as [Conventional Commits](https://www.conventionalcommits.org): `feat(web): …`, `fix(library): …`, `docs: …`, `build(docker): …`, `ci: …`.
3. Open the pull request against `main`. [CI](.github/workflows/ci.yml) and the [image build](.github/workflows/image.yml) must pass.
4. Merge once it is reviewed and green.

[Dependabot](.github/dependabot.yml) opens weekly update pull requests, which go through the same checks.

## Releases

| Event | Images pushed to `ghcr.io/kongrawd/rust-albums` |
|---|---|
| merge to `main` | `edge`, `sha-<commit>` |
| tag `v1.2.3` | `1.2.3`, `1.2`, `1`, `latest`, `sha-<commit>` |
| tag `v0.4.1` | `0.4.1`, `0.4`, `latest`, `sha-<commit>` (no major tag for `v0.x`) |
| tag `v1.3.0-rc.1` | `1.3.0-rc.1`, `sha-<commit>` (never `latest`) |

`v0.1.0` is the first release: until it is tagged, only `edge` exists, so use `ALBUMS_IMAGE=…:edge`.

The cycle:

1. Changes merge to `main` and publish `edge`. Try `edge` on a test machine.

   After the very first publish, make the package public once (GitHub → your profile → Packages → `rust-albums` → Package settings → Change visibility), or pulls need `docker login ghcr.io`.
2. When `main` is ready, tag it with a [semantic version](https://semver.org) and push the tag:

   ```sh
   git switch main && git pull
   git tag -a v0.1.0 -m "v0.1.0"
   git push origin v0.1.0
   ```

   For a release candidate, tag e.g. `v0.2.0-rc.1` first.
3. The tag build pushes the version tags, with an attestation you can verify:

   ```sh
   gh attestation verify oci://ghcr.io/kongrawd/rust-albums:0.1.0 -R kongrawd/rust-albums
   ```

Deploying and rolling back a release: [docs/docker.md](docs/docker.md#upgrading).
