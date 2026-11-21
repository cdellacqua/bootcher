# bootcher

[![CI](https://github.com/cdellacqua/bootcher/actions/workflows/ci.yml/badge.svg)](https://github.com/cdellacqua/bootcher/actions/workflows/ci.yml)
[![Audit](https://github.com/cdellacqua/bootcher/actions/workflows/audit.yml/badge.svg)](https://github.com/cdellacqua/bootcher/actions/workflows/audit.yml)
[![Release](https://github.com/cdellacqua/bootcher/actions/workflows/release.yml/badge.svg)](https://github.com/cdellacqua/bootcher/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/cdellacqua/bootcher?sort=semver)](https://github.com/cdellacqua/bootcher/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

An opinionated tool for crafting [bootc](https://bootc-dev.github.io/bootc/) images.

`bootcher` scaffolds a self-contained **project** — one bootc image that starts
`FROM quay.io/fedora/fedora-bootc:<N>` (pinned to the Fedora release current at
`init`) with a security baseline (admin user,
SSH, sudo, firewalld + fail2ban + chrony, auto-updates) layered on top — then
drives the lifecycle: build the container (`podman build`), turn it into a
bootable disk image with
[image-builder](https://github.com/osbuild/image-builder), push updates straight to
devices over SSH, or publish named releases to a registry and move the channels your
fleet follows — base-image patches included.

The tool is image-agnostic: it carries the scaffold as an embedded template and
otherwise knows nothing about what you build on top. Your project is yours to
edit.

## Documentation

**[cdellacqua.github.io/bootcher](https://cdellacqua.github.io/bootcher/)** — the
full documentation: concepts, workflows, recipes, and the complete
[`bootcher.toml` reference](https://cdellacqua.github.io/bootcher/book/reference/bootcher-toml.html),
plus [rustdoc](https://cdellacqua.github.io/bootcher/book/api/bootcher_core/) for
`bootcher-core`. The rest of this README is a summary.

## Runtime dependencies

bootcher is **Linux-only** (Podman is a native Linux tool; use the container image on macOS/Windows — see [Install](#install)).

| Dependency | Required for | Notes |
|---|---|---|
| **podman** | All operations | Builds the container image, runs image-builder (`device provision`), serves images to direct devices and pushes releases. Version ≥ 5 recommended. |
| **skopeo** | `release …`, `channel …`, `signing …`, `device … --channel` | Reads and copies registry images. Not needed for a direct-only project. |
| **git** | `release publish`, the refreshes | Records the commit a release is built from; checks out a release's commit for a refresh. |
| **ssh / ssh-keygen** | every `device …` command | The SSH tunnel and remote commands. Usually provided by `openssh-clients`. |
| **sudo** | `device provision` | image-builder must run as root; bootcher invokes it via `sudo`. Passwordless `sudo sh` is required for unattended runs. |
| **qemu** (`qemu-system-<arch>`, `qemu-img`) + UEFI firmware | `[builder] = "vm"` | Only when a build step is routed to a throwaway local VM (e.g. for cross-arch builds). |
| **qemu-user** (binfmt_misc handler) | Cross-arch in-process builds | Only when building a foreign-arch image with the default `local` builder. |

image-builder itself is pulled as a pinned container image by Podman — you do not need to install it separately.

Each subcommand checks the tools it actually needs **before doing any work** and fails immediately, naming any that are missing and how to install them — so a missing tool never surfaces as a bare spawn error partway through a long build.

## Install

### Binaries
Prebuilt release binaries are published to [github.com/cdellacqua/bootcher/releases](https://github.com/cdellacqua/bootcher/releases). They're built on Ubuntu 24.04, so they should run anything with glibc >= 2.39.

### From source
If you have Rust and Cargo already installed in your system, you can use `cargo install` from a local checkout or directly pointing it at this git repository.

```sh
# from git
cargo install --git https://github.com/cdellacqua/bootcher bootcher-cli
# from a local checkout
cargo install --path crates/bootcher-cli
```

### Container image

Since the native binary is Linux-only, a multi-arch image is available at
[ghcr.io/cdellacqua/bootcher](https://ghcr.io/cdellacqua/bootcher) (tags: `vX.Y.Z` and `latest`). It runs privileged with the host's container storage
mounted in, which lets it work from the Podman/Docker machine VM on macOS and
Windows. Run it with `sudo` from inside your project directory. The process
inside is root and writes every image it builds into the mounted store, so that
store is the rootful one at `/var/lib/containers/storage`: `sudo podman` owns it,

```sh
sudo podman run --rm -it --privileged \
    --security-opt label=type:unconfined_t \
    -v "$PWD:/project" -w /project \
    -v /var/lib/containers/storage:/var/lib/containers/storage \
    ghcr.io/cdellacqua/bootcher:latest <command>
```

The image includes QEMU and UEFI firmware, so the `vm` builder (`[builder] = "vm"` in `bootcher.toml`) works inside the container: `--privileged` exposes `/dev/kvm` for same-arch guests, and the cross-arch builder uses TCG (pure userspace emulation) which needs no host devices at all. If you use the `vm` builder, add a cache mount so the Fedora Cloud base image is not re-downloaded on every run. Put that cache on a root-owned path too (`/var/cache/bootcher`), so it doesn't land in your home directory owned by root:

```sh
sudo podman run --rm -it --privileged \
    --security-opt label=type:unconfined_t \
    -v "$PWD:/project" -w /project \
    -v /var/lib/containers/storage:/var/lib/containers/storage \
    -v /var/cache/bootcher:/root/.cache/bootcher \
    ghcr.io/cdellacqua/bootcher:latest <command>
```

## Quickstart

Direct only — no registry, nothing hosted:

```sh
bootcher init my-os                  # "publish to a registry?" → no
cd my-os
bootcher device provision            # build + disk; asks for your admin SSH key
# write output/<arch>/<type>/disk.* to the device, boot it, add it to `remotes`
bootcher device deploy               # every change after that: build here, push over SSH
bootcher device status               # what each device runs
bootcher device refresh              # base-image patches: rebuild what each device runs
```

With a registry, channels and releases:

```sh
bootcher init my-os                  # registry → yes, channels → stable, next
bootcher release publish v1.4 --promote next     # from a clean commit
bootcher channel promote v1.4 stable
bootcher device provision --channel stable --target aarch64 --disk raw
bootcher channel status              # stale base? → bootcher channel refresh
```

## Commands

```text
bootcher init [name] | info [--format] | build [--target] | clean      the project

bootcher device status | provision | takeover | deploy | refresh | switch | rotate
bootcher release publish X | refresh X | list
bootcher channel status | promote X ch | refresh
bootcher signing enroll | rotate | retire | verify
```

Off a TTY, every command that touches devices or moves a channel needs `-y`; on a TTY
it asks, with the options read from live state, and prints the equivalent command. See
the [command reference](https://cdellacqua.github.io/bootcher/book/reference/commands.html).

## Direct and channel devices

The devices bootcher manages are listed in `bootcher.toml`'s top-level `remotes`. What
each one follows is read live from the device (`bootc status`), never from config:

- A **direct** device runs `localhost/<name>:latest` — what you build here.
  `device deploy` serves the image from a throwaway, loopback-only registry and the
  device pulls it through an SSH remote-forward (`ssh -R`): only the missing layers move,
  and the bytes ride your SSH session, so the device needs no registry, certificate or
  credential. Rollback is `bootc rollback` on the device; base patches come from `device
  refresh`, which rebuilds **the commit each device booted** (deploy from a clean commit
  to make a device refreshable).
- A **channel** device follows `<registry>/<name>:<channel>` and updates itself on its
  `bootc-fetch-apply-updates.timer`. Moving the channel is the deploy.

A project with a registry usually has both — the bench board on your desk direct, the
field units on `:stable`, a canary rack on `:next` — and every command reaches exactly
its own devices: `device deploy` only the direct ones, `channel promote` only the devices
on that channel. **Only `device switch` moves a device**, onto a channel or to direct.

## Releases and channels

- A **release** `:X` is any valid OCI tag you choose (`v1.4`). `release publish` builds it
  from a **clean** commit, records that commit in every member's `bootcher.source` label,
  annotates the multi-arch index `bootcher.release=X`, and pushes it (signed, if you
  enabled signing). `:X` only ever holds that one commit: republishing from it is a
  no-op, from any other commit it's refused.
- A **channel** (`[registry] channels`) is a byte-for-byte copy of a release.
  `channel promote X ch` copies it by digest after checking the signature and that its
  base is current, then updates the devices on `ch`. Rollback is a promote of the
  previous release (`release refresh` it first if its base moved).
- **Base patches**: `channel status` reports channels whose base image moved;
  `channel refresh` rebuilds the release each serves **from its own commit**, under that
  commit's own config, re-pushes it and re-copies the channels. A scheduled CI job runs it
  (pause a channel with `--exclude`); see [`recipes/ci/`](recipes/ci/).

Two credentials exist for channel devices: a read-only **pull token** baked on each
device (rotate it with `device rotate pull-token`), and your own **push auth**
(`podman login`), which never leaves your machine or CI.

## Image signing

`bootcher signing enroll` turns it on: a cosign keypair, `[registry] key`, and an install
drop-in that makes every install record a signature-enforcing origin. Releases are signed
at publish, `channel promote` refuses a release the key doesn't verify, and channel
devices verify on every upgrade. Direct devices don't: their trust is the SSH channel, and
a direct install needs no trust files. On a running fleet, `enroll` also signs what the
channels serve (digests unchanged) and enforces signatures on the channel devices.
`signing rotate` adds a key with no window in which any device rejects its channel;
`signing retire` drops the old key once every device — including ones not in `remotes` —
has the new one.

## Lifecycle hooks

Each phase — the container build, the disk-image step, publishing a release, moving a
channel, the device rollout, the in-place takeover, and the credential rotation — is a
`[hooks.<phase>]` table with an optional `pre` and `post` command:

```toml
[hooks.build]
pre  = "echo 'prepare artifacts your Containerfile COPYs'"

[hooks.disk]
post = "echo 'edit the disk image: embed files outside the Containerfile reach (e.g. the ESP)'"

[hooks.publish]
post = "echo 'announce the release'"

[hooks.promote]
post = "echo 'a channel moved'"

[hooks.upgrade]
pre  = "echo 'drain the devices before the rollout'"
post = "echo 'smoke-test the devices'"

[hooks.takeover]
pre  = "echo 'snapshot the hosts before converting them'"

[hooks.rotate]
post = "echo 'revoke the old credential at its issuer'"
```

Each command runs with `sh -c` from the project root (for a refresh build, the release
commit's worktree, whose own hooks run), with the progress UI suspended and the terminal
handed over. A non-zero exit aborts the run. On the device phases (`upgrade`, `takeover`,
`rotate`) `post` runs once every device has been attempted, even if some failed, with each
outcome in `BOOTCHER_METADATA`'s `results`.

## Secrets: collected at provision, never stored

The per-device secrets — the admin SSH authorized key and, for a channel device of a
private registry, the pull token — are **not** baked into the container image.
`device provision` and `device takeover` collect them up front and inject them into the
device's persistent `/etc`, so the container image stays secret-free and can be published.

## Users

Users are declared as systemd JSON user records under `sysroot/usr/lib/userdb/` and
resolved at runtime by `nss-systemd`. To add a user, drop `<user>.user` and
`<user>.group` into `sysroot/usr/lib/userdb/`, a matching `home-<user>.conf` into
`sysroot/usr/lib/tmpfiles.d/`, then `RUN /usr/libexec/derive-userdb.sh` in your
Containerfile after `COPY sysroot/ /` so the reverse-lookup symlinks and membership
markers are generated. See systemd's [user-record spec][user-record].

SSH authorization is a **separate** concern — it is *not* in the userdb record.
sshd reads the admin's keys from `/etc/ssh/authorized_keys.d/admin` (an
`AuthorizedKeysFile` drop-in shipped in the scaffold), and `device provision` injects
that file into the device's `/etc` — see
[Secrets](#secrets-collected-at-provision-never-stored).

The scaffold ships one user: **`admin`** (uid 1000) — SSH-only via key auth,
passwordless `sudo` through `wheel`, login shell `fish`.

[user-record]: https://systemd.io/USER_RECORD/

## Developing bootcher itself

See [DEVELOPMENT.md](DEVELOPMENT.md) for build prerequisites and common workflows.

- [`crates/bootcher-core/`](crates/bootcher-core/) — the library: builders, one job
  module per command domain, device classification, the registry reads, the loopback
  registry, lifecycle hooks.
- [`crates/bootcher-core/scaffold/`](crates/bootcher-core/scaffold/) — the embedded
  project template stamped out by `bootcher init`.
- [`crates/bootcher-cli/`](crates/bootcher-cli/) — the `bootcher` binary.

`cargo xtask ci` runs the full check set (`cargo fmt --check`, `cargo clippy -D
warnings`, `cargo test`, and the CI recipe scripts' stdlib `unittest` suite, so it needs
`python3`). The CLI integration tests
([`crates/bootcher-cli/tests/integration.rs`](crates/bootcher-cli/tests/integration.rs))
scaffold throwaway projects with `bootcher init` and assert the project plumbing
(no podman needed); the registry-level tests
([`crates/bootcher-cli/tests/registry.rs`](crates/bootcher-cli/tests/registry.rs))
publish, promote and refresh against a loopback `registry:2` (podman + skopeo, no VM). `cargo xtask run <args>` runs a dev build of the binary;
`cargo xtask install` puts it on `PATH`.
