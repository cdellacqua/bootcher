# Development

## Dependencies

### Rust toolchain

bootcher is pinned to a specific Rust version via `rust-toolchain.toml`. The toolchain is installed automatically by `rustup` on first use; if you don't have `rustup` yet, get it from <https://rustup.rs>.

The pinned channel and required components (`rustfmt`, `clippy`) are declared in [rust-toolchain.toml](rust-toolchain.toml). `rustup` provisions them without any extra steps.

### just

The `just` task runner is used to run the development workflows (`just ci`, `just run`, `just test`, …).

```sh
cargo install just --locked
```

### mdbook

The documentation site under [`docs/`](docs/) is built with [mdBook](https://rust-lang.github.io/mdBook/).
The GitHub Pages landing page (animated hero + links) lives in [`docs/landing/`](docs/landing/);
`just docs` assembles it together with the book into `docs/site/` (landing at the
root, the book under `book/`, rustdoc under `book/api/`) — that's what CI publishes.

```sh
cargo install mdbook --locked
cargo install mdbook-mermaid --locked
```

Build the site locally with:

```sh
just docs        # assemble docs/site/ (landing + book + embedded rustdoc)
just docs-serve  # prose live-reload at http://localhost:3000 (book only, rustdoc not embedded)
```

To preview the landing page, open `docs/site/index.html` in a browser after `just docs`.

### Runtime tools (for `just run`)

Building this repo needs only the Rust toolchain above, but actually *running* the
binary against a project (`just run build` / `disk` / `deploy` …) shells out to
`podman`, `ssh`, `sudo`, and — for the `vm` builder — `qemu`. The full matrix
(which tool each subcommand needs, and the install hints) lives in the user-facing
[Installation › Runtime dependencies](docs/src/installation.md#runtime-dependencies)
page. You don't need to memorise it: each subcommand preflights the tools it needs
and fails up front naming any that are missing (see
[`bootcher_core::preflight`](crates/bootcher-core/src/preflight.rs)).

## Common tasks

```sh
just run <args>   # run a dev build of the binary
just test         # cargo test
just ci           # fmt check + clippy + doc + test (what CI runs)
just fix          # auto-fix fmt and clippy warnings
just install      # cargo install --path crates/bootcher-cli
```

## Cutting a release

Releases are tagged, not published to crates.io. Pushing a `vX.Y.Z` tag triggers
the `release` (binaries) and `image` (ghcr.io) jobs in
[release.yml](.github/workflows/release.yml). That workflow runs on tags only and
does not re-run the test suite: the tagged commit already passed
[ci.yml](.github/workflows/ci.yml) when it landed on `main`, so only tag commits
that are green on `main`.

`just release` does the whole dance from a clean `main` — bump the workspace
version, commit, tag, push:

```sh
just release         # patch bump (default): 0.0.1 -> 0.0.2
just release minor   # 0.0.1 -> 0.1.0
just release major   # 0.0.1 -> 1.0.0
just release 1.2.3   # set an explicit version
```

The version bump is done by the dev-only [`housekeeper`](crates/housekeeper/)
binary (a [cargo-xtask](https://github.com/matklad/cargo-xtask)-style helper) so
the workflow needs no external tooling like `cargo-edit`/`cargo-release` — it
edits `[workspace.package].version` in [Cargo.toml](Cargo.toml) in place with
`toml_edit` (preserving comments) and `semver`, and prints the new version for
the recipe to tag. Both member crates inherit the version via
`version.workspace = true`, so that one field is the single source of truth.

## End-to-end tests

The e2e tests boot real bootc disk images and are opt-in: plain `cargo test`
skips them (`#[ignore]` + feature gates), and they **assert** their
prerequisites rather than skipping — a missing tool fails the run with the
reason, so only enable them on a runner that can actually host a VM. They come
in two gated suites.

### Same-arch VM + registry (`just e2e`)

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
just e2e
# or, combined with the standard CI checks:
just ci e2e=1
```

### Cross-arch builder (`just e2e-cross`)

Builds a **foreign-arch** bootc disk (image-builder inside a Fedora Cloud builder VM) and
boots it — both under full CPU emulation (qemu TCG, no KVM). Gated apart behind
the `e2e_cross` feature because its prerequisites differ:

- **foreign `qemu-system-<arch>`** + **foreign UEFI firmware** — to build and boot the foreign-arch guest
- **qemu-user binfmt_misc handler** (e.g. `qemu-user-static`) — the cross-arch `podman build --platform` needs it
- **podman, qemu-img, ssh, ssh-keygen** — and network for the builder's one-time cloud-image + base-image pulls
- **No** `/dev/kvm` or host `sudo` needed — everything foreign is emulated, and the privileged image-builder step runs inside the builder VM, not in-process

```sh
just e2e-cross
# or, combined with the standard CI checks:
just ci e2e_cross=1
```

> **Cross-arch is slow.** Because everything runs under TCG emulation rather than
> KVM, the cross-arch run can take **~2 hours or more** depending on hardware —
> far longer than the KVM-accelerated `just e2e` suite.

## Project layout

- [`crates/bootcher-core/`](crates/bootcher-core/) — library: builders, jobs, pipelines, LAN registry, lifecycle hooks
- [`crates/bootcher-core/scaffold/`](crates/bootcher-core/scaffold/) — embedded project template stamped out by `bootcher init`
- [`crates/bootcher-cli/`](crates/bootcher-cli/) — the `bootcher` binary
- [`crates/housekeeper/`](crates/housekeeper/) — dev-only release helper (version bumps for `just release`); never published
