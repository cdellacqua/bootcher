# Takeover: convert a live host in place

`bootcher takeover` converts a live host — for example a stock Ubuntu/Debian/Rocky VPS — into a bootc system **in place**, over SSH. Use it when you can't [disk-provision](setup.md) the machine and can only reach it over the network.

> ⚠️ **Takeover replaces the target's OS.** The host reboots into the new image. All existing files from the previous OS are retained under the new system's `/sysroot`. **Back up the VPS first** — a provider snapshot, or anything you need off the box. An interactive run names every target and makes you confirm.

```sh
bootcher takeover --login debian
 ```

## Prerequisites

Each target must already have **`podman`** and **`sudo`** installed — bootcher never mutates the foreign distro, so install them yourself first:

```sh
ssh debian@vps 'sudo apt-get install -y podman'   # or dnf/zypper install -y podman
```

The pre-flight (run before the build) checks each host over SSH and fails fast on:

- missing `podman` or `sudo`;
- a CPU architecture the project doesn't build (`[general.disk_types]`);
- a root filesystem the installer can't adopt (must be `ext4` / `xfs` / `btrfs`);
- an empty/missing `/boot`, or a host booted in **legacy BIOS** mode (UEFI is required — `/sys/firmware/efi` must exist).

The authoritative layout check runs at install start; the pre-flight is the cheap filter so a disqualified host fails before a multi-GB build.

### Networking is the image's responsibility

Takeover does not configure the new system's network. The image must bring its own working network config — cloud-init, or NetworkManager keyfiles baked into the `Containerfile` — or the host won't come back online after the reboot. Make sure the image is reachable on the target's addressing **before** taking over a remote box you can't physically touch.

## The two SSH identities

A takeover spans two logins, because the image's `admin` user (uid 1000) replaces the stock cloud user at install time:

- **Before the reboot** — image transfer, the install, the reboot itself — bootcher connects as the **stock cloud user** (`debian`/`ubuntu`/`cloud-user`/`root`), the only login that exists yet, which has passwordless sudo on cloud images.
- **After the reboot** — the `wait online` + `bootc status` verify — it reconnects as **`admin@host`**, the admin identity it just injected, proving the new system came up with a working admin key.

So `[deploy] remotes` stays the steady-state `admin@host` (consistent with `deploy`/`rotate`), and bootcher only needs to be told the **stock login** for the initial connection — per host with a remote's `takeover_login`, or fleet-wide with `--login`:

```toml
[deploy]
remotes = [
  "admin@10.0.0.5",                                                 # uses --login
  { remote = "admin@vps.example.com", takeover_login = "debian" },  # per-host override
]
```

## What a run does, per host

1. **Build** the container image (the pipeline's first phase, with `[hooks.build]`).
2. **Transfer** the arch-matched image into the host's `containers-storage` — the same two backends as `deploy`: a loopback registry over `ssh -R` in [LAN mode](../concepts/deploy-backends.md), or a direct registry pull (the multi-arch list is pushed first) in registry mode.
3. **Convert** the disk in place from the transferred image.
4. **Inject secrets** into the staged deployment's `/etc` — the admin key, the registry pull token, and (in signing mode) the signature-enforcement policy — exactly the file set [disk provisioning](setup.md) bakes.
5. **Reboot**, wait for the host to answer SSH on the new boot as `admin@`, and assert `bootc status`.

Hosts roll out in parallel, capped by `[concurrency] takeover`.

## Afterwards

The host is a normal bootc device with no `bootcher.toml` change needed — `[deploy] remotes` already names it as `admin@host`. Ship it updates with `bootcher deploy`, and roll its credentials with `bootcher rotate`, exactly as for a host that was disk-provisioned.
