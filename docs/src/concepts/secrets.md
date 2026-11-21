# Secrets

bootcher manages two per-device secrets. Neither is stored in the manifest, in the
container image, or anywhere on the builder's filesystem beyond the provisioning session.

## The two secrets

**Admin SSH authorized key** *(every device)*

The public key placed in `/etc/ssh/authorized_keys.d/admin` on the device. It's the key
bootcher (and you) log in with for every device step: `device deploy`, `device status`,
the channel device step, `device rotate`.

**Registry pull token** *(channel devices of a private registry)*

Stored in `/etc/ostree/auth.json` on the device. Used by `bootc upgrade` to pull the
channel from the registry. It should be a least-privilege, read-only credential, separate
from the push auth on your workstation or CI. Direct devices carry none.

This token is the one secret that necessarily lives on every channel device you ship,
which makes physical access to any one of them equivalent to holding it: the file is
`0600`, but root on the device — or the disk in someone else's hands — reads it. Scope it
read-only so that what leaks is the ability to *pull* your image and nothing more, and
treat a device that leaves your control as a token to replace with
[`bootcher device rotate pull-token`](../workflows/rotate/pull-token.md).

## When and how they are collected

`bootcher device provision` and `bootcher device takeover` collect them before anything is
built:

1. **Admin SSH key** — `--ssh-key <path>` (a private key; the `<path>.pub` sibling is
   read), or picked from `~/.ssh/` on a TTY.
2. **Registry pull token** *(`--channel` only)* — read from `BOOTCHER_PULL_USER` /
   `BOOTCHER_PULL_TOKEN` if set, else prompted on a TTY, and checked against the registry
   from this machine unless `--skip-pull-check`. For a **public** registry pass
   `--anonymous` (or leave the username blank at the prompt): no `auth.json` is baked and
   the device pulls anonymously.

`device switch --channel` collects one pull token for the run when the devices it moves
have none, and installs it on each after the device proves it works.

## How they reach the device

For a disk, the collected files go to image-builder as blueprint `customizations.files`,
written straight into the disk's persistent `/etc`; a takeover writes the same files into
the staged deployment's `/etc` over SSH:

```
/etc/ssh/authorized_keys.d/admin              ← admin public key (0644)
/etc/ostree/auth.json                          ← pull token (0600, channel installs)
/etc/pki/containers/<name>-<fp8>.pub           ← signing public key (channel installs, signing on)
/etc/containers/policy.json                    ← signature policy (channel installs, signing on)
/etc/containers/registries.d/bootcher-signing.yaml
```

The ostree `/etc` 3-way merge preserves these files across `bootc upgrade` cycles.

## Rotating secrets after provisioning

Secrets can be changed on running devices without reprovisioning. See
[Credential rotation](../workflows/rotate/index.md).
