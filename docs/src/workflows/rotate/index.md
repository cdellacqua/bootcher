# Credential rotation

bootcher can replace on-device credentials — the admin SSH key, the registry pull token, or the image signing key — over the existing SSH channel, without reprovisioning or rebuilding the image.

All `rotate` subcommands:

- Target the `[deploy] remotes` devices listed in `bootcher.toml`.
- Run the fleet in parallel (up to `[concurrency] rotate` workers).
- Attempt every device even when some fail, collecting all errors before returning.
- Run the [`[hooks.rotate]`](../../reference/bootcher-toml.md#hooksphase) `pre`/`post` commands once around the rollout — `pre` after the new credential is collected and checked locally, `post` only once every device has succeeded. The hook's `BOOTCHER_METADATA` names the subcommand in `credential` (`pull-token` / `ssh-key` / `sign-key`).
- Never brick a device: `rotate key` keeps the old SSH key live until the new one is proven by a fresh login; `rotate pull-token` verifies the token can actually pull before committing it. The only exception is `rotate sign-key`, which writes directly without a post-write check — verifying a new signing key would require pulling and staging an image signed with it, which is expensive and only makes sense when a new image is actually ready to deploy; a bad key only blocks future upgrades, SSH access is always unaffected and can be used to rotate the signing key again.

## Subcommands

| Command | What it rotates | Mode |
|---|---|---|
| [`bootcher rotate pull-token`](pull-token.md) | Registry pull token (`/etc/ostree/auth.json`) | Registry only |
| [`bootcher rotate ssh-key`](admin-key.md) | Admin SSH authorized key (`/etc/ssh/authorized_keys.d/admin`) | Both |
| [`bootcher rotate sign-key`](sign-key.md) | Image signing public key (`/etc/pki/containers/<name>.pub`) | Registry + signing only |
