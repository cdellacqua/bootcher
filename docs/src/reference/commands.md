# Commands

```text
bootcher init [name] | info [--format] | build [--target] | clean      the project

bootcher device status    [--remote…] [--format]
bootcher device provision [--channel ch | --direct] [--target --disk --ssh-key --skip-build …]
bootcher device takeover  [--channel ch | --direct] [--remote… --login --ssh-key …]
bootcher device deploy    [--skip-build] [--remote…]
bootcher device refresh   [--remote…] [--force]
bootcher device switch    (--channel ch | --direct) [--remote…] [--skip-build --anonymous]
bootcher device rotate    ssh-key | pull-token

bootcher release publish X [--skip-build --rebuild --git-tag … --promote ch]
bootcher release refresh X [--force --skip-build]
bootcher release list      [--format]

bootcher channel status  [ch…] [--format]
bootcher channel promote X ch [--allow-stale-base]
bootcher channel refresh [ch…] [--exclude ch --force --skip-build]

bootcher signing enroll [prefix] | rotate [prefix] | retire | verify [ref|name]
```

Every command except `init` needs a `bootcher.toml`. A global `-m`/`--manifest <path>`
loads another manifest instead (e.g. `bootcher --manifest bootcher.ci.toml build`). Only
the manifest is redirected: the build context and every relative path stay anchored to
the working directory. A refresh loads the same relative path from the release commit's
own tree.

## Shared flags

Each common flag means one thing everywhere:

| Flag | Meaning |
|---|---|
| `-y`, `--yes` | Proceed without asking. On a TTY it skips the confirmation; **off a TTY it is required** by every command that touches devices or moves a channel: `device deploy/refresh/switch/takeover/rotate`, `channel promote/refresh` (even with `--skip-remotes`: a moved channel moves the fleet on its next auto-update), `release publish --promote`, and `signing enroll/rotate/retire` when they reach devices. Read-only and publish-only commands don't take it. (`init -y` means "accept the defaults".) |
| `--remote <name>` | Restrict the device step to these remotes (repeatable), named by their destination as written in `remotes` or by their bare host. |
| `--skip-remotes` | Move the channel but skip the device step (`channel promote/refresh`, `release publish --promote`). |
| `--skip-build` | Use the `localhost/<name>:latest-<arch>` images already in local storage, however they got there. The release commands check that each records the right `bootcher.source`. |
| `--target <arch>` | Only where one arch makes sense alone: `build`, `device provision`. |
| `--format <fmt>` | On every read-only command: `json`, or a Go-template subset (`{{ .Field }}`, `{{ json .Field }}`; fields match case-insensitively). `device status` and `release list` apply a template per row. |

## Interactive runs

On a TTY a command only ever asks for what a flag could have supplied, with options read
from live state and annotated (`stable: serves v1.3`, `admin@kiosk-2: on :next`,
`admin@vps3: unreachable`), and the pre-selection equal to what the scripted run would do.
After the prompts it prints the equivalent command (`→ bootcher channel promote v1.4
stable -y`). Esc and Ctrl-C both mean "interrupted" (exit 130). Off a TTY nothing is ever
prompted: a missing value is an error naming the flag.

---

## Project

### `bootcher init [name] [-y] [-f] [--fedora-release <N>]`

Scaffold a project. `name` is the directory to create (its last component is the image
name); omit to scaffold the empty current directory. The questionnaire asks for the
arches and disk types, whether to publish to a registry (then the channels, suggesting
`stable`, and whether to sign), the devices, and the builders. `-y` takes the defaults (no
registry); `-f` scaffolds into a non-empty directory. The `Containerfile` pins one Fedora
release (`--fedora-release` picks it; default: the current one). See
[Project setup](../workflows/setup.md).

### `bootcher info [--format <fmt>]`

The project's resolved facts, offline (it follows the `extend` chain):

| Field | Meaning |
|---|---|
| `{{ .Name }}` | `[general] name` |
| `{{ .Registry }}` | `[registry] url`, or empty |
| `{{ .Image }}` | `<url>/<name>`, or empty |
| `{{ .LocalImage }}` | `localhost/<name>:latest` |
| `{{ .Channels }}` | the declared channels |
| `{{ .Remotes }}` | the configured devices |
| `{{ .Signing }}` | whether `[registry] key` is set |
| `{{ .Arches }}`, `{{ .Targets.x86_64 }}` | target arches; disk types per arch |

### `bootcher build [--target <arch>]`

Build the current checkout into `localhost/<name>:latest-<arch>`, plus the local list
`localhost/<name>:latest` when unscoped. Never publishes. With `--target` it stops at that
arch's member (the producer half of a split CI pipeline).

