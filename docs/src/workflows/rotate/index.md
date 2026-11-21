# Credential rotation

bootcher can replace on-device credentials — the admin SSH key, the registry pull token, or the image signing key — over the existing SSH channel, without reprovisioning or rebuilding the image.

`bootcher device rotate ssh-key | pull-token`, and the `signing` commands' device steps:

- Target the devices listed in `remotes` — `ssh-key` every reachable one, `pull-token` and the signing commands the channel devices — restricted with `--remote`.
- Ask for confirmation on a TTY, and need `-y` off one.
- Run the fleet in parallel (up to `[concurrency] rotate` workers).
- Attempt every device even when some fail, collecting all errors before returning.
- Run the [`[hooks.rotate]`](../../reference/bootcher-toml.md#hooksphase) `pre`/`post` commands once around the rollout — `pre` after the new credential is collected and checked locally, `post` once every device has been attempted, even if some failed, with each device's outcome in [`results`](../../reference/bootcher-toml.md#bootcher_metadata) (e.g. to revoke the old credential only where the new one landed); the run still fails afterwards if any device did. The hook's `BOOTCHER_METADATA` names the credential in `credential` (`pull-token` / `ssh-key` / `sign-key`).
- Never brick a device: `ssh-key` keeps the old SSH key live until the new one is proven by a fresh login; `pull-token` verifies the token reaches the device's own channel before committing it; `signing rotate` makes devices trust the old and new keys at once, and only `signing retire` drops the old one.

## Subcommands

| Command | What it rotates | Devices |
|---|---|---|
| [`bootcher device rotate pull-token`](pull-token.md) | Registry pull token (`/etc/ostree/auth.json`) | channel devices |
| [`bootcher device rotate ssh-key`](admin-key.md) | Admin SSH authorized key (`/etc/ssh/authorized_keys.d/admin`) | every device |
| [`bootcher signing rotate` / `retire`](sign-key.md) | Trusted signing public keys (`/etc/pki/containers/<name>-<fp8>.pub`) | channel devices |
