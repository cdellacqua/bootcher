# Takeover: convert a live host in place

`bootcher device takeover` converts a live host — for example a stock Ubuntu/Debian/Rocky VPS — into a bootc system **in place**, over SSH. Use it when you can't [disk-provision](setup.md) the machine and can only reach it over the network.

> ⚠️ **Takeover replaces the target's OS.** The host reboots into the new image. All existing files from the previous OS are retained under the new system's `/sysroot`. **Back up the VPS first** — a provider snapshot, or anything you need off the box. An interactive run names every target and makes you confirm.

```sh
bootcher device takeover --direct --remote vps1 --login debian
bootcher device takeover --channel stable --remote vps2
```

It never converts every remote by default: name the hosts with `--remote` (on a TTY,
pick them from a checklist, none pre-checked). The mode is `--direct` (built here,
shipped over SSH, updated with `device deploy`) or `--channel <ch>` (the host pulls the
channel itself; nothing is built or pushed), as for [provisioning](setup.md#2-provision);
without a registry `--direct` is implied, and on a TTY with a registry you're asked.
The confirmation names every host and the mode it ends up in; off a TTY `-y` is
required.

## Prerequisites

Each target must already have **`podman`** and **`sudo`** installed — bootcher never mutates the foreign distro, so install them yourself first:

```sh
ssh debian@vps 'sudo apt-get install -y podman'   # or dnf/zypper install -y podman
```

The pre-flight (run before the build) first tries each host as `admin@` with `--ssh-key` — a host that answers with a working `bootc status` is already converted and is skipped — then checks it over the stock login and fails fast on:

- missing `podman` or `sudo`;
- a CPU architecture the project doesn't build (`[targets]`);
- a root filesystem the installer can't adopt (must be `ext4` / `xfs` / `btrfs`);
- an empty/missing `/boot`.

Both UEFI and legacy BIOS hosts are supported.

The authoritative layout check runs at install start; the pre-flight is the cheap filter so a disqualified host fails before a multi-GB build.

### Networking is the image's responsibility

Takeover does not configure the new system's network. The image must bring its own working network config — cloud-init, or NetworkManager keyfiles baked into the `Containerfile` — or the host won't come back online after the reboot. Make sure the image is reachable on the target's addressing **before** taking over a remote box you can't physically touch.

## The two SSH identities

A takeover spans two logins, because the image's `admin` user (uid 1000) replaces the stock cloud user at install time:

- **Before the reboot** — image transfer, the install, the reboot itself — bootcher connects as the **stock cloud user** (`debian`/`ubuntu`/`cloud-user`/`root`), the only login that exists yet, which has passwordless sudo on cloud images.
- **After the reboot** — the `wait online` + `bootc status` verify — it reconnects as **`admin@host`**, the admin identity it just injected, proving the new system came up with a working admin key.

So `remotes` stays the steady-state `admin@host` (consistent with every other device command), and bootcher only needs to be told the **stock login** for the initial connection — per host with a remote's `takeover_login`, or fleet-wide with `--login` (prompted for on a TTY when a host has neither):

```toml
remotes = [
  "admin@10.0.0.5",                                                 # uses --login
  { remote = "admin@vps.example.com", takeover_login = "debian" },  # per-host override
]
```

## What a run does, per host

1. **Build** the container image (`--direct` only, with `[hooks.build]`; `--skip-build` reuses the local images).
2. **Get the image** into the host's `containers-storage`: `--direct` ships the arch-matched member over an `ssh -R`-tunnelled loopback registry, as `device deploy` does; `--channel` has the host pull the channel itself (with the pull credential, when there is one).
3. **Convert** the disk in place from the transferred image.
4. **Inject secrets** into the staged deployment's `/etc` — the admin key and, for `--channel`, the pull token and (signing on) the trust files — exactly the file set [disk provisioning](setup.md) bakes. The same step masks `boot.automount` there (see [Hosts with an ESP](#hosts-with-an-esp)).
5. **Reboot**, wait for the host to answer SSH on the new boot as `admin@`, assert `bootc status`, and check that nothing automounts on `/boot`.

Hosts roll out in parallel, capped by `[concurrency] takeover`. Steps 2–5 are bracketed, once for the whole set, by the [`[hooks.takeover]`](../reference/bootcher-toml.md#hooksphase) `pre`/`post` commands. `post` runs once every host has been attempted, even if some failed — each host's outcome is in `BOOTCHER_METADATA`'s [`results`](../reference/bootcher-toml.md#bootcher_metadata) — and the run still fails afterwards if any host did.

## Afterwards

The host is a normal bootc device with no `bootcher.toml` change needed — `remotes` already names it as `admin@host`. A direct one gets updates with `bootcher device deploy`, a channel one whenever its channel moves, and both roll credentials with `bootcher device rotate`, exactly as for a disk-provisioned device.

## Hosts with an ESP

A taken-over host keeps its disk layout: `/boot` is a directory on the root filesystem, and most cloud and VPS images also carry an EFI System Partition (ESP), whatever firmware they boot with. On that layout systemd-gpt-auto-generator adds an automount for the ESP on `/boot`, on top of ostree's own `/boot`. ostree then can't apply a staged upgrade at shutdown (`ostree-finalize-staged`: `Remounting /boot read-write: Invalid argument`), and the host reboots into its old deployment. Takeover masks the automount in the new deployment's `/etc`, which bootc keeps across upgrades, so this needs nothing from your image. Every device step checks what each device booted and fails if a staged upgrade wasn't applied.