### `bootcher clean`

Delete the bootcher cache (`~/.cache/bootcher`): the builder VM images.

---

## Devices

### `bootcher device status [--remote <name>…] [--format <fmt>]`

What each remote runs: class (`channel`, `direct`, `foreign`, `unreachable`), origin,
booted and staged digests, the `version` label, and for a channel device the release its
digest belongs to and whether it lags its channel. Unreachable devices are a row, not a
failure. Row fields: `device`, `class`, `channel`, `origin`, `transport`, `signature`,
`arch`, `booted`, `staged`, `version`, `release`, `lags`, `state`, `error`.

### `bootcher device provision [--channel <ch> | --direct] …`

Build disk images for new devices under `output/<arch>/<disk_type>/`.

| Flag | Purpose |
|---|---|
| `--channel <ch>` | Follow this channel: pull its members (by digest, every time) into `localhost/<name>:pull-<ch>-<arch>`, bake the pull credential and (signing on) the trust files, record the channel as the origin. Never builds |
| `--direct` | Build here and record `localhost/<name>:latest`; no pull credential, no trust files. Implied without a `[registry]` |
| `--target <arch>` / `--disk <type>…` | One arch / some of its disk types (`--disk` requires `--target`) |
| `--ssh-key <path>` | Admin private key; `<path>.pub` is baked. Omit to pick on a TTY |
| `--skip-build` | `--direct` only: use the local `latest-<arch>` images |
| `--anonymous`, `--skip-pull-check` | `--channel` only: no pull credential (a public registry); don't check it from here |

With a registry and no mode flag: a TTY asks, off a TTY one is required.

### `bootcher device takeover [--channel <ch> | --direct] --remote <host>… …`

**Destructively** convert live hosts into bootc systems in place (`bootc install
to-existing-root`). Each must already have `podman` and `sudo`. `--remote` names the hosts
(required off a TTY; on a TTY, a checklist with nothing pre-checked); the mode works as
for `provision` (`--channel`: the host pulls the channel, nothing is pushed; `--direct`:
built here and shipped over SSH). `--login <user>` is the stock cloud login for the
initial connection (a remote's `takeover_login` overrides it; prompted on a TTY when
neither is set). `--ssh-key`, `--skip-build`, `--anonymous`, `--skip-pull-check` as for
`provision`. Hosts already converted (they answer as `admin@` with `--ssh-key`) are
skipped. See [Takeover](../workflows/takeover.md).

### `bootcher device deploy [--skip-build] [--remote <name>…] [-y]`

Build this checkout (dirty trees welcome) and push it to the **direct** devices over SSH.
Never moves a device: `--remote` naming a non-direct device is an error pointing at
`device switch --direct`. On a TTY: a checklist of the direct devices, all pre-checked.
See [Direct devices](../workflows/direct.md).

### `bootcher device refresh [--remote <name>…] [--force] [-y]`

Base-image patches for direct devices: rebuild the commit each one booted (read from the
device) on the current base, in a temporary worktree under that commit's config, and
ship it back to the devices on that commit. Commits whose devices all have a current base
are skipped unless `--force` (which also builds `--no-cache`). Devices deployed from a
dirty tree are listed and skipped. See [Base-image patches](../concepts/base-patches.md).

### `bootcher device switch (--channel <ch> | --direct) [--remote <name>…] [-y]`

The only command that moves a device. `--channel`: install a pull credential where
missing (`--anonymous` skips, `--skip-pull-check` skips the local check), the trust files
(signing on), then the channel device step. `--direct`: build (or `--skip-build`) and run
the direct device step. Off a TTY the mode, `--remote` and `-y` are required. See
[Moving devices](../workflows/moving.md).

### `bootcher device rotate ssh-key [--ssh-key <path>] [--remote <name>…] [-y]`

Replace the admin SSH key on every reachable remote, lockout-safe (the old key is retired
only once a fresh login with the new key works). See
[Admin SSH key](../workflows/rotate/admin-key.md).

### `bootcher device rotate pull-token [--skip-pull-check] [--remote <name>…] [-y]`

Replace the pull credential on the channel devices, each verified against its own channel
before it's committed. From `BOOTCHER_PULL_USER` / `BOOTCHER_PULL_TOKEN`, or prompted. See
[Pull token](../workflows/rotate/pull-token.md).

---

## Releases

### `bootcher release publish [X] [--skip-build] [--rebuild] [--git-tag push|local|no] [--promote <ch>]`

