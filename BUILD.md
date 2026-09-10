# Building claude-code-proxy

## Prerequisites

- A stable Rust toolchain. The crate uses `edition = "2024"`, so Rust 1.85 or
  newer is required. Builds are verified with cargo 1.93.1.
- macOS or Linux. Windows binaries are produced by the release workflow below.

## Release build

From the repository root:

```sh
cargo build --release --locked
./target/release/claude-code-proxy --version
```

The binary is `target/release/claude-code-proxy`. `Cargo.toml` defines no
`[profile.release]`, so cargo's default release profile applies.

Run it directly:

```sh
./target/release/claude-code-proxy serve
```

## Install the release binary

```sh
cargo install --path . --locked
claude-code-proxy --version
```

This installs `~/.cargo/bin/claude-code-proxy`. `just install` runs the same
command with `--offline`, which requires the dependency sources to be in the
cargo cache already (a prior build does that). `just install-dev` builds the
debug binary and symlinks it into `~/.cargo/bin` instead.

## Build for a specific target

```sh
rustup target add aarch64-apple-darwin
cargo build --release --locked --target aarch64-apple-darwin
```

The artifact is `target/<triple>/release/claude-code-proxy`. The release
workflow builds these targets:

| Platform      | Target triple                 |
| ------------- | ----------------------------- |
| darwin-arm64  | `aarch64-apple-darwin`        |
| darwin-amd64  | `x86_64-apple-darwin`         |
| linux-amd64   | `x86_64-unknown-linux-gnu`    |
| linux-arm64   | `aarch64-unknown-linux-gnu`   |
| windows-amd64 | `x86_64-pc-windows-msvc`      |
| windows-arm64 | `aarch64-pc-windows-msvc`     |

## Checks before a release

```sh
cargo fmt --check
cargo clippy --all-targets
cargo test -- --test-threads=1
```

Run the test suite single-threaded.

## GitHub Releases

`.github/workflows/release.yml` runs when a `v*` tag is pushed. It runs
`cargo test --locked`, builds `cargo build --release --locked --target <triple>`
for the six targets above, publishes a GitHub Release with
`softprops/action-gh-release`, and pushes a Homebrew formula.

Running that workflow in this fork needs two changes to its inputs:

- A `RELEASE_TOKEN` repository secret with `contents: write`. The release job
  authenticates with `secrets.RELEASE_TOKEN`, not the default `GITHUB_TOKEN`.
- The `update-tap` job pushes to `raine/homebrew-claude-code-proxy`. Remove the
  job or point it at a tap you own.

`just release` uses cargo-release with `--skip-publish` (the crate has
`publish = false`) to bump the patch version and create the tag that triggers
the workflow.

`scripts/install.sh` and the `brew install raine/claude-code-proxy/...` line in
README.md download the upstream `raine/claude-code-proxy` release binaries, not
a build of this fork.
