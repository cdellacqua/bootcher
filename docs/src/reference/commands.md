# Commands

All subcommands require a `bootcher.toml` in the current directory, except `init` (which creates one).

---

## `bootcher init [name] [-y] [-f]`

Scaffold a new project. `name` is the directory to create (and the image name); omit to scaffold the current directory in place. `-y` accepts defaults without prompting; `-f` allows scaffolding into a non-empty directory.

---

## `bootcher provision [--ssh-key <path>] [--skip-pull-check] [--anonymous] [--skip-build]`

First-time provisioning: collect secrets → build the container → build its disk(s). Leaves each disk artifact under `output/<arch>/<disk_type>/` for you to write to the target device.

| Flag | Purpose |
|---|---|
| `--ssh-key <path>` | Admin SSH private key; the `<path>.pub` sibling is injected into the device. Omit to pick interactively on a TTY |
| `--skip-pull-check` | Skip the eager local pull-token validation |
| `--anonymous` | The configured registry is public: provision with no pull credential, baking no `auth.json` (the device pulls anonymously). On a TTY, leaving the username blank does the same. No effect in LAN mode |
| `--skip-build` | Build the disk from the already-built container, skipping the container build — for iterating on the disk step, or when an earlier `bootcher build` produced the container |

---

## `bootcher deploy [--skip-bootc-upgrade] [--skip-build]`

Subsequent deploy: build the container → push it and trigger `bootc upgrade` on the configured targets. All configuration comes from `bootcher.toml`.

| Flag | Purpose |
|---|---|
| `--skip-bootc-upgrade` | Registry mode only: push the image and return without SSHing into remotes to run `bootc upgrade`. Devices pick up the update on their next auto-update cycle. Has no effect in LAN mode. |
| `--skip-build` | Push the already-built container, skipping the container build — for shipping an image an earlier `bootcher build` produced, or iterating on the push/upgrade step |

---

## `bootcher build`

Build the container image (the `podman build` step) and assemble the per-arch results into a local multi-arch manifest list (`localhost/<name>:latest`). Useful when iterating on the Containerfile before a `--skip-build` `provision`/`deploy`/`takeover`.

---

## `bootcher takeover [--ssh-key <path>] [--login <user>] [--skip-pull-check] [--anonymous] [-y] [--skip-build]`

**Destructively** convert each live `[deploy] remotes` host into a bootc system in place: build the container → per-host `bootc install to-existing-root`. Irreversibly wipes the target's current OS; back it up first. Each host must already have `podman` and `sudo` installed. After it succeeds the host is an ordinary bootc device, so `deploy`/`rotate` take over. See [Takeover](../workflows/takeover.md).

| Flag | Purpose |
|---|---|
| `--ssh-key <path>` | Admin SSH private key: the `<path>.pub` half is installed on the new system, and the private half is the identity for reconnecting as `admin@host` after the reboot. Effectively required; omit only to pick on a TTY |
| `--login <user>` | Fleet-wide default stock cloud login for the initial connection (`debian`/`ubuntu`/`cloud-user`/`root`). Override per host with a remote's `takeover_login`. No safe default — a host with neither is an error |
| `--skip-pull-check` | See `provision` |
| `--anonymous` | See `provision` |
| `-y` / `--yes` | Skip the interactive destructive-action confirmation. Without it, a non-TTY run fails closed |
| `--skip-build` | Convert from the already-built container, skipping the container build — for re-running against more hosts, or when an earlier `bootcher build` produced the image |

---

## `bootcher sign enroll [<prefix>] [--force]`

Generate a cosign/sigstore signing keypair: `<prefix>.key` (private, 0600) and `<prefix>.pub` (public). Then automatically rewrites the `registry` field in `bootcher.toml` to the signing form (inline table pairing the URL with the new key).

**Requires** `bootcher.toml` to already contain a `[deploy] registry` entry (plain URL form). Can be run at any point:

- **Before first `provision`**: the public key is baked into the disk image and signing is active from day one.
- **After devices are already deployed**: run `bootcher rotate sign-key` afterwards to push the public key to each device over SSH, then `bootcher deploy` to start signing images.

| Argument / Flag | Default | Purpose |
|---|---|---|
| `<prefix>` | `cosign` | Output filename prefix (`<prefix>.key` + `<prefix>.pub`) |
| `--force` / `-f` | — | Overwrite an existing `<prefix>.key` |

After running, commit `<prefix>.pub` and run `bootcher provision` / `bootcher deploy`.

**Environment:** `BOOTCHER_SIGN_PASSPHRASE` — passphrase for the new key (also honoured on a TTY, skipping the prompt).

---

## `bootcher sign verify <image> [--pubkey <path>] [--tls-verify <bool>]`

Verify that a registry image carries a valid cosign signature. Exits 0 on success.

| Argument / Flag | Purpose |
|---|---|
| `<image>` | Fully-qualified image reference (e.g. `reg.example.com/org/name:tag`) |
| `--pubkey <path>` | Public key to verify against; defaults to the key from `[deploy] registry` in `bootcher.toml` |
| `--tls-verify` | Default `true`; pass `false` for a plain-HTTP (non-TLS) registry |

---

## `bootcher rotate pull-token [--skip-pull-check]`

Replace the registry pull token on every `[deploy] remotes` device. Registry mode only.

| Flag | Purpose |
|---|---|
| `--skip-pull-check` | Skip the pre-flight local validation (per-device check still runs) |

**Environment:** `BOOTCHER_PULL_USER`, `BOOTCHER_PULL_TOKEN`.

---

## `bootcher rotate ssh-key [--path <path>]`

Replace the admin SSH authorized key on every `[deploy] remotes` device. Lockout-safe: the old key stays valid until the new one is proven. Applies in both LAN and registry mode.

| Flag | Purpose |
|---|---|
| `--path <path>` | New admin SSH private key; the `<path>.pub` sibling is injected and the private key is used to validate the new login before retiring the old key. Omit to pick interactively on a TTY |

---

## `bootcher rotate sign-key [--pubkey <path>]`

Replace the image signing public key on every `[deploy] remotes` device. Registry mode with signing configured only.

| Flag | Purpose |
|---|---|
| `--pubkey <path>` | New signing public key; defaults to the key from `[deploy] registry` in `bootcher.toml` |

---

## `bootcher clean`

Delete the bootcher cache (`~/.cache/bootcher`): the builder VM images (Fedora Cloud base + prepared overlays) created by `builder = "vm"`.
