# Project setup

This workflow covers everything from a blank directory to a provisioned device.

## 1. Scaffold the project

```sh
bootcher init my-os
cd my-os
```

`bootcher init` creates:

| Path | Purpose |
|---|---|
| `bootcher.toml` | The project manifest |
| `Containerfile` | `FROM quay.io/fedora/fedora-bootc:latest` plus a security baseline |
| `sysroot/` | Overlay `COPY`ed into the image; contains sshd drop-ins, sudoers, firewalld, fail2ban, and the admin userdb records |
| `.gitignore` | Keeps build artifacts and secrets out of git |
| `.containerignore` | Excludes some files and directories from the `podman build` context |

The interactive setup asks for target architecture(s), image format, registry, image signing, deploy targets, and builder locations (asked per arch for multi-arch projects, so you can route each arch's container build and disk-image step independently). Pass `-y` to accept all defaults without prompting (useful in CI or scripted setups). Pass `-f` to scaffold into a non-empty directory.

If signing was opted into, `bootcher init` prints a `bootcher sign enroll` reminder — run it before provisioning to create `cosign.key`/`cosign.pub`. The passphrase is read from `BOOTCHER_SIGN_PASSPHRASE` if set, else prompted on a TTY.

Edit the `Containerfile` freely — add `dnf install` steps, copy in assets, or replace baseline components. Edit `bootcher.toml` afterward if you need to adjust anything the questionnaire collected.

## 2. Provision

```sh
bootcher provision
```

`provision` builds the container, then its disk image, in sequence. It runs the following automatically:

1. **Collects the admin SSH key** — prompted from available `~/.ssh/` keys on a TTY, or supplied via `--key <private-key-path>` (the `<path>.pub` sibling is read). In non-interactive mode, the sole available key is used automatically.
2. **Collects the registry pull token** *(registry mode only)* — read from `BOOTCHER_PULL_USER` / `BOOTCHER_PULL_TOKEN` if set, else prompted on a TTY. Pre-validated against the registry unless `--skip-pull-check` is passed. For a **public registry** that needs no authentication, leave the username blank at the prompt (or pass `--anonymous` non-interactively) — no `auth.json` is baked and the device pulls anonymously.
3. **Builds** the container image with `podman build`.
4. **Builds the disk image** with image-builder, baking the collected secrets into the disk image's persistent `/etc`.

The output artifact lands at `output/<arch>/<disk_type>/disk.<ext>`. Write it to the target storage medium or upload it to a provisioning service.

### Flags

| Flag | Purpose |
|---|---|
| `--key <path>` | Admin SSH private key; the `<path>.pub` sibling is injected. Omit to prompt on a TTY |
| `--skip-pull-check` | Skip the pre-flight pull-token validation (e.g. when the registry is VPN-only) |
| `--anonymous` | The registry is public: provision with no pull token (no `auth.json` baked, device pulls anonymously) |

### Skipping the build

`bootcher build` builds just the container; `bootcher provision --skip-build` then builds the disk from it without rebuilding — for iterating on the disk step without repeating the container build.

## After provisioning

How you get the image onto the target depends on the format chosen at init:

**Physical device (`raw`)** — write directly to the block device:
```sh
sudo dd if=output/x86_64/raw/disk.raw of=/dev/sdX bs=4M status=progress conv=fsync
```
GUI alternatives: [Gnome Disks](https://apps.gnome.org/DiskUtility/) ("Restore Disk Image…") or [Balena Etcher](https://etcher.balena.io/).

**Physical device (`bootc-installer`)** — burn the installer ISO to a USB drive with `dd` or Balena Etcher, then boot it on the target to install to internal storage.

**Virtual machine (`qcow2`)** — boot directly with QEMU:
```sh
qemu-system-x86_64 -m 2G -hda output/x86_64/qcow2/disk.qcow2 -enable-kvm
```
Or import into virt-manager / libvirt as a new VM using the qcow2 as an existing disk.

**Cloud (`ami`, `vhd`, `gce`)** — upload via your provider's CLI (`aws ec2 import-image`, `az image create`, `gcloud compute images import`) then launch an instance from the imported image.

Once the device is online, subsequent updates are pushed with `bootcher deploy` — see [Deploy updates](deploy.md).
