# Secrets

bootcher manages two per-deployment secrets. Neither is stored in the manifest, in the container image, or anywhere on the builder's filesystem beyond the provisioning session.

## The two secrets

**Admin SSH authorized key** *(registry and LAN mode)*

The public key placed in `/etc/ssh/authorized_keys.d/admin` on the device. This is the key you use to SSH in and run `bootcher deploy` / `bootcher rotate` commands against the device.

**Registry pull token** *(registry mode only)*

Stored in `/etc/ostree/auth.json` on the device. Used by `bootc upgrade` to pull the project image from the registry. It should be a least-privilege, read-only credential, separate from the push auth managed on the builder.

## When and how they are collected

Both secrets are collected by `bootcher provision` (and `bootcher takeover`) before anything is built. Collection order:

1. **Admin SSH key** — prompted on a TTY from available `~/.ssh/` keys, or supplied via `--key <path>` (private key; the `<path>.pub` sibling is read). In non-interactive mode, the sole available key is picked automatically; multiple candidates without `--key` is an error.

2. **Registry pull token** *(if registry is configured)* — read from `BOOTCHER_PULL_USER` / `BOOTCHER_PULL_TOKEN` if set, else prompted on a TTY for username + token. bootcher pre-validates the token against the registry before building, unless `--skip-pull-check` is passed. If the registry is public, leave the username blank at the prompt (or pass `--anonymous` non-interactively) to provision without a pull token at all — no `auth.json` is baked and the device pulls anonymously.

## How they reach the device

The collected secrets are passed to image-builder in a blueprint as `customizations.files` entries, which inject them directly into the device image's persistent `/etc` at disk-build time.

```
/etc/ssh/authorized_keys.d/admin   ← admin public key (mode 0644)
/etc/ostree/auth.json              ← pull token JSON (mode 0600, registry mode only)
/etc/containers/policy.json        ← signing policy (mode 0644, signing mode only)
```

The ostree `/etc` 3-way merge preserves these files across `bootc upgrade` cycles.

## Rotating secrets after provisioning

Secrets can be changed on running devices without reprovisioning. See [Credential rotation](../workflows/rotate/index.md).
