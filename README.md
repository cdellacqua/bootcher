# bootcher

[![CI](https://github.com/cdellacqua/bootcher/actions/workflows/ci.yml/badge.svg)](https://github.com/cdellacqua/bootcher/actions/workflows/ci.yml)
[![Audit](https://github.com/cdellacqua/bootcher/actions/workflows/audit.yml/badge.svg)](https://github.com/cdellacqua/bootcher/actions/workflows/audit.yml)
[![Release](https://github.com/cdellacqua/bootcher/actions/workflows/release.yml/badge.svg)](https://github.com/cdellacqua/bootcher/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/cdellacqua/bootcher?sort=semver)](https://github.com/cdellacqua/bootcher/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

An opinionated tool for crafting [bootc](https://bootc-dev.github.io/bootc/) images.

`bootcher` scaffolds a self-contained **project** — one bootc image that starts
`FROM quay.io/fedora/fedora-bootc:latest` with a security baseline (admin user,
SSH, sudo, firewalld + fail2ban + chrony, auto-updates) layered on top — then
drives the lifecycle: build the container (`podman build`), turn it into a
bootable disk image with
[image-builder](https://github.com/osbuild/image-builder), and roll
subsequent updates to running devices.

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
| **podman** | All operations | Builds the container image (`bootcher build`), runs image-builder (`bootcher disk` / `bootcher provision`), and pushes to registries (`bootcher deploy`). Version ≥ 4 recommended. |
| **ssh / ssh-keygen** | `bootcher provision`, `bootcher deploy` / `upgrade` | Key generation at provision time; SSH tunnel and remote commands for deploy. Usually provided by `openssh-clients`. |
| **sudo** | `bootcher disk` / `bootcher provision` | image-builder must run as root; bootcher invokes it via `sudo`. Passwordless `sudo sh` is required for unattended runs. |
| **qemu** (`qemu-system-<arch>`, `qemu-img`) + UEFI firmware | `[builder] = "vm"` | Only when a build step is routed to a throwaway local VM (e.g. for cross-arch builds). Firmware is OVMF (x86_64) / AAVMF (aarch64). Not needed for the default in-process `local` builder. |
| **qemu-user** (binfmt_misc handler, e.g. `qemu-user-static`) | Cross-arch in-process builds | Only when building a foreign-arch image with the default `local` builder; not needed if that arch's steps are routed to a `vm`. |

image-builder itself is pulled as a pinned container image by Podman — you do not need to install it separately.

Each subcommand checks the tools it actually needs **before doing any work** and fails immediately, naming any that are missing and how to install them — so a missing tool never surfaces as a bare spawn error partway through a long build. A given run only needs the rows matching its `[builder]` / `[deploy]` configuration.

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

```sh
# interactive setup; pass -y to accept defaults
bootcher init my-os
cd my-os
# interactive first provisioning, it will ask for one-time
# configurations such as ssh key and registry authentication for
# automated future updates 
bootcher provision
```

After the first boot, deploying updates to the running device can be done by simply launching:

```sh
bootcher deploy
```

## The two flows

- **First-time install** — `bootcher provision`: build → disk image. It leaves
  the image-builder artifact under `output/<name>/<arch>/` for you to write or
  upload to your target (the format is the manifest's `disk_type`, `qcow2` by
  default). The image grows its root filesystem to fill the disk on first boot.
  Project-specific post-processing (e.g. embedding firmware or files outside the
  Containerfile's reach) goes in a `[hooks.disk] post` lifecycle hook — see
  [Lifecycle hooks](#lifecycle-hooks).
- **Subsequent deploys** — `bootcher deploy`: build → push to the
  running target and `bootc upgrade`. `[deploy] remotes` lists the SSH
  destinations (e.g. `["admin@192.168.1.42"]`). The *push* half runs over one of two
  backends — a direct LAN transfer or a container registry — selected
  automatically; see [Deploy backends](#deploy-backends-lan-vs-registry).

Per-stage shortcuts exist too: `bootcher build`, `bootcher disk`,
`bootcher upgrade`. Run `bootcher --help` for the full list.

## Project layout

`bootcher init` scaffolds:

- `bootcher.toml` — the manifest: `[general]` holds `name` (the image name,
  used for the container tag and output dir) and `rootfs`; `[targets]` maps each
  target arch to its disk type(s) (one arch, or several for a multi-arch image);
  `[deploy]` holds `remotes` (SSH targets) and
  an optional `registry` for registry-mode deploys (see
  [Deploy backends](#deploy-backends-lan-vs-registry)). Its presence marks the
  directory as a bootcher project.
- `Containerfile` — `FROM quay.io/fedora/fedora-bootc:latest` plus the security
  baseline. **This is yours to edit** — add `dnf install`s, units, and assets as
  plain `RUN`/`COPY` steps; replace or `systemctl disable` anything the baseline
  ships.
- `sysroot/` — an overlay `COPY`ed into the image (`COPY sysroot/ /`): sshd / sudoers
  drop-ins, firewalld zone, fail2ban jail, the admin userdb records, and
  `/usr/libexec/derive-userdb.sh`.
- `.gitignore` — keeps build artifacts and rendered secrets out of git.
- `.containerignore` — excludes files and directories from the `podman build` context.
- `output/<name>/<arch>/` — image-builder artifacts (gitignored).

## Lifecycle hooks

Hooks allow performing project-specific tasks when building with `bootcher`, by providing a way of injecting custom behavior between the standard steps.

Each phase — the container build, the disk-image step, and the deploy upgrade —
is a `[hooks.<phase>]` table with an optional `pre` and `post` command:

```toml
[hooks.build]
pre  = "echo 'here you could prepare some artifacts that your Containerfile could reference in a COPY command'"
post = "echo 'here you could perform some cleanups'"

[hooks.disk]
pre  = "echo 'here you could perform some tests on the built artifact before turning it into a disk image'"
post = "echo 'here you could edit the disk image to embed stuff that is outside of the Containerfile reach (e.g. files in the ESP/EFI partition)'"

[hooks.upgrade]
pre  = "echo 'here you could drain or notify the fleet before rolling out'"
post = "echo 'here you could run a post-deploy smoke test against the devices'"
```

Each command is run with `sh -c` from the project root, with bootcher's progress
UI suspended and the terminal handed over — so a hook may print freely, prompt, or
`sudo` for the steps that need root. A non-zero exit aborts the run.

The hooks fire wherever the phase runs, so they apply across the shortcuts too:
`[hooks.build]` wraps the container build (`build`, `provision`, `deploy`);
`[hooks.disk]` wraps the disk build (`disk`, `provision`); `[hooks.upgrade]`
wraps the push + per-device `bootc upgrade` (`upgrade`, `deploy`).

## Secrets: collected at provision, never stored

The two per-deployment secrets — the admin SSH authorized key and (in registry
mode) the registry pull token — are **not** baked into the container image. `disk` / `provision` collect them up front and inject them into the future persistent `/etc` of the device image (the installable image produced by image-builder). This way, the container image
stays secret-free and can be safely published to a remote registry.

## Deploy backends (LAN vs registry)

`deploy` / `upgrade` push the freshly built image to the target over one of two
backends, decided entirely by the manifest's `registry` field — there's no flag:

- **LAN (default)** — when `registry` is unset in `bootcher.toml`. Your computer briefly
  serves the image from a throwaway, loopback-only registry and the target pulls
  it over an SSH remote-forward (`ssh -R`). Only
  the layers the target is missing cross the wire, and the image rides your
  existing SSH session — no TLS, certs, or registry config on the device is necessary thanks to the secure tunnel. Ideal
  for a workshop bench or an air-gapped LAN. This configuration requires you to configure at least one remote (IP or domain name) in `bootcher.toml`.
- **Registry** — when `registry = "registry.example.com/"` under
  `[deploy]` in `bootcher.toml`. The image is `podman push`ed there and the target is pointed at
  `<registry>/<name>:latest` with `bootc switch`. The
  `bootc-fetch-apply-updates.timer` then becomes genuine OTA: push from this
  machine *or* a cloud runner and targets pull on their own. If `[deploy] remotes` is configured, the upgrade is triggered immediately.

In registry mode, two distinct credentials are needed:

- **Pull token** — ends up on *every deployed device*, so use a
  least-privilege, read-only token (a GitLab `read_registry` deploy token from
  *Settings → Repository → Deploy tokens*, which yields a
  `gitlab+deploy-token-N` username plus a secret; a registry robot account; etc.).
- **Push auth** — this builder's own `podman login <host>`, needed for `deploy`
  to push. It's powerful (write) and stays on the builder / in a CI secret;
  `bootcher` never puts it on a device. Log in separately:
  `podman login registry.gitlab.com`.

## Updates

Once provisioned, the target self-updates: `bootc-fetch-apply-updates.timer`
fires periodically, runs `bootc upgrade --apply`, and reboots if a newer image
revision is available.

The timer only finds new revisions if the target's bootc origin points somewhere
that *changes on its own* — i.e. a registry. In **registry** mode a push from
this machine or a cloud runner is enough. In **LAN** mode the origin is the
target's local containers-storage, so deploys are always push-driven and the
timer is effectively a no-op.

## Users

Users are declared as systemd JSON user records under `sysroot/usr/lib/userdb/` and
resolved at runtime by `nss-systemd`. To add a user, drop `<user>.user` and
`<user>.group` into `sysroot/usr/lib/userdb/`, a matching `home-<user>.conf` into
`sysroot/usr/lib/tmpfiles.d/`, then `RUN /usr/libexec/derive-userdb.sh` in your
Containerfile after `COPY sysroot/ /` so the reverse-lookup symlinks and membership
markers are generated. See systemd's [user-record spec][user-record].

SSH authorization is a **separate** concern — it is *not* in the userdb record.
sshd reads the admin's keys from `/etc/ssh/authorized_keys.d/admin` (an
`AuthorizedKeysFile` drop-in shipped in the scaffold), and `provision` injects
that file into the device's `/etc` — see
[Secrets](#secrets-collected-at-provision-never-stored).

The scaffold ships one user: **`admin`** (uid 1000) — SSH-only via key auth,
passwordless `sudo` through `wheel`, login shell `fish`.

[user-record]: https://systemd.io/USER_RECORD/

## Developing bootcher itself

See [DEVELOPMENT.md](DEVELOPMENT.md) for build prerequisites and common workflows.

- [`crates/bootcher-core/`](crates/bootcher-core/) — the library: builders, jobs,
  pipelines, the LAN registry, lifecycle hooks.
- [`crates/bootcher-core/scaffold/`](crates/bootcher-core/scaffold/) — the embedded
  project template stamped out by `bootcher init`.
- [`crates/bootcher-cli/`](crates/bootcher-cli/) — the `bootcher` binary.

`cargo xtask ci` runs the full check set (`cargo fmt --check`, `cargo clippy -D
warnings`, `cargo test`). The CLI integration tests
([`crates/bootcher-cli/tests/integration.rs`](crates/bootcher-cli/tests/integration.rs))
scaffold throwaway projects with `bootcher init` and assert the project plumbing
(no podman needed). `cargo xtask run <args>` runs a dev build of the binary;
`cargo xtask install` puts it on `PATH`.
