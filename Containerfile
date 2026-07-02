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
#
# sudo: the image-builder disk step (`bootcher provision`) shells out to it to run
# the privileged disk build, so it belongs in the runtime even though the binary
# never calls it directly on the deploy path.
#
# jq/unzip/python3 aren't used by the bootcher binary itself — they're the
# interpreters the first-party hook recipes (recipes/*) shell out to, so the
# containerized entry point can run them: hooks execute with `sh -c` in this
# image. The raspi4 disk.post recipe needs all three.
#
# zstd + zip: the CI recipes (recipes/ci/*) compress each built disk (zstd) and
# pack it into a single, natively-extractable .zip (store mode — the member is
# already compressed) before uploading it to a release/package registry. Baked in
# here so the pipelines need no `dnf install` step — the published image is
# self-sufficient for the recipes it ships.
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
      e2fsprogs \
      openssh-clients \
      curl \
      which \
      tar \
      sudo \
      zstd \
      systemd-libs \
      jq \
      unzip \
      zip \
      python3 \
 && dnf clean all

COPY --from=builder /src/target/release/bootcher /usr/bin/bootcher
ENTRYPOINT ["/usr/bin/bootcher"]
