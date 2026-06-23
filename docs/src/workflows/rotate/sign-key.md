# Signing key enroll / rotation

Writes the image-signing config to each device (`/etc/pki/containers/<name>.pub`, `/etc/containers/policy.json`, and the `registries.d` sigstore-attachment drop-in) without reprovisioning. Use this when:

- **Enrolling signing** on a fleet provisioned before signing was enabled — run this, then `bootcher deploy` to switch the bootc origin to the verifying transport.
- **Rotating the signing key** when the cosign key has leaked or is expiring.

*Registry mode with signing configured only.*

## Command

```sh
bootcher rotate sign-key
```

By default, reads the public key from `[deploy] registry` in `bootcher.toml` (the `.pub` sibling of the `key` field). Pass `--pubkey` to rotate to a different key:

```sh
bootcher rotate sign-key --pubkey new-cosign.pub
```

### Flags

| Flag | Purpose |
|---|---|
| `--pubkey <path>` | New signing public key; defaults to the key from `[deploy] registry` in `bootcher.toml` |

## Enrolling signing on a running fleet

Use this when devices were provisioned before signing was enabled (no `key` in `[deploy] registry`).

```
1. Generate a keypair and update bootcher.toml
   bootcher sign enroll                 # writes cosign.key / cosign.pub, patches [deploy] registry

2. Push the signing config to every device
   bootcher rotate sign-key             # writes pubkey + policy.json + registries.d

3. Push a signed image and flip the bootc origin
   bootcher deploy                      # --enforce-container-sigpolicy applied per device
```

After step 3, each device's bootc origin is `ostree-image-signed` and every subsequent upgrade verifies the signature.

During the window between steps 2 and 3, devices' `bootc upgrade` behaviour is unchanged (the origin is still unverified and the registry image is still unsigned). SSH access is unaffected throughout.

## Full rotation workflow

Use this when the cosign signing key has leaked or is expiring.

```
1. Generate a new keypair and update bootcher.toml
   bootcher sign enroll --force        # overwrites cosign.key / cosign.pub, patches [deploy] registry
   # or with a new prefix:
   bootcher sign enroll --prefix cosign-new   # writes cosign-new.key / cosign-new.pub, patches [deploy] registry

2. Push the new public key to every device
   bootcher rotate sign-key            # reads cosign-new.pub from [deploy] registry

3. Next deploy re-signs with the new key
   bootcher deploy
```

## What happens during the interim (steps 2 → 3, rotation only)

After `rotate sign-key` and before the next `deploy`:

- **`bootc upgrade` on devices will fail** — the registry still carries the image signed with the old key, but the device's policy now requires a signature from the new key.
- **Devices keep running their current image** — a failed upgrade leaves the running system untouched.
- **SSH access is unaffected** — admin key rotation is independent of signing key rotation.

The fleet is always recoverable: once `bootcher deploy` runs, the image is signed with the new key and devices accept it on the next upgrade attempt.
