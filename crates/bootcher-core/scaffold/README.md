# bootc image

A [bootc](https://bootc-dev.github.io/bootc/) image scaffolded by `bootcher`.
Edit `Containerfile` and the `sysroot/` overlay to taste, then:

```sh
bootcher provision    # build → disk image under output/ (prompts for your SSH key)
bootcher deploy       # build → push an update to the configured targets
```

`provision` leaves the disk image under `output/<arch>/` for you to write or
upload to your target. It grows its root filesystem to fill the disk on first
boot.

The target arch and build location live in `bootcher.toml` (`platform` and the
`[builder]` table) — edit them to cross-build or to offload the build.

## What's here

- `Containerfile` — `FROM quay.io/fedora/fedora-bootc:latest` plus a security
  baseline (admin user, SSH, sudo, firewalld + fail2ban + chrony, bootc
  auto-updates). Yours to extend with `dnf install`s, units, and assets.
- `sysroot/` — files COPYed into the image (`COPY sysroot/ /`): sshd/sudoers drop-ins,
  the firewalld zone, the fail2ban jail, the `admin` userdb record, and
  `derive-userdb.sh`.
- `bootcher.toml` — project manifest. The `[general]` table holds `name`,
  `platform`, the disk-image `disk_type` (image-builder image type, default
  `qcow2`) and `rootfs` (default `ext4`), and an optional `registry` (set it for
  registry-mode deploys); the `[builder]` table sets where the container build
  and the disk-image step run
  (`local`, `vm`, or an ssh destination — `[user@]host`, or
  `ssh://[user@]host[:port]` for a non-default port). Cross-arch, `local` builds
  in-process under qemu-user emulation (slower than native); a native-arch
  `remote` runs with no emulation and is by far the fastest; a `vm` is a clean
  local sandbox needing no second machine, but cross-arch it runs under
  full-system emulation, slower still. The `[deploy]` table's
  `remotes = ["user@host", …]` lists the
  targets `deploy` / `upgrade` ship to (required for LAN, optional in registry
  mode). A remote needing extra ssh args takes the object form instead —
  `{ remote = "user@host", ssh_opts = ["-i", "/path/to/key"] }`.

## Admin login

The `admin` user (uid 1000, passwordless `sudo` via `wheel`, `fish` shell) logs
in over SSH by key only. sshd reads authorized keys from
`/etc/ssh/authorized_keys.d/admin`, which `bootcher provision` bakes into the
disk image's persistent `/etc` — **never into the OCI container image**. Rotate it
by re-provisioning, or by editing that file on the device over SSH.

## Image signing (registry mode)

Optionally sign pushed images with a [cosign](https://docs.sigstore.dev/) key so
devices **reject unsigned or tampered images** on `bootc upgrade`. It's opt-in and
registry-mode only (a LAN deploy's integrity already rides the ssh channel). To
enable it:

```sh
bootcher sign enroll          # writes cosign.key (private, git-ignored) + cosign.pub
```

Then expand `[deploy] registry` in `bootcher.toml` from a plain URL to the signing form:

```toml
[deploy]
registry = { url = "<your-registry>", key = "cosign.key" }
```

and create `sysroot/usr/lib/bootc/install/30-bootcher-signing.toml` so a freshly
provisioned device enforces signatures from first boot (`bootcher init` writes
this for you when you enable signing during setup):

```toml
[install]
enforce-container-sigpolicy = true
```

With this set, `deploy`/`upgrade` sign the pushed multi-arch image, and
`provision` bakes the public key and a signature-requiring
`/etc/containers/policy.json` into the disk image. `deploy` points the device's bootc
origin at the verifying `ostree-image-signed:` scheme.

If the signing key leaks or expires, rotate it without reprovisioning:

```sh
bootcher sign enroll newcosign        # new keypair (newcosign.key + newcosign.pub)
# update [deploy] registry key to "newcosign.key" in bootcher.toml, then:
bootcher rotate sign-key              # push newcosign.pub to every device
bootcher deploy                       # re-sign + push with the new key
```

Note: between `rotate sign-key` and `deploy`, devices will fail `bootc upgrade` (the
registry still carries the old signature). They keep running their current image; SSH
access is unaffected. The fleet recovers as soon as `deploy` runs.

## Adding users

Drop `<user>.user` + `<user>.group` into `sysroot/usr/lib/userdb/` and a
`home-<user>.conf` into `sysroot/usr/lib/tmpfiles.d/`, then `RUN
/usr/libexec/derive-userdb.sh` after `COPY sysroot/ /` (it generates the
nss-systemd reverse-lookup symlinks and membership markers). SSH keys are *not*
part of the user record — see Admin login above.
