# bootc image

A [bootc](https://bootc-dev.github.io/bootc/) image scaffolded by `bootcher`.
Edit `Containerfile` and the `sysroot/` overlay to taste, then:

```sh
bootcher device provision   # build → disk image under output/ (prompts for your SSH key)
bootcher device deploy      # build → push an update to the direct devices in `remotes`
```

With a `[registry]`, publish releases and move channels instead:
`bootcher release publish v1.0`, `bootcher channel promote v1.0 stable`, and
`bootcher device provision --channel stable` for devices that follow the channel.

`device provision` leaves the disk image under `output/<arch>/` for you to write or
upload to your target. It grows its root filesystem to fill the disk on first
boot.

The target arch and build location live in `bootcher.toml` (the `[targets]` and
`[builder]` tables) — edit them to cross-build or to offload the build.

## What's here

- `Containerfile` — `FROM quay.io/fedora/fedora-bootc:${FEDORA_VERSION}` plus a
  security baseline (admin user, SSH, sudo, firewalld + fail2ban + chrony, bootc
  auto-updates). Yours to extend with `dnf install`s, units, and assets.
  `FEDORA_VERSION` pins the Fedora release: change it to upgrade before the pinned
  one reaches end of life.
- `sysroot/` — files COPYed into the image (`COPY sysroot/ /`): sshd/sudoers drop-ins,
  the firewalld zone, the fail2ban jail, the `admin` userdb record, and
  `derive-userdb.sh`.
- `bootcher.toml` — project manifest. The `[general]` table holds `name` and
  `rootfs` (default `ext4`); the `[targets]` table maps each target arch to its
  disk type(s) (image-builder image type, default `qcow2`); the optional
  `[registry]` table (`url`, `channels`, and `key` with signing on) is where releases
  and channels are published; the `[builder]` table sets where the container build and
  the disk-image step run (`local`, `vm`, or an ssh destination). The top-level
  `remotes = ["admin@host", …]` lists the devices bootcher reaches over SSH (what each
  follows — direct or a channel — is read from the device). A remote needing extra ssh
  args takes the object form — `{ remote = "admin@host", ssh_opts = ["-i", "/path/to/key"] }`.

## Admin login

The `admin` user (uid 1000, passwordless `sudo` via `wheel`, `fish` shell) logs
in over SSH by key only. sshd reads authorized keys from
`/etc/ssh/authorized_keys.d/admin`, which `bootcher device provision` bakes into the
disk image's persistent `/etc` — **never into the OCI container image**. Rotate it
with `bootcher device rotate ssh-key`.

## Image signing

Optionally sign every release with a [cosign](https://docs.sigstore.dev/) key so
channel devices **reject unsigned or tampered images** on `bootc upgrade` (direct
devices' trust is the SSH channel). With a `[registry]` set:

```sh
bootcher signing enroll       # cosign.key (private, git-ignored) + cosign.pub, [registry] key
```

It also bakes `sysroot/usr/lib/bootc/install/30-bootcher-signing.toml`
(`enforce-container-sigpolicy = true`), so every install records a verifying origin, and
on a running fleet signs what the channels serve and enforces signatures on the channel
devices. Rotate with `bootcher signing rotate` (devices trust old and new keys, no
window of rejected upgrades), then `bootcher signing retire` once every device has the
new key.

## Adding users

Drop `<user>.user` + `<user>.group` into `sysroot/usr/lib/userdb/` and a
`home-<user>.conf` into `sysroot/usr/lib/tmpfiles.d/`, then `RUN
/usr/libexec/derive-userdb.sh` after `COPY sysroot/ /` (it generates the
nss-systemd reverse-lookup symlinks and membership markers). SSH keys are *not*
part of the user record — see Admin login above.
