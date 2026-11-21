# bootcher

bootcher is an opinionated tool for the full lifecycle of a [bootc](https://bootc-dev.github.io/bootc/) image project: scaffold, build, provision disk images, publish releases, and keep devices updated and patched — without touching the individual steps by hand.

A **bootcher project** is one logical image — potentially built for multiple architectures into a multi-arch manifest list — described by a `bootcher.toml` in the working directory. The image name, target architectures, build location, registry and devices all come from that manifest.

Commands are grouped by what they act on: the project (`init`, `info`, `build`, `clean`), the **devices** (`device …`), the **releases** published to a registry (`release …`), the **channels** devices follow (`channel …`), and image **signing** (`signing …`).

## Two kinds of device

```mermaid
flowchart LR
    checkout["your checkout"] -->|device deploy| direct["direct devices"]
    checkout -->|release publish v1.4| release[":v1.4"]
    release -->|channel promote v1.4 stable| channel[":stable"]
    channel -->|auto-update timer| fleet["channel devices"]
```

- A **direct** device runs what you build on your machine: `bootcher device deploy` builds
  the checkout and pushes it over SSH. No registry, nothing hosted — the inner loop, and
  all you need for a small fleet you can reach.
- A **channel** device follows a channel in your registry (`:stable`, `:next`) and
  updates itself. You publish named releases from clean commits and promote them to
  channels; base-image patches are rebuilt from the same commits by `channel refresh`.

A project with a registry usually has both: the bench boards direct, the fleet on
channels. Only `bootcher device switch` moves a device between them.

## Quick start

Direct only:

```sh
bootcher init my-os                  # "publish to a registry?" → no
cd my-os
bootcher device provision            # build + disk; asks for an SSH key
# write the disk, boot it, add it to `remotes` in bootcher.toml
bootcher device deploy               # every change after that
```

With a registry and channels:

```sh
bootcher init my-os                  # registry → yes, channels → stable
bootcher release publish v1.0        # from a clean commit
bootcher channel promote v1.0 stable
bootcher device provision --channel stable
```

See [Project setup](workflows/setup.md), [Direct devices](workflows/direct.md) and
[Publishing releases](workflows/releases.md).

## Install

See the [Installation](installation.md) page for binaries, cargo install instructions, and the container image for macOS / Windows.
