# Development

## Before you commit (required)

**Run `cargo xtask ci` locally and make sure it's green before every commit.**
This is a mandatory pre-commit step, not a suggestion — the same checks run in
[ci.yml](.github/workflows/ci.yml), and a red `cargo xtask ci` locally means a
red pipeline. Skipping it is how broken commits reach `main`.

```sh
cargo xtask ci    # fmt check + clippy + doc + test — must pass before you commit
cargo xtask fix   # auto-fix fmt and clippy warnings, then re-run `cargo xtask ci`
```

## Dependencies

### Rust toolchain

bootcher is pinned to a specific Rust version via `rust-toolchain.toml`. The toolchain is installed automatically by `rustup` on first use; if you don't have `rustup` yet, get it from <https://rustup.rs>.

The pinned channel and required components (`rustfmt`, `clippy`) are declared in [rust-toolchain.toml](rust-toolchain.toml). `rustup` provisions them without any extra steps.

### Task runner

There isn't one to install. The development workflows live in
[`crates/xtask/`](crates/xtask/), a dev-only workspace member reached through the
`cargo xtask` alias in [.cargo/config.toml](.cargo/config.toml) — so `cargo xtask
ci`, `cargo xtask run`, `cargo xtask test`, … work with nothing beyond the
toolchain. `cargo xtask --help` lists every task.

### mdbook

