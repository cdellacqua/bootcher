bootcher := "cargo run --quiet -p bootcher-cli --"

# Extra flags threaded into the cargo build/check commands. Empty for local dev;
# CI overrides it (`just cargo_flags=--frozen ci`) to stay offline and pin
# Cargo.lock after a prior `cargo fetch --locked`.
cargo_flags := ""

# Set non-empty to also run the slow VM end-to-end test from `ci`
# (`just e2e=1 ci`), or run it directly with `just e2e`. Opt-in everywhere because
# it needs podman, qemu+KVM, UEFI firmware, and passwordless `sudo sh`; it
# hard-fails when those are missing (it asserts its prerequisites rather than
# skipping), so only enable it on a runner that can actually host a VM.
e2e := ""

[private]
default:
    @just --list --unsorted

# bootcher is now a standalone CLI: end users `cargo install` it and run
# `bootcher <cmd>` *inside their own project* (scaffolded by `bootcher init`).
# This Justfile is for developing bootcher itself — build/image/deploy live in a
# project, not here. Run a dev build of the binary against any args with:
#   just run init my-project
#   just run build              # (from inside a project dir; --platform optional)

# Run the dev build of the bootcher binary with arbitrary args.
[group('dev')]
run *args:
    {{bootcher}} {{args}}

# Install the CLI onto PATH (the real-world entry point: `cargo install`).
[group('dev')]
install:
    cargo install --path crates/bootcher-cli

# Build the cross-platform container image locally (what CI publishes to ghcr.io).
[group('dev')]
image tag="bootcher:dev":
    podman build -f Containerfile -t {{tag}} .

[group('dev')]
test:
    cargo test {{cargo_flags}}

# Slow VM end-to-end: boot real bootc disks and exercise the LAN rotate + upgrade
# lifecycle (e2e_vm), the plain registry lifecycle + pull-token rotation + signing
# enrollment (e2e_registry), the LAN→registry origin switch (e2e_lan_to_registry),
# and registry-mode image signing (e2e_registry_sign).
#
# Each test uses its own podman store under /var/tmp/bootcher-e2e-*-store, wiped on
# teardown by default. Set BOOTCHER_E2E_KEEP_STORE=1 to keep them across runs (warm
# base-image cache, faster reruns) at the cost of several GB of disk per store.
[group('dev')]
e2e:
    cargo test {{cargo_flags}} -p bootcher-cli --features=e2e --test e2e_vm -- --nocapture
    cargo test {{cargo_flags}} -p bootcher-cli --features=e2e --test e2e_registry -- --nocapture
    cargo test {{cargo_flags}} -p bootcher-cli --features=e2e --test e2e_lan_to_registry -- --nocapture
    cargo test {{cargo_flags}} -p bootcher-cli --features=e2e --test e2e_registry_sign -- --nocapture

# Cross-arch builder end-to-end: build a *foreign-arch* bootc disk (image-builder in a TCG
# builder VM) and boot it under TCG. Far slower than `e2e` and with different
# prerequisites (foreign qemu + firmware + qemu-user binfmt; no KVM/sudo), so it's
# gated apart, behind the `e2e_cross` feature.
#
# Honors BOOTCHER_E2E_KEEP_STORE (see `e2e`) for its podman store; the separate
# foreign-arch cloud-image download cache is always kept regardless.
[group('dev')]
e2e-cross:
    cargo test {{cargo_flags}} -p bootcher-cli --features=e2e_cross --test e2e_cross_arch -- --nocapture

# Everything CI runs (also invoked by .github/workflows/ci.yml so they can't drift).
# Pass `e2e=1` to additionally run the VM end-to-end test (see the `e2e` recipe),
# and/or `e2e_cross=1` for the cross-arch builder test (see the `e2e-cross` recipe).
[group('dev')]
ci e2e="" e2e_cross="":
    cargo fmt --check
    cargo clippy --all-targets {{cargo_flags}} -- -D warnings
    # --document-private-items so intra-doc links in private items' docs get checked too (this is a lint pass, output is discarded; the published `docs` recipe stays public-only).
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items {{cargo_flags}}
    cargo test {{cargo_flags}}
    {{ if e2e != "" { "just e2e" } else { "true" } }}
    {{ if e2e_cross != "" { "just e2e-cross" } else { "true" } }}


# Build the published site into docs/site/: the landing page (docs/landing/) at the
# root, the mdBook under docs/site/book/, and rustdoc embedded under book/api/. The
# GitHub Pages workflow uploads docs/site/ as-is.
[group('dev')]
docs:
    mdbook build docs/
    cargo doc --workspace --no-deps {{cargo_flags}}
    rm -rf docs/book/api
    cp -r target/doc docs/book/api
    rm -rf docs/site
    mkdir -p docs/site
    cp -r docs/landing/. docs/site/
    cp -r docs/book docs/site/book

# Render and serve the site generated from landing + docs http://localhost:3000.
[group('dev')]
site-serve:
    just docs
    python3 -m http.server 3000 --directory docs/site


# Serve the documentation site with live reload at http://localhost:3000.
# Note: mdbook clears its output on every rebuild, so the rustdoc embedded by
# `just docs` does not survive here. Use `just docs-serve` to edit prose and
# `just docs` when you need working API links.
[group('dev')]
docs-watch:
    mdbook serve docs/

# Attempts to fix formatting and linting issues.
[group('dev')]
fix:
    cargo fmt
    cargo clippy --fix --allow-dirty --all-targets {{cargo_flags}} -- -D warnings
