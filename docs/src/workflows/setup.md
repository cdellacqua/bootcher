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
| `Containerfile` | `FROM quay.io/fedora/fedora-bootc:<N>` (the current Fedora release) plus a security baseline |
| `sysroot/` | Overlay `COPY`ed into the image; contains sshd drop-ins, sudoers, firewalld, fail2ban, and the admin userdb records |
| `.gitignore` | Keeps build artifacts and secrets out of git |
| `.containerignore` | Excludes some files and directories from the `podman build` context |

The interactive setup asks for target architecture(s), image format, whether to publish to a registry (then the channel names, suggesting `stable`, and whether to sign releases), the devices to manage, and builder locations (asked per arch for multi-arch projects, so you can route each arch's container build and disk-image step independently). Pass `-y` to accept all defaults without prompting (host arch, no registry — every device direct; useful in CI or scripted setups). Pass `-f` to scaffold into a non-empty directory.

If signing was opted into, `bootcher init` enrolls it right away (as `bootcher signing enroll` would): it writes `cosign.key`/`cosign.pub`, sets `[registry] key`, and bakes the install drop-in. The passphrase is read from `BOOTCHER_SIGN_PASSPHRASE` if set, else prompted on a TTY.

Edit the `Containerfile` freely — add `dnf install` steps, copy in assets, or replace baseline components. Edit `bootcher.toml` afterward if you need to adjust anything the questionnaire collected.

### Fedora releases

The `Containerfile` builds from one Fedora release, set at the top:

```dockerfile
ARG FEDORA_VERSION=45
FROM quay.io/fedora/fedora-bootc:${FEDORA_VERSION}
```

`init` writes the stable release current at the time (pass `--fedora-release <N>` to choose). The numbered tag keeps getting updates for the life of that release, so rebuilds still pick up security fixes. What it avoids is `:latest` moving to a new major release on Fedora's release day: with a scheduled rebuild, every device would upgrade and reboot into it unattended, before you've tested your own layers on it.

The cost is that a release reaches end of life about four weeks after the release two versions newer ships. Its tag then stops moving, and devices stop getting security updates. The [CI recipes](../recipes/index.md) check the pin on their weekly schedule: a warning once a newer release is out, a failure once the pin is two releases behind.

To upgrade, change `FEDORA_VERSION`, rebuild, test, and ship it (`device deploy`, or publish and promote a release). bootc upgrades across Fedora releases in place, and the previous deployment stays available for rollback. To follow `:latest` instead, replace the `FROM` line with `FROM quay.io/fedora/fedora-bootc:latest`.

The security baseline the scaffold brings — key-only SSH for `admin`, `firewalld`, `fail2ban`, `chrony`, automatic updates — is a **starting point, not a finished posture**. It is what a generic appliance can reasonably assume; it knows nothing about the ports your workload opens, the data it holds, or the network it lands on. Review it against your own threat model before a fleet leaves the bench, and treat everything in `sysroot/` as yours to tighten.

## 2. Provision

```sh
bootcher device provision                    # no registry: direct
bootcher device provision --channel stable   # with a registry: follow a channel
bootcher device provision --direct           # …or be updated with `device deploy`
```

`device provision` builds a disk image for a new device, in one of two modes:

- **`--direct`** (implied without a `[registry]`): builds the current checkout (or, with
  `--skip-build`, takes the `localhost/<name>:latest-<arch>` images already in local
  storage) and records `localhost/<name>:latest` as the origin. No pull credential and no
  signing trust files are baked. Update the device with `bootcher device deploy`.
- **`--channel <ch>`**: never builds. It pulls the selected arches' members of `:<ch>` —
  by digest, every time — into `localhost/<name>:pull-<ch>-<arch>`, bakes the read-only
  pull credential (unless `--anonymous`) and, with signing on, the trust files, and
  records the channel as the origin: the device updates whenever the channel moves.

With a registry and no mode flag, a TTY asks (channels annotated with what they serve,
plus "direct"); off a TTY one of the two flags is required.

It then:

1. **Collects the admin SSH key** — `--ssh-key <private-key-path>` (the `<path>.pub`
   sibling is read), or picked from `~/.ssh/` on a TTY.
2. **Collects the registry pull token** *(`--channel` only)* — from `BOOTCHER_PULL_USER` /
   `BOOTCHER_PULL_TOKEN`, else prompted on a TTY; checked against the registry unless
   `--skip-pull-check`. For a **public registry**, pass `--anonymous`.
3. **Builds or pulls** the container images.
4. **Builds the disk image** with image-builder, baking the collected files into the
   disk's persistent `/etc`.

The output artifact lands at `output/<arch>/<disk_type>/disk.<ext>`. `--target <arch>`
builds one arch, and `--disk <type>` (repeatable) restricts it to some of that arch's disk
types.

### Flags

| Flag | Purpose |
|---|---|
| `--channel <ch>` / `--direct` | The mode (see above) |
| `--ssh-key <path>` | Admin SSH private key; the `<path>.pub` sibling is injected. Omit to pick on a TTY |
| `--target <arch>`, `--disk <type>` | Build one arch / some of its disk types |
| `--skip-build` | `--direct` only: use the local `latest-<arch>` images |
| `--skip-pull-check` | `--channel` only: skip the local pull-token check (e.g. a VPN-only registry) |
| `--anonymous` | `--channel` only: the registry is public — bake no `auth.json` |

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

Once the device is online, list it in `remotes` (as `admin@<host>`). A direct device is updated with `bootcher device deploy` ([Direct devices](direct.md)); a channel device updates itself whenever its channel moves ([Publishing releases](releases.md)).
