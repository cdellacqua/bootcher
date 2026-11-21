# bootcher.toml

The project manifest. Its presence marks a directory as a bootcher project. All paths in the manifest are relative to the project root (the directory containing `bootcher.toml`).

## Editor validation & autocomplete

`bootcher init` writes the generated JSON Schemas into a `schemas/` directory beside the manifest, and binds the manifest to its schema with a directive on its first line:

```toml
#:schema ./schemas/bootcher.schema.json
```

This is the [Taplo](https://taplo.tamasfe.dev/) `#:schema` directive, honoured by the **Even Better TOML** VS Code extension (and the Taplo CLI/LSP). With it, your editor validates keys and values, completes table/property names, completes the closed-value fields (the `[targets]` arch keys and type values, `rootfs`), and shows the field docs on hover. The relative path resolves against the manifest's own directory, so the schema needs no hosting — commit the `schemas/` directory alongside `bootcher.toml` so collaborators get the same support without installing bootcher.

`init` also writes `schemas/metadata.schema.json` — the schema for the [`BOOTCHER_METADATA`](#bootcher_metadata) object hooks receive. It binds to nothing (it's reference docs for hook/recipe authors), but a recipe written in a typed language can validate against it.

Both schemas are generated from the same types bootcher uses at runtime, so they can't drift from what it actually accepts. The directive is bootcher- and Taplo-specific; other TOML tooling simply ignores it.

## Annotated example

```toml
#:schema ./schemas/bootcher.schema.json
remotes = []               # devices: "admin@host" or { remote = "...", ssh_opts = [...] }

[general]
name = "my-os"             # image name; used as the container tag
rootfs = "ext4"            # root filesystem (default: ext4)

[targets]                   # the build matrix: each target arch → disk type(s)
x86_64 = "qcow2"           # one type, or a list: ["qcow2", "bootc-installer"]

[builder]
build = "local"            # container-build backend: "local", "vm", "[user@]host" or a rich { type = "remote", host = "user@build-host", ssh_opts = ["-i", "/path/to/key"] }
image = "local"            # disk-image (image-builder) backend

# [builder.aarch64]          # per-arch override: route cross-arch image-builder to a VM
# image = "vm"

# [registry]               # optional: publish releases, move channels
# url = "registry.example.com/org/project"
# channels = ["stable", "next"]   # required with a registry
# key = "cosign.key"       # optional: sign releases (set by `bootcher signing enroll`)

# [concurrency]
# build   = 2              # max parallel container builds
# disk    = 1              # max parallel image-builder runs
# upgrade = 4              # max parallel per-device upgrade SSHes
# rotate  = 4              # max parallel per-device rotate SSHes
# takeover = 2             # max parallel per-host takeover rollouts

# [hooks.build]
# pre  = "sh prepare.sh"
# post = "sh cleanup.sh"

# [hooks.disk]
# pre  = "sh pre-disk.sh"
# post = "dd if=output/x86_64/qcow2/disk.qcow2 of=/dev/sdX"

# [hooks.publish]
# post = "sh notify-release.sh"

# [hooks.promote]
# post = "sh announce-rollout.sh"

# [hooks.upgrade]
# pre  = "sh drain.sh"
# post = "sh smoke-test.sh"

# [hooks.takeover]
# pre  = "sh snapshot-hosts.sh"
# post = "sh smoke-test.sh"

# [hooks.rotate]
# pre  = "sh store-credential.sh"
# post = "sh revoke-old-credential.sh"
```

---

## `extend` *(top-level, optional)*

Inherit from another manifest. The named file (a path relative to *this* manifest's directory) is loaded first and this file's keys are layered over it. Use it to keep a pipeline-specific variant beside the base `bootcher.toml` without restating it — paired with the global `--manifest` flag:

```toml
# bootcher.ci.toml
extend = "bootcher.toml"

[builder]
build = "vm"          # CI builds in a VM; everything else inherited from bootcher.toml
```

```sh
bootcher --manifest bootcher.ci.toml device provision --channel stable
```

Merge semantics:

- **Tables merge recursively** — `[builder] build = "vm"` above overrides only that one key; the base's `image`, `[general]`, `[registry]`, `remotes`, etc. are inherited intact.
- **Scalars and arrays replace wholesale** — a child that sets `x86_64 = ["qcow2"]` replaces the base's list rather than appending to it.

`extend` chains are followed (a parent may itself `extend` a grandparent, applied underneath) and cycles are rejected. Only the manifest is redirected: the build context and every other relative path (Containerfile, `sysroot/`, keys) stay anchored to the working directory, so a variant layers over the same project tree. `extend` is resolved at load time and never appears in a serialized manifest.

A refresh builds an older release commit under **that commit's** manifest, loaded from the same relative `--manifest` path (and, for a project below the top of its git repository, the same subdirectory) in a temporary worktree of the commit under `.git/bootcher-worktrees/`, with normal validation and no merging with today's config. Unknown tables (an old `[deploy]`, say) are ignored rather than rejected.

---

## `[general]`

### `name` *(required)*

The image name. Used as:
- The local container tags: `localhost/<name>:latest-<arch>` and `localhost/<name>:latest`
- The registry repository: `<url>/<name>`, whose tags are the releases and channels
- The on-device signing key files: `/etc/pki/containers/<name>-<fp8>.pub`

Renaming it makes the devices that followed the old name foreign: bring them back with `device switch`.

### `rootfs`

Root filesystem format (`--bootc-default-fs`). Default: `ext4`. Also accepts `xfs` and `btrfs`.

---

## `[targets]`

The build matrix: each target architecture mapped to the image-builder disk type(s) to build for it. The present keys *are* the architectures the project builds — there is no separate `platform` key. Each value is a bare type or an array of them, and the artifacts land under `output/<arch>/<disk_type>/disk.<ext>`.

```toml
[targets]
x86_64  = ["qcow2", "bootc-installer"]   # a VM image and an installer ISO
aarch64 = "raw"                          # a raw image for an edge device
```

The two axes are independent. **Architecture** is the container/registry axis: builds and releases fan out over the distinct arches, and a multi-arch project (more than one key) publishes one multi-arch manifest list. **Disk type** is a per-arch artifact axis only the `disk`/`provision` step fans out over — an arch listing several types reuses its single container build to render each, and disk types never enter the manifest list (an installer ISO isn't something a device `bootc upgrade`s to).

If `[targets]` is omitted, it defaults to the host architecture mapped to a single `qcow2` (falling back to `x86_64` on unrecognised hosts).

`build` and `provision` can build a subset of this matrix without editing the manifest, so a CI pipeline can fan the arches out across native runners instead of cross-building under emulation:

- `device provision --target <arch>` restricts the run to one architecture, and `--disk <type>` (repeatable, requires `--target`) further restricts it to specific disk types of that arch — e.g. one provision job per arch on a per-arch CI runner.
- `build --target <arch>` builds just that arch's container and stops at the per-arch `localhost/<name>:latest-<arch>` member, **without** assembling the multi-arch list. That's the producer half of a split pipeline: build each arch on its own runner in parallel, hand the members over, then one `release publish X --skip-build` assembles and pushes the release — the only job that needs the signing key.

Either way the value must name an arch / type the manifest actually lists.

Accepted type values:

| Value | Artifact |
|---|---|
| `qcow2` *(default)* | QEMU/KVM disk, runs as-is under qemu/libvirt and most clouds |
| `raw` | Raw block-device image; write with `dd` |
| `bootc-installer` | Anaconda-based installer ISO (requires the container to ship the installer payload) |
| `ami` | Amazon Machine Image |
| `vhd` | Azure / Hyper-V virtual hard disk |
| `gce` | Google Compute Engine tarball |
| `vmdk` | VMware/vSphere disk |

> **Note** — `bootc-installer` is not a drop-in for the disk types. Unlike the disk
> formats (which work from any bootc container), it builds an Anaconda-based
> installer ISO and requires the **container itself to ship the installer payload**
> — Anaconda plus the ISO build tools, and the kickstart wiring described in the
> [image-builder bootc ISO docs](https://osbuild.org/docs/bootc/). A stock
> `FROM fedora-bootc` image will *not* produce a working installer without those
> additions. (It replaces the predecessor's `anaconda-iso`, which `image-builder`
> no longer accepts for bootc inputs.)

---

## `[builder]`

### `build` / `image`

Builder spec for the container build (every building command) and the image-builder disk-image step (the disk step of `device provision`) respectively.

Each value is either a **bare-string shorthand** for the no-opts common case, or an **explicit table** tagged by `type` when it carries extra `ssh_opts` and/or `podman_opts`. Each `type` exposes only the fields valid for it (`host`/`ssh_opts` live on `remote` alone).

| Value | Meaning |
|---|---|
| `"local"` *(default)* | Shorthand: run in-process on the build host |
| `"vm"` | Shorthand: a throwaway local QEMU VM matching the target arch |
| `"[user@]host"` or `"ssh://[user@]host[:port]"` | Shorthand: a remote native-arch machine over SSH |
| `{ type = "local", podman_opts = […] }` | In-process, with extra `podman` flags; see [`podman_opts`](#podman_opts) |
| `{ type = "vm", podman_opts = […] }` | Local VM, with extra `podman` flags |
| `{ type = "remote", host = "…", ssh_opts = […], podman_opts = […] }` | Remote with extra ssh args (identity file, port, …) and/or `podman` flags |

**Which backend when** — the tradeoffs differ sharply by whether you're building for the host arch or cross-arch, and `bootcher init` defaults/recommends accordingly:

| | `local` | `vm` | remote |
|---|---|---|---|
| **Same arch** | *Recommended.* Fastest — runs in-process. Needs `sudo` on this host for the image-builder step. | A clean sandbox, but needs qemu + KVM (`/dev/kvm` accessible) to run at a usable speed. | Offloads to another machine; only worth it if this host can't build. |
| **Cross arch** | Container build works but runs slowly under emulation; the **image-builder step is slow and fragile** under emulation — avoid for `image`. | Safe everywhere, but **very slow** — emulated (no KVM for a foreign arch), so a full build can take hours for *either* role. | *Recommended when available.* A native-arch host runs at full speed with none of the emulation hazards. |

So: a cross-arch `image` step on a host with no same-arch remote means `vm` (slow but reliable); with a same-arch remote, prefer the remote. For same-arch, stay on `local` unless you specifically want the VM's isolation.

When a remote builder needs ssh options that the user's `~/.ssh/config` doesn't cover, use the explicit `type = "remote"` table:

```toml
[builder]
build = { type = "remote", host = "user@build-host", ssh_opts = ["-i", "/path/to/key"] }
image = "vm"
```

Or the equivalent expanded form with dotted keys:

```toml
[builder]
build.type     = "remote"
build.host     = "user@build-host"
build.ssh_opts = ["-i", "/path/to/key"]
image          = "vm"
```

### `podman_opts`

Extra flags spliced **verbatim** into this builder's `podman` invocation, available on every `type`. They're scoped by where the spec sits in the table — so the same underlying `podman` command can take different flags per role, per backend, and per arch:

- on a `build` spec → the container build's `podman build`;
- on an `image` spec → the privileged image-builder `podman run`.

A `local` (or `vm`) builder carries `podman_opts` via its own typed table:

```toml
[builder]
build = { type = "local", podman_opts = ["--network=host"] }      # local build on the host network
image = { type = "remote", host = "user@build-host", podman_opts = ["--network=host"] }
```

This is an escape hatch for environments where podman's defaults don't fit — e.g. `--network=host` where per-container networking can't be set up (a runner with a broken iptables→nftables shim or missing kernel features). The flags are passed through unchecked: one that clashes with a flag bootcher already sets (`--arch`, `--privileged`, …) is your responsibility, and `podman build` and `podman run` accept different flag sets, so put each on the matching role.

Because the spec is per-arch (see below), this is also how you give a **cross-arch** build flags its same-arch sibling doesn't need:

```toml
[builder.aarch64]
build = { type = "local", podman_opts = ["--network=host"] }   # only the emulated aarch64 build
```

> **Note:** `bootcher.toml` is committed and shared across machines, so flags here apply everywhere the project builds (local dev included). Keep `podman_opts` to flags that are genuinely intrinsic to that builder/arch; for a flag that only one *environment* needs (e.g. one CI runner), prefer an environment-specific override outside the manifest.

### `[builder.<arch>]`

Per-arch override. Either `build` or `image` (or both) may be set to override the flat default for that architecture only. Unset roles inherit the flat default.

```toml
[builder.aarch64]
image = "vm"    # route aarch64 image-builder to a VM; aarch64 container build stays local
```

---

## `remotes` *(top-level)*

The devices bootcher reaches over SSH: `device status`, `deploy`, `refresh`, `switch`,
`takeover`, `rotate`, and the device steps of `channel promote`/`refresh` and `signing`.
Each entry is either a bare connection string or an object with per-device options:

```toml
remotes = [
  "admin@192.168.1.42",
  { remote = "admin@192.168.1.43", ssh_opts = ["-o", "StrictHostKeyChecking=accept-new"] },
  { remote = "admin@vps.example.com", takeover_login = "debian" },
]
```

Being a top-level array, `remotes` must come before the first `[table]` in the file.
The connection string is anything `ssh` accepts as a destination: `[user@]host` or
`ssh://[user@]host[:port]`. On the command line, `--remote` names a device by its full
destination or its bare host.

There is no per-device mode or channel: what each device follows is read live from the
device ([Direct and channel devices](../concepts/devices.md)). Devices you can't reach
(an edge fleet behind NAT) just aren't listed: channel devices update themselves.

The object form accepts these per-device keys:

| Key | Purpose |
|---|---|
| `ssh_opts` | Extra `ssh` args prepended to every connection (identity file, port, host-key policy, …) |
| `takeover_login` | The stock cloud login (`debian`/`ubuntu`/`cloud-user`/`root`) [`bootcher device takeover`](../workflows/takeover.md) uses for its *initial* connection, before the image's `admin` user replaces it. Per-host override of `--login`. Unused outside takeover — the steady-state identity stays `admin@`, so after takeover this is an ordinary remote |

#### Host-key verification

bootcher connects with `-o StrictHostKeyChecking=accept-new`, because a device that was just provisioned has no entry in `known_hosts` yet and a strict policy would refuse the first connection outright. This is **trust on first use**: the first connection to a given host accepts whatever key it presents and pins it, and every later connection is verified against that pin. A mismatch afterwards fails loudly.

The exposure is therefore limited to that first connection — but on a network where someone could be impersonating the device, that is enough to hand them the session. If you provision over a network you don't control, pin the key yourself instead: read it off the device locally (`ssh-keyscan` on the bench, or the host key printed in the console log), add it to `known_hosts`, and set a strict policy for that remote:

```toml
remotes = [
  { remote = "admin@vps.example.com", ssh_opts = ["-o", "StrictHostKeyChecking=yes"] },
]
```

`ssh_opts` is prepended to every connection bootcher makes to that host, so the override covers every device command alike.

---

## `[registry]` *(optional)*

Where releases are published and channels moved. Without it every device is direct.

```toml
[registry]
url = "registry.example.com/org/project"
channels = ["stable", "next"]
key = "cosign.key"
```

### `url` *(required)*

The registry namespace: the image is `<url>/<name>`, and every release and channel is a
tag on it. Not a credential — safe to commit. Push auth is `podman login` on your
workstation or CI; a channel device's read-only pull token is collected at provision.

### `channels` *(required)*

The channels devices can follow — mutable tags (`<url>/<name>:<channel>`), each a copy of
some release, moved by `channel promote`. Required and non-empty; each must be a valid OCI
tag and may not start with `sha256-` or `ci-`. `latest` isn't special: declare it to use
it. Removing a channel makes the devices on it foreign. See
[Releases and channels](../concepts/releases.md).

### `key` *(optional)*

The cosign/sigstore **private** key every release is signed with (its public half is the
`.pub` sibling). Set by `bootcher signing enroll` and repointed by `signing rotate`; see
[Image signing](../concepts/signing.md).

---

## `[concurrency]`

Optional caps on parallel worker counts. When unset, each activity is unbounded — the pool scales up to `min(host parallelism, work items)`.

| Key | Default | Activity capped |
|---|---|---|
| `build` | unbounded | Parallel per-arch container builds (every building command) |
| `disk` | unbounded | Parallel per-arch image-builder runs (the disk step of `device provision`) |
| `upgrade` | unbounded | Parallel per-device rollouts: `device deploy/refresh/switch` and every channel device step |
| `rotate` | unbounded | Parallel per-device credential rollouts (`device rotate`) |
| `takeover` | unbounded | Parallel per-host takeover rollouts (`device takeover`) — its own knob, since a takeover moves files in place on a live host (and may pull a multi-GB image per host) rather than wiping a disk |

"unbounded" means the pool scales to `min(host cores, work items)`; the values shown in the example above (`build = 2`, etc.) are illustrative caps, not defaults.

Each value must be ≥ 1. Setting a cap only ever lowers the pool — it is bounded by both the cap and the number of work items.

---

## `[hooks.<phase>]`

Pre/post shell commands wrapping a phase. Each hook is run with `sh -c` from the project root (for a refresh build, the release commit's worktree — whose own hooks run); a non-zero exit aborts the run.

A `post` hook runs once its phase succeeded — except on the device-fleet phases (`upgrade`, `takeover`, `rotate`), where it runs once every device has been attempted, even if some failed, with each device's outcome in `BOOTCHER_METADATA`'s `results`; the run still fails afterwards if any device did. A failure before any device is attempted still aborts without `post`.

Because of this, **a `bootcher.toml` is executable content**, not inert configuration: a hook runs with the privileges of the phase it brackets, and `[hooks.disk]` brackets a step that runs under `sudo`. Treat a manifest from somewhere else — a recipe you copied, a colleague's project, a repository you just cloned — exactly as you would treat their `Makefile`, and read its hooks before the first run.

| Phase | Wraps |
|---|---|
| `[hooks.build]` | Every container build, including a refresh's worktree build (which runs that commit's hook) |
| `[hooks.disk]` | The image-builder disk-image step (`device provision`) |
| `[hooks.publish]` | Pushing a release `:X` (`release publish/refresh`, `channel refresh`, and the re-signing in `signing enroll/rotate`) |
| `[hooks.promote]` | Moving a channel (`channel promote/refresh`, `release publish --promote`) |
| `[hooks.upgrade]` | The device rollout only: `device deploy/refresh/switch` and every channel device step |
| `[hooks.takeover]` | Per-host in-place conversion (`device takeover`) |
| `[hooks.rotate]` | Per-device credential rollout (`device rotate`, and the `signing` device steps with `credential = "sign-key"`) |

Each `[hooks.<phase>]` table accepts `pre` and/or `post` string keys. Absent keys are no-ops.

### `BOOTCHER_METADATA`

Every hook is run with a single environment variable, `BOOTCHER_METADATA`, holding a JSON object describing the phase it brackets and the artifacts it concerns — so a portable recipe can find what bootcher just built without re-deriving the output layout from `bootcher.toml`. One schema spans all phases; a phase omits the fields it can't fill.

```jsonc
{
  "phase": "disk",            // "build" | "disk" | "publish" | "promote" | "upgrade" | "takeover" | "rotate"
  "stage": "post",            // "pre" | "post"
  "image_name": "kiosk",
  "arches": ["x86_64", "aarch64"],
  "registry": "ghcr.io/acme",          // [registry] url (omitted without a registry)
  "image": "ghcr.io/acme/kiosk",       // <registry>/<name>, whatever the install mode
  "image_ref": "localhost/kiosk:latest", // what the phase acts on: the local list, or a release/channel ref
  "revision": "9f3a1c2-dirty",        // git commit the image was built from (omitted outside a git tree)
  "version": "v1.4.0-2-g9f3a1c2",      // git describe: nearest tag / short SHA (omitted outside a git tree)

  // publish / promote, and a promote's device rollout:
  "release": "v1.4",
  "channel": "stable",                 // promote, and a channel rollout
  "previous_release": "v1.3",          // promote: what the channel served before

  // disk phase only:
  "output_dir": "output",
  "targets": [
    { "arch": "x86_64",  "disk_type": "qcow2", "dir": "output/x86_64/qcow2", "file": "output/x86_64/qcow2/disk.qcow2" },
    { "arch": "aarch64", "disk_type": "raw",   "dir": "output/aarch64/raw",  "file": "output/aarch64/raw/disk.raw"    }
  ],

  // upgrade / takeover / rotate phases only:
  "remotes": ["root@10.0.0.2"],

  // rotate phase only:
  "credential": "ssh-key",    // "pull-token" | "ssh-key" | "sign-key"

  // post stage of upgrade / takeover / rotate only:
  "results": [
    { "host": "admin@10.0.0.2", "ok": true },
    { "host": "admin@10.0.0.3", "ok": false, "error": "ssh: connect to host 10.0.0.3 port 22: Connection timed out" }
  ]
}
```

| Field | Phase(s) | Notes |
|---|---|---|
| `phase`, `stage` | all | branch a shared script on `"\(.phase).\(.stage)"` |
| `image_name` | all | `[general] name` |
| `arches` | all | the `[targets]` keys |
| `registry`, `image` | all, with a registry | `[registry] url` and the `<registry>/<name>` repository every release and channel is a tag on. Unlike `image_ref`, they don't change with the phase or install mode (a `--direct` disk's `image_ref` is local storage): derive a tag or a sibling repository from these rather than parsing `image_ref` |
| `image_ref` | all | the local list ref at `build`, the recorded origin at `disk`, the pushed release ref at `publish`, the channel ref at `promote` and a channel rollout, the local list ref at a direct rollout, the host's origin at `takeover`, the registry namespace at `rotate` |
| `revision` | when known | for a build, the git commit (`-dirty`-suffixed for an uncommitted tree; the `org.opencontainers.image.revision` label); for commands that build nothing, the release's `bootcher.source`. Omitted when unknown |
| `version` | builds | human build description (`git describe --tags --always --dirty`); the `org.opencontainers.image.version` label. Omitted outside a git work tree |
| `release` | publish, promote, a promote's rollout | the release name |
| `channel` | promote, channel rollouts, channel takeovers | the channel |
| `previous_release` | promote | the release the channel served before (omitted when it served none) |
| `output_dir` | disk | base output dir, relative to the project root |
| `targets[]` | disk | the `(arch × disk_type)` build matrix; `file` is the resolved `disk.<ext>`, present only at `disk.post` (and omitted if a dir doesn't hold exactly one `disk.*` — fall back to `dir`) |
| `remotes[]` | upgrade, takeover, rotate | the devices the rollout targets; at `takeover`, as the post-takeover `admin@host` |
| `credential` | rotate | `pull-token`, `ssh-key`, or `sign-key` (the `signing` device steps) |
| `results[]` | upgrade, takeover, rotate (`post` only) | each device's outcome: `host`, `ok`, and `error` (only when `ok` is false); unreachable devices are failures |

Paths are relative to the project root (the hook's working directory). A `disk.post` recipe walks the matrix straight from the JSON — e.g. embedding Raspberry Pi firmware into the aarch64 raw image:

```sh
echo "$BOOTCHER_METADATA" \
  | jq -r '.targets[] | select(.arch=="aarch64" and .disk_type=="raw") | .file' \
  | while read -r img; do embed_pi_firmware "$img"; done
```

See [`recipes/raspi4/`](https://github.com/cdellacqua/bootcher/tree/main/recipes/raspi4) for a complete, copy-pasteable recipe.

