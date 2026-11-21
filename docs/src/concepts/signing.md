# Image signing

bootcher supports opt-in cosign/sigstore signing. With it on, every release is signed when
it's published, `channel promote` refuses a release the configured key doesn't verify,
and every channel device verifies the signature on `bootc upgrade`, rejecting tampered or
unsigned images.

## Scope

Signatures protect **registry pulls**, so it's **channel devices** that verify. A direct
device's `containers-storage` origin is unverified: its trust is the SSH channel `device
deploy` ships over, exactly as it always was.

Enforcement lives in the **image**: `signing enroll` bakes
`sysroot/usr/lib/bootc/install/30-bootcher-signing.toml`
(`enforce-container-sigpolicy = true`), so every install of the image — a disk or a
takeover, channel or direct — records a signature-enforcing origin. What differs is the
**trust files** (the public key, a `policy.json` requiring a signature, and the
registries.d drop-in that fetches signatures):

- **Channel installs** (`device provision --channel`, `device takeover --channel`) bake
  them, so the device verifies from its very first boot.
- **Direct installs** bake none. They don't need any: the disk installs and boots with a
  verifying origin and the stock policy, and the first `device deploy` re-pins it to an
  unverified `containers-storage` origin. Until that first deploy, the device's
  auto-update timer fails harmlessly — there's no registry to pull from.
- **Every channel device step** installs or refreshes the trust files *before* `bootc
  switch --enforce-container-sigpolicy`, which bootc refuses with the stock permissive
  default policy.

## Turning it on

```sh
bootcher signing enroll        # cosign.key (0600, git-ignored) + cosign.pub, [registry] key, the drop-in
git add cosign.pub bootcher.toml sysroot && git commit -m "sign releases"
```

`signing enroll` refuses if a key is already configured (use `rotate`). On a new project,
enroll before the first publish or provision. On a **running fleet** it also, after the
local setup:

1. signs every release a channel serves **in place** (`skopeo copy --all
   --preserve-digests --sign-by-sigstore-private-key` onto itself) — adding a signature
   leaves the digest unchanged, so nothing is re-pulled;
2. runs the channel device step on the reachable channel devices: trust files, then
   `bootc switch --enforce-container-sigpolicy`.

Devices **outside `remotes`** can't be reached, so they keep an unverified origin until
they're re-provisioned or switched.

## Rotating the key

```sh
bootcher signing rotate        # 1. new keypair   2. sign served releases   3. trust old+new   4. repoint the config
git add cosign-*.pub bootcher.toml && git commit -m "rotate signing key"
# … once every device — including any not in `remotes` — trusts the new key:
bootcher signing retire
```

`signing rotate` (new key named `cosign-<fingerprint>` unless you pass a prefix):

1. generates the new keypair;
2. adds a new-key signature to every release a channel serves (the old signature stays,
   the digest doesn't change);
3. installs the old **and** new keys on the channel devices (`policy.json` lists both
   under `keyPaths`);
4. points `[registry] key` at the new key — **last**, so a failure part-way leaves the
   config on the old key.

No rebuild, no channel move, and no window in which any device — listed or not — rejects
what its channel serves: an old-key device accepts the old signature, a new-key one the
new. New releases are signed with the new key only.

`signing retire` makes the channel devices trust only the current key and deletes the
other key files. Run it only once every device has the new key: a device outside
`remotes` that still has only the old one would reject every update from then on.
Releases no channel serves keep only their old signature; `channel promote` refuses
them, pointing at `release refresh X --force` (re-signed with the current key).

## Passphrase

On a TTY bootcher prompts for the signing passphrase; in CI set
`BOOTCHER_SIGN_PASSPHRASE`. The env var wins even when empty (cosign permits an empty
passphrase). Only the commands that sign need the key and its passphrase: `release
publish|refresh`, `channel refresh`, and `signing enroll|rotate`. `channel promote` only
needs the public key.

## Verifying

```sh
bootcher signing verify stable                 # a channel or release of this project
bootcher signing verify reg.example.com/org/x:v1 --pubkey cosign.pub
```

`signing verify` pulls the image under a policy that requires a signature from the key —
the check a device performs. A plain-HTTP registry is declared the usual way, `insecure =
true` in `registries.conf`.

## On-device files

| Path | Purpose |
|---|---|
| `/etc/pki/containers/<name>-<fp8>.pub` | A trusted public key, named by its fingerprint prefix so several can coexist during a rotation |
| `/etc/containers/policy.json` | `default: reject`, and a sigstore signature from one of the keys (`keyPaths`) for the project's OS image; other images pull unsigned |
| `/etc/containers/registries.d/bootcher-signing.yaml` | Enables `use-sigstore-attachments`, so the signature is fetched alongside the image |
