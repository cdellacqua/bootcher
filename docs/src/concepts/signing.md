# Image signing

bootcher supports opt-in cosign/sigstore image signing in registry mode. When enabled, every `deploy` signs the pushed image with a project-managed private key, and every device verifies the signature on `bootc upgrade`, rejecting tampered or unsigned images.

Signing is opt-in: run `bootcher sign enroll` once in a project that already has `[deploy] registry` set to a plain URL. It writes the keypair and rewrites that field to an inline table that pairs the URL with the new key — no manual TOML editing required. It has no effect in LAN mode (LAN integrity rides the trusted SSH channel).

## Signing key lifecycle

```
┌─ SETUP (once) ──────────────────────────────────────────────────┐
│  opt-in using `bootcher sign enroll`                            │
│    ├─ cosign.key          private, 0600, gitignored             │
│    ├─ cosign.pub          public, safe to commit                │
│    └─ bootcher.toml       gets registry.key = "cosign.key"      │
└───────────────┬─────────────────────────────────────────────────┘
                │ cosign.pub will be injected at provision
                ▼
┌─ PROVISION (per disk-image) ────────────────────────────────────┐
│  bootcher provision                                             │
│    ├─ /etc/pki/containers/<name>.pub  ← cosign.pub              │
│    ├─ /etc/containers/policy.json     ← require sigstore        │
│    └─ bootc origin                    ← ostree-image-signed:    │
└───────────────┬─────────────────────────────────────────────────┘
                │
                ▼
┌─ DEPLOY (each release) ─────────────────────────────────────────┐
│  bootcher deploy                                                │
│    ├─ push image to registry                                    │
│    └─ sign with cosign.key                                      │
└───────────────┬─────────────────────────────────────────────────┘
                │ signed image will be pulled from the registry
                ▼
┌─ VERIFY ────────────────────────────────────────────────────────┐
│  bootc upgrade (device)                                         │
│    └─ verify against cosign.pub — reject if invalid             │
└─────────────────────────────────────────────────────────────────┘
```

## Passphrase

On a TTY run, bootcher prompts for the signing passphrase; in CI set `BOOTCHER_SIGN_PASSPHRASE`. The env var wins over the prompt even when empty (cosign permits an empty passphrase).

## Enrolling signing on a running fleet

If devices were provisioned before signing was enabled (i.e. without a `key` in `[deploy] registry`), you can enroll them without reprovisioning:

1. Generate a keypair: `bootcher sign enroll`. The TOML is updated automatically.
2. Push the signing config to every running device: `bootcher rotate sign-key`.
3. Run `bootcher deploy` — it pushes a signed image and switches each device's bootc origin to the verifying transport (`--enforce-container-sigpolicy`). From this point on every upgrade verifies the signature.

## Rotating the signing key

If the signing key is leaked or expiring:

1. Generate a new keypair: `bootcher sign enroll --force` (or `--prefix new-name` to change the key file prefix). The TOML is updated automatically in both cases.
2. Push the new public key to every running device: `bootcher rotate sign-key`.
3. The next `bootcher deploy` re-signs with the new key; devices accept it immediately.

During the window between step 2 and step 3, devices attempting a `bootc upgrade` will fail signature verification (the registry still carries the old signature). They keep running their current image; SSH access is unaffected. The fleet is always recoverable.

See [Signing key rotation](../workflows/rotate/sign-key.md) for the full workflow.

## On-device files

| Path | Purpose |
|---|---|
| `/etc/pki/containers/<name>.pub` | The cosign public key the policy verifies against |
| `/etc/containers/policy.json` | Requires a valid sigstore signature for the project's registry namespace |
| `/etc/containers/registries.d/bootcher-<name>.yaml` | Enables `use-sigstore-attachments` so the signature is fetched alongside the image |