Publish release `X` from a clean checkout. Refuses a dirty tree, an invalid name (not an
OCI tag, a declared channel, `sha256-*`, `ci-*`) and a git tag `X` at another commit.
`:X` absent: build every arch (or `--skip-build`: local images recording HEAD), check one
member per `[targets]` arch, annotate, push signed as an OCI index. `:X` already recording
HEAD: nothing to do (`--rebuild` forces it). `:X` recording another commit: refused.
`--git-tag` creates an annotated tag at HEAD after the push (`push` also pushes it; off a
TTY the default is `no`). `--promote <ch>` runs `channel promote X <ch>` afterwards, with
`--remote`, `--skip-remotes` and `-y`. On a TTY with no `X`: a name prompt pre-filled with
the git tag at HEAD. See [Publishing releases](../workflows/releases.md).

### `bootcher release refresh X [--force] [--skip-build]`

Rebuild `X` on the current base from its recorded commit — its stale arches, or every one
with `--force` (`--no-cache`) — and re-push it, keeping the untouched members by digest.
Moves no channel. Needs the signing key with signing on.

### `bootcher release list [--format <fmt>]`

The tags whose index entries carry `bootcher.release`, with their source commit, per-arch base
status and the channels serving each. Row fields: `release`, `digest`, `commit`, `stale`,
`base`, `channels`, `arches`.

---

## Channels

### `bootcher channel status [ch…] [--format <fmt>]`

Per channel (default: all declared): the release it serves, its source commit, per-arch
base current/stale, and whether it lags its release. `--format json` for any selection; a
template (`{{ .Release }}`, `{{ .Commit }}`, `{{ .Stale }}`, `{{ .Lags }}`) only when
exactly one channel is selected — what a scheduled CI refresh gates on.

### `bootcher channel promote [X] [ch] [--allow-stale-base] [--remote…] [--skip-remotes] [-y]`

Point `:ch` at `:X`: a digest-preserving copy, after checking `:X` exists, is signed by
the configured key (signing on) and is on the current base (`--allow-stale-base`
overrides), and that `:ch` doesn't hold a release named `ch`. Prints the previous release.
Then the channel device step on the devices following `ch` — always, even when the channel
didn't move — unless `--skip-remotes`. Needs neither the signing key nor a build
environment. On a TTY, a missing `X` is completed from `release list` and a missing `ch`
picked from an annotated list.

### `bootcher channel refresh [ch…] [--exclude <ch>…] [--force] [--skip-build] [--remote…] [--skip-remotes] [-y]`

Base-image patches for channels: rebuild each served release's stale arches from its own
commit (in a worktree, under that commit's config), re-push it, re-copy it to every
selected channel serving it (skipping one a concurrent promote moved), then the device
step. `--exclude` pauses a channel. `--skip-build` (the CI path): the selected channels
must serve one release, and the local images must record its commit. On a TTY with no
channels: a checklist pre-checking the stale or lagging ones. See
[Base-image patches](../concepts/base-patches.md).

---

## Signing

### `bootcher signing enroll [prefix] [--remote…] [-y]`

Write `<prefix>.key` / `<prefix>.pub` (default `cosign`); on a running fleet, sign every
served release in place and enforce signatures on the reachable channel devices
(unreachable ones are a warning); then set `[registry] key` and bake the install drop-in —
last, so a failed run can be re-run (it reuses the keypair). Needs `-y` off a TTY only
when there is something to sign or a device to update. Refuses when a key is configured.

### `bootcher signing rotate [prefix] [--remote…] [-y]`

New keypair (`cosign-<fingerprint>` by default), new-key signatures on every served
release, old + new keys trusted on the channel devices, then `[registry] key` repointed —
last. After a failure, `rotate <prefix>` with the prefix it printed resumes with that
keypair.

### `bootcher signing retire [--remote…] [-y]`

Trust only the current key on the channel devices and delete the others. Only once every
device has the new key; refuses while anything a channel serves doesn't verify under the
current key.

### `bootcher signing verify <ref|name> [--pubkey <path>]`

Verify a signature the way a device does (a pull under a signature-requiring policy). A
bare release or channel name resolves against the project image; `--pubkey` defaults to
the `.pub` of `[registry] key`. A plain-HTTP registry is declared with `insecure = true` in
`registries.conf`. See [Image signing](../concepts/signing.md).

## Environment

| Variable | Read by |
|---|---|
| `BOOTCHER_PULL_USER`, `BOOTCHER_PULL_TOKEN` | the pull credential: `device provision/takeover/switch --channel`, `device rotate pull-token` |
| `BOOTCHER_SIGN_PASSPHRASE` | the signing passphrase: `release publish/refresh`, `channel refresh`, `signing enroll/rotate` |
