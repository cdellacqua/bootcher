# bootcher

bootcher is an opinionated tool for the full lifecycle of a [bootc](https://bootc-dev.github.io/bootc/) image project: scaffold, build, provision disk images, and roll updates — without touching the individual steps by hand.

A **bootcher project** is one logical image — potentially built for multiple architectures into a multi-arch manifest list — described by a `bootcher.toml` in the working directory. The image name, target architectures, build location, and deploy targets all come from that manifest.

## The two main flows

```mermaid
flowchart LR
    init["bootcher init"] --> provision["bootcher provision<br/>[build + disk]"]
    provision --> boot["first boot"]
    boot --> deploy["bootcher deploy<br/>[build + upgrade]"]
    deploy --> updated["devices updated"]
    updated -->|next release| deploy
```

**First-time provisioning** — `bootcher provision`

Builds the container image and the bootable disk artifact (via image-builder), collecting any one-time secrets (admin SSH key, optional registry pull token) up front and baking them into the disk image's persistent `/etc`. The artifact lands under `output/` for you to write to the target device.

**Subsequent deploys** — `bootcher deploy`

Rebuilds the container, pushes the update to the running device, and triggers `bootc upgrade`. The push backend is selected automatically based on the manifest: a direct LAN transfer over SSH, or a container registry for OTA-capable fleets.

## Quick start

```sh
bootcher init my-os      # scaffold a new project
cd my-os
bootcher provision       # build + disk; asks for SSH key on first run
```

After the first boot, rolling an update is:

```sh
bootcher deploy
```

## Install

See the [Installation](installation.md) page for binaries, cargo install instructions, and the container image for macOS / Windows.
