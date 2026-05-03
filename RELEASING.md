# Releasing

muAgent is distributed internally through GitHub Releases. The project does not
publish to the npm registry.

## What A Release Contains

The release workflow builds the `muagent` Rust CLI and uploads native packages:

| Platform | Asset |
|---|---|
| macOS Apple Silicon | `muagent-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `muagent-vX.Y.Z-x86_64-apple-darwin.tar.gz` |
| Linux x64 static musl | `muagent-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` |

GitHub automatically attaches source archives for the tag, so source files do
not need to be uploaded manually.

The packaged files include:

- `muagent`
- `README.md`
- `USAGE.md`
- `CONFIG.md`
- `BUILD.md`
- `DEVELOPMENT.md`
- `RELEASING.md`
- `LICENSE-APACHE`
- `LICENSE-MIT`

## Create A Release

Update the version in `Cargo.toml` and `package.json` if needed, then commit the
change.

Create and push a tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The `Release` GitHub Actions workflow runs on the tag and publishes the assets
to the GitHub Release page.

You can also run the workflow manually from GitHub Actions with an existing tag,
for example `v0.1.0`.

## Tests

Release builds intentionally do not run the full test suite. Several live tests
use provider API keys and can be slow or cost money. The release workflow only
builds the native binaries and runs `muagent --version` plus `muagent --help` on
the runner that produced each asset.

Run local checks before tagging when appropriate:

```bash
cargo check --locked --bin muagent
cargo test --locked --test cli_smoke
```

## Internal npm Usage

The npm package remains a local development shim:

```bash
npm install -g .
```

This path builds the Rust binary on the installing machine and requires
Rust/Cargo. Do not publish the package to the npm registry unless the
distribution model changes.
