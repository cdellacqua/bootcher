# Signing key: enroll, rotate, retire

The signing key's lifecycle is three commands, each of which reaches the reachable
**channel** devices (direct devices don't verify signatures; see
[Image signing](../../concepts/signing.md#scope)). Each asks for confirmation on a TTY and
needs `-y` off one; `--remote` restricts the device part. The device part fires
[`[hooks.rotate]`](../../reference/bootcher-toml.md#hooksphase) with `credential =
"sign-key"`; the re-signing of releases fires `[hooks.publish]`.

## Enroll

```sh
bootcher signing enroll [prefix]      # default prefix: cosign
git add cosign.pub bootcher.toml sysroot && git commit -m "sign releases"
```

Writes `<prefix>.key` (private, 0600, git-ignored) and `<prefix>.pub`, sets `[registry]
key`, and bakes `sysroot/usr/lib/bootc/install/30-bootcher-signing.toml`. On a running
fleet it then signs every release a channel serves **in place** (the digest doesn't
change) and runs the channel device step on the channel devices: trust files, then `bootc
switch --enforce-container-sigpolicy`, upgrade, reboot. Refuses when a key is already
configured — that's `rotate`.

Devices outside `remotes` keep an unverified origin until they're re-provisioned or
moved with `device switch --channel`.

## Rotate

```sh
bootcher signing rotate [prefix]      # default prefix: cosign-<fingerprint>
git add cosign-*.pub bootcher.toml && git commit -m "rotate signing key"
```

1. Generate a new keypair.
2. Add a new-key signature to every release a channel serves (no rebuild, digests
   unchanged).
3. Install the old **and** new keys on the channel devices (no reboot: the origin already
   verifies; `policy.json` lists both).
4. Point `[registry] key` at the new key — last, so a failure part-way leaves the config
   on the old key.

There is no window in which any device — listed or not — rejects what its channel
serves: the served releases carry both signatures. From now on releases are signed with
the new key.

## Retire

```sh
bootcher signing retire
```

Makes the channel devices trust only the current key and deletes the other key files.
**Run it only once every device, including ones not in `remotes`, has the new key** — a
device that still only trusts the old one rejects every update from then on, and
`signing retire` can't reach a device it doesn't know about.

Releases no channel serves keep only their old signature. `channel promote` refuses
them; `release refresh X --force` rebuilds and re-signs one with the current key.

## A leaked key

Rotate first (devices then trust both keys), and retire as soon as every device has the
new one: until you retire, a device still accepts an image signed with the leaked key.
