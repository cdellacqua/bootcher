# bootcher as a container image — the cross-platform entry point.
#
# The native binary is Linux-only (it links libudev and drives loop/block
# devices + podman), but wrapping it in a privileged container lets it run on
# macOS and Windows too, through the Podman/Docker machine VM — the same model
# image-builder uses. See the README for the `podman run` invocation.

# ---- build stage --------------------------------------------------------
# Pinned to the toolchain in rust-toolchain.toml. Debian bookworm's glibc
# (2.36) is older than the Fedora runtime's, so the dynamically-linked binary
# stays forward-compatible when it lands in the runtime stage. Built natively
# per-arch in CI (no emulation), so this works for both amd64 and arm64.
FROM docker.io/library/rust:1.96.0-slim-bookworm AS builder
# clang/libclang: loopdev-3's build script runs bindgen, which needs libclang
# at build time. libudev-dev + pkg-config: the udev crate links libudev.
RUN apt-get update \
 && apt-get install -y --no-install-recommends clang libudev-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# --locked: never mutate Cargo.lock; fail if it's stale.
RUN cargo build --release --locked -p bootcher-cli

# ---- runtime stage ------------------------------------------------------
# Fedora carries every system tool bootcher shells out to, plus libudev.so.1
# (systemd-libs) and the UEFI firmware the VM builder boots guests with.
FROM quay.io/fedora/fedora:44
RUN dnf install -y --setopt=install_weak_deps=False \
      podman \
      fuse-overlayfs \
      qemu-img \
      qemu-system-x86-core \
      qemu-system-aarch64-core \
      edk2-ovmf \
      edk2-aarch64 \
      cloud-utils-growpart \
      util-linux \
      openssh-clients \
      curl \
      which \
      tar \
      systemd-libs \
 && dnf clean all

# containers-common (pulled in by podman) ships /usr/share/containers/storage.conf
# with `mountopt = "nodev,metacopy=on"`. metacopy=on is hostile to the nested
# image-store builds bootcher drives inside this container (the `:O` overlay-on-
# overlay mount in builder/local.rs). Strip it in place: this preserves every other shipped
# default (additionalimagestores, imagestore, …) — which a minimal /etc override
# would silently drop, since storage.conf replaces rather than merges. A drop-in
# under storage.conf.d is *not* honoured for overlay.mountopt, so sed is the fix.
# No-op if upstream ever drops metacopy on its own.
RUN sed -i 's/,metacopy=on//' /usr/share/containers/storage.conf

COPY --from=builder /src/target/release/bootcher /usr/bin/bootcher
ENTRYPOINT ["/usr/bin/bootcher"]
