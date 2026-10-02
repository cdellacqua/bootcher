# Installation

bootcher is **Linux-only** as a native binary (Podman is a native Linux tool). macOS and Windows users should use the [container image](#container-image).

## Runtime dependencies

| Dependency | Required for | Notes |
|---|---|---|
| **podman** | All operations | Builds the container image (`bootcher build`), runs image-builder (`bootcher provision`), and pushes to registries (`bootcher deploy`). Version ≥ 4 recommended. |
| **ssh / ssh-keygen** | `bootcher provision`, `bootcher deploy`, `bootcher takeover` | Key generation at provision time; SSH tunnel and remote commands for deploy/takeover. Usually provided by `openssh-clients`. |
| **sudo** | `bootcher provision` | image-builder must run as root; bootcher invokes it via `sudo`. Passwordless `sudo sh` is required for unattended runs. |
| **qemu** (`qemu-system-<arch>`, `qemu-img`) + UEFI firmware | `[builder] = "vm"` | Only when a build step is routed to a throwaway local VM (e.g. for cross-arch builds). Firmware is OVMF (x86_64) / AAVMF (aarch64). Not needed for the default in-process `local` builder. |
| **qemu-user** (binfmt_misc handler, e.g. `qemu-user-static`) | Cross-arch in-process builds | Only when building a foreign-arch image with the default `local` builder; not needed if that arch's steps are routed to a `vm`. |

image-builder itself is pulled as a pinned container image by Podman — you do not need to install it separately.

Each subcommand checks the tools it actually needs **before doing any work** and, if any are missing, fails immediately with a single message naming them and how to install them — so you never get a bare spawn error partway through a long build. The table above lists the full set; a given run only requires the rows matching its `[builder]` / `[deploy]` configuration.

## Binaries

Prebuilt release binaries are published to [github.com/cdellacqua/bootcher/releases](https://github.com/cdellacqua/bootcher/releases). They are built on Ubuntu 24.04 and require glibc ≥ 2.39.

Download the binary for your architecture, make it executable, and place it somewhere on your `PATH`:

```sh
chmod +x bootcher
sudo mv bootcher /usr/local/bin/
```

## From source

If you have Rust and Cargo installed, use `cargo install` from a local checkout or directly from the git repository:

```sh
# from git
cargo install --git https://github.com/cdellacqua/bootcher bootcher-cli
# from a local checkout
cargo install --path crates/bootcher-cli
```

## Container image

Since the native binary is Linux-only, a multi-arch image is available at
[ghcr.io/cdellacqua/bootcher](https://ghcr.io/cdellacqua/bootcher) (tags: `vX.Y.Z` and `latest`). It runs privileged with the host's container storage mounted in, which lets it work from the Podman/Docker machine VM on macOS and Windows.

Run it with `sudo` from inside your project directory. The process inside is root and writes every image it builds into the mounted store, so that store is the rootful one at `/var/lib/containers/storage`: `sudo podman` owns it, and `sudo podman rmi` can remove what a build leaves there.

```sh
sudo podman run --rm -it --privileged \
    --security-opt label=type:unconfined_t \
    -v "$PWD:/project" -w /project \
    -v /var/lib/containers/storage:/var/lib/containers/storage \
    ghcr.io/cdellacqua/bootcher:latest <command>
```

The image includes QEMU and UEFI firmware, so the `vm` builder (`[builder] = "vm"` in `bootcher.toml`) works out of the box: `--privileged` exposes `/dev/kvm` for same-arch guests, and the cross-arch builder uses TCG (pure userspace emulation) which needs no host devices at all.

If you use the `vm` builder, also mount the bootcher cache so the Fedora Cloud base image is not re-downloaded on every run. Put that cache on a root-owned path too (`/var/cache/bootcher`), so it doesn't land in your home directory owned by root:

```sh
sudo podman run --rm -it --privileged \
    --security-opt label=type:unconfined_t \
    -v "$PWD:/project" -w /project \
    -v /var/lib/containers/storage:/var/lib/containers/storage \
    -v /var/cache/bootcher:/root/.cache/bootcher \
    ghcr.io/cdellacqua/bootcher:latest <command>
```