The documentation site under [`docs/`](docs/) is built with [mdBook](https://rust-lang.github.io/mdBook/).
The GitHub Pages landing page (animated hero + links) lives in [`docs/landing/`](docs/landing/);
`cargo xtask docs` assembles it together with the book into `docs/site/` (landing at the
root, the book under `book/`, rustdoc under `book/api/`) — that's what CI publishes.

mdBook is the one dev tool that still needs installing — it does something no
cargo task can:

```sh
cargo install mdbook --locked
cargo install mdbook-mermaid --locked
```

Build the site locally with:

```sh
cargo xtask docs        # assemble docs/site/ (landing + book + embedded rustdoc)
cargo xtask site-serve  # build the site and serve it at http://localhost:3000
cargo xtask docs-watch  # prose live-reload at http://localhost:3000 (book only, rustdoc not embedded)
```

To preview the landing page, open `docs/site/index.html` in a browser after `cargo xtask docs`.

### Runtime tools (for `cargo xtask run`)

Building this repo needs only the Rust toolchain above, but actually *running* the
binary against a project (`cargo xtask run build` / `disk` / `deploy` …) shells out to
`podman`, `ssh`, `sudo`, and — for the `vm` builder — `qemu`. The full matrix
(which tool each subcommand needs, and the install hints) lives in the user-facing
[Installation › Runtime dependencies](docs/src/installation.md#runtime-dependencies)
page. You don't need to memorise it: each subcommand preflights the tools it needs
and fails up front naming any that are missing (see
[`bootcher_core::preflight`](crates/bootcher-core/src/preflight.rs)).

## Common tasks

```sh
cargo xtask run <args>   # run a dev build of the binary (in the current directory)
cargo xtask test         # cargo test
cargo xtask ci           # fmt check + clippy + doc + test (what CI runs)
cargo xtask fix          # auto-fix fmt and clippy warnings
cargo xtask install      # cargo install --path crates/bootcher-cli
cargo xtask --help       # the full list
```

`cargo xtask run` is the one task that keeps the directory you invoked it from
rather than the repo root, since bootcher reads `./bootcher.toml`. The alias
itself is found by walking up from the working directory, so it only reaches
projects nested under this repo — for a project elsewhere on disk, `cargo xtask
install` and use the real `bootcher` binary.

Every task that shells out to cargo splices in `$BOOTCHER_CARGO_FLAGS`
(whitespace-separated). It's empty for local dev; CI sets it to `--frozen` to pin
`Cargo.lock` for the nested invocations, alongside `CARGO_NET_OFFLINE=true` for
the outer build of xtask itself.

## Cutting a release

Releases are tagged, not published to crates.io. Pushing a `vX.Y.Z` tag triggers
the `release` (binaries) and `image` (ghcr.io) jobs in
[release.yml](.github/workflows/release.yml). That workflow runs on tags only and
does not re-run the test suite: the tagged commit already passed
[ci.yml](.github/workflows/ci.yml) when it landed on `main`, so only tag commits
that are green on `main`.

`cargo xtask release` does the whole dance from a clean `main` — bump the
workspace version, commit, tag, push:

```sh
cargo xtask release         # patch bump (default): 0.0.1 -> 0.0.2
cargo xtask release minor   # 0.0.1 -> 0.1.0
cargo xtask release major   # 0.0.1 -> 1.0.0
cargo xtask release 1.2.3   # set an explicit version
```

The version bump lives in [`crates/xtask/src/release.rs`](crates/xtask/src/release.rs),
so the workflow needs no external tooling like `cargo-edit`/`cargo-release` — it
edits `[workspace.package].version` in [Cargo.toml](Cargo.toml) in place with
`toml_edit` (preserving comments) and `semver`. Both member crates inherit the
version via `version.workspace = true`, so that one field is the single source of
truth.

Every guard — clean tree, on `main`, the version parses, the tag doesn't already
exist — runs *before* the manifest is touched, so a release rejected by one of
them leaves the tree exactly as it found it. Past that point the steps are
sequential and are not rolled back: if `git push` fails, the bump is already
committed and the tag already created locally, and a re-run stops at `tag vX.Y.Z
already exists` — finish that release with `git push origin main vX.Y.Z` rather
than starting a new one.

## End-to-end tests

The e2e tests boot real bootc disk images and are opt-in: plain `cargo test`
skips them (`#[ignore]` + feature gates), and they **assert** their
prerequisites rather than skipping — a missing tool fails the run with the
reason, so only enable them on a runner that can actually host a VM. They come
in two gated suites.

### Same-arch VM + registry (`cargo xtask e2e`)

Builds and boots disks for the **host** architecture, KVM-accelerated. Covers
the LAN rotate + upgrade lifecycle, the registry lifecycle + pull-token rotation
+ signing enrollment, the LAN→registry origin switch, and registry-mode image
signing. Prerequisites:

- **podman** — to build and push container images
- **qemu-system-\*** and **qemu-img** — to run bootc disk images in a VM (`qemu-system-x86_64` or `qemu-system-aarch64` depending on host arch)
- **KVM** (`/dev/kvm`) — hardware virtualisation
- **UEFI firmware** — OVMF (x86-64) or edk2-aarch64 (aarch64); the test locates the firmware automatically from common distro paths
- **ssh, ssh-agent, ssh-keygen** — for key generation and VM communication
- **passwordless `sudo sh`** — the image-builder step runs under one `sudo sh` root session, and there is no TTY for a password prompt while it runs

```sh
cargo xtask e2e
# or, combined with the standard CI checks:
cargo xtask ci --e2e
```

### Cross-arch builder (`cargo xtask e2e-cross`)

Builds a **foreign-arch** bootc disk (image-builder inside a Fedora Cloud builder VM) and
boots it — both under full CPU emulation (qemu TCG, no KVM). Gated apart behind
the `e2e_cross` feature because its prerequisites differ:

- **foreign `qemu-system-<arch>`** + **foreign UEFI firmware** — to build and boot the foreign-arch guest
- **qemu-user binfmt_misc handler** (e.g. `qemu-user-static`) — the cross-arch `podman build --platform` needs it
- **podman, qemu-img, ssh, ssh-keygen** — and network for the builder's one-time cloud-image + base-image pulls
- **No** `/dev/kvm` or host `sudo` needed — everything foreign is emulated, and the privileged image-builder step runs inside the builder VM, not in-process

```sh
cargo xtask e2e-cross
# or, combined with the standard CI checks:
cargo xtask ci --e2e-cross
```

> **Cross-arch is slow.** Because everything runs under TCG emulation rather than
> KVM, the cross-arch run can take **~2 hours or more** depending on hardware —
> far longer than the KVM-accelerated `cargo xtask e2e` suite.

## Project layout

- [`crates/bootcher-core/`](crates/bootcher-core/) — library: builders, jobs, pipelines, LAN registry, lifecycle hooks
- [`crates/bootcher-core/scaffold/`](crates/bootcher-core/scaffold/) — embedded project template stamped out by `bootcher init`
- [`crates/bootcher-cli/`](crates/bootcher-cli/) — the `bootcher` binary
- [`crates/xtask/`](crates/xtask/) — dev-only task runner ([cargo-xtask](https://github.com/matklad/cargo-xtask) pattern) behind the `cargo xtask` alias; never published
