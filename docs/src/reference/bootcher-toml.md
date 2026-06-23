# bootcher.toml

The project manifest. Its presence marks a directory as a bootcher project. All paths in the manifest are relative to the project root (the directory containing `bootcher.toml`).

## Editor validation & autocomplete

`bootcher init` writes the generated JSON Schemas into a `schemas/` directory beside the manifest, and binds the manifest to its schema with a directive on its first line:

```toml
#:schema ./schemas/bootcher.schema.json
```

This is the [Taplo](https://taplo.tamasfe.dev/) `#:schema` directive, honoured by the **Even Better TOML** VS Code extension (and the Taplo CLI/LSP). With it, your editor validates keys and values, completes table/property names, completes the closed-value fields (the `[general.disk_types]` arch keys and type values, `rootfs`), and shows the field docs on hover. The relative path resolves against the manifest's own directory, so the schema needs no hosting — commit the `schemas/` directory alongside `bootcher.toml` so collaborators get the same support without installing bootcher.

`init` also writes `schemas/metadata.schema.json` — the schema for the [`BOOTCHER_METADATA`](#bootcher_metadata) object hooks receive. It binds to nothing (it's reference docs for hook/recipe authors), but a recipe written in a typed language can validate against it.

Both schemas are generated from the same types bootcher uses at runtime, so they can't drift from what it actually accepts. The directive is bootcher- and Taplo-specific; other TOML tooling simply ignores it.

## Annotated example

```toml
#:schema ./schemas/bootcher.schema.json
[general]
name = "my-os"             # image name; used as the container tag
rootfs = "ext4"            # root filesystem (default: ext4)

[general.disk_types]        # the build matrix: each target arch → disk type(s)
x86_64 = "qcow2"           # one type, or a list: ["qcow2", "bootc-installer"]

[builder]
build = "local"            # container-build backend: "local", "vm", "[user@]host" or a rich { remote = "user@build-host", ssh_opts = ["-i", "/path/to/key"] }
image = "local"            # disk-image (image-builder) backend

# [builder.aarch64]          # per-arch override: route cross-arch image-builder to a VM
# image = "vm"

[deploy]
# registry = "registry.gitlab.com/org/project"       # plain: registry mode, no signing
# registry = { url = "registry.gitlab.com/org/project", key = "cosign.key" }  # with signing
remotes = []               # SSH targets: "admin@host" or { remote = "...", ssh_opts = [...] }

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

# [hooks.upgrade]
# pre  = "sh drain.sh"
# post = "sh smoke-test.sh"
```

---

## `[general]`

### `name` *(required)*

The image name. Used as:
- The local container tag: `localhost/<name>:latest-<arch>` and `localhost/<name>:latest`
- The per-device signing key path: `/etc/pki/containers/<name>.pub`
- The registry reference: `<registry>/<name>:latest` (registry mode)

### `[general.disk_types]`

The build matrix: each target architecture mapped to the image-builder disk type(s) to build for it. The present keys *are* the architectures the project builds — there is no separate `platform` key. Each value is a bare type or an array of them, and the artifacts land under `output/<arch>/<disk_type>/disk.<ext>`.

```toml
[general.disk_types]
x86_64  = ["qcow2", "bootc-installer"]   # a VM image and an installer ISO
aarch64 = "raw"                          # a raw image for an edge device
```

The two axes are independent. **Architecture** is the container/registry axis: `build`, `deploy` and `upgrade` fan out over the distinct arches, and a multi-arch project (more than one key) publishes one multi-arch manifest list. **Disk type** is a per-arch artifact axis only the `disk`/`provision` step fans out over — an arch listing several types reuses its single container build to render each, and disk types never enter the manifest list (an installer ISO isn't something a device `bootc upgrade`s to).

If `[general.disk_types]` is omitted, it defaults to the host architecture mapped to a single `qcow2` (falling back to `x86_64` on unrecognised hosts).

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

### `rootfs`

Root filesystem format (`--bootc-default-fs`). Default: `ext4`. Also accepts `xfs` and `btrfs`.

---

## `[builder]`

### `build` / `image`

Builder spec for the container build (the `build` step shared by `build`/`provision`/`deploy`/`takeover`) and the image-builder disk-image step (the disk step of `provision`) respectively.

| Value | Meaning |
|---|---|
| `"local"` *(default)* | Run in-process on the build host |
| `"vm"` | Use a throwaway local QEMU VM matching the target arch |
| `"[user@]host"` or `"ssh://[user@]host[:port]"` | Use a remote native-arch machine over SSH |
| `{ remote = "…", ssh_opts = […] }` | Remote with extra ssh args (identity file, port, etc.) |

**Which backend when** — the tradeoffs differ sharply by whether you're building for the host arch or cross-arch, and `bootcher init` defaults/recommends accordingly:

| | `local` | `vm` | remote |
|---|---|---|---|
| **Same arch** | *Recommended.* Fastest — runs in-process. Needs `sudo` on this host for the image-builder step. | A clean sandbox, but needs qemu + KVM (`/dev/kvm` accessible) to run at a usable speed. | Offloads to another machine; only worth it if this host can't build. |
| **Cross arch** | Container build works but runs slowly under emulation; the **image-builder step is slow and fragile** under emulation — avoid for `image`. | Safe everywhere, but **very slow** — emulated (no KVM for a foreign arch), so a full build can take hours for *either* role. | *Recommended when available.* A native-arch host runs at full speed with none of the emulation hazards. |

So: a cross-arch `image` step on a host with no same-arch remote means `vm` (slow but reliable); with a same-arch remote, prefer the remote. For same-arch, stay on `local` unless you specifically want the VM's isolation.

When a remote builder needs ssh options that the user's `~/.ssh/config` doesn't cover, use the inline-table form:

```toml
[builder]
build = { remote = "user@build-host", ssh_opts = ["-i", "/path/to/key"] }
image = "vm"
```

Or the equivalent expanded form with dotted keys:

```toml
[builder]
build.remote   = "user@build-host"
build.ssh_opts = ["-i", "/path/to/key"]
image          = "vm"
```

### `[builder.<arch>]`

Per-arch override. Either `build` or `image` (or both) may be set to override the flat default for that architecture only. Unset roles inherit the flat default.

```toml
[builder.aarch64]
image = "vm"    # route aarch64 image-builder to a VM; aarch64 container build stays local
```

---

## `[deploy]`

### `registry`

Selects the registry deploy backend and optionally enables image signing. Accepts two forms:

**Plain string** — registry namespace only, no signing:
```toml
registry = "registry.gitlab.com/org/project"
```

**Inline table** — namespace plus cosign signing config:
```toml
registry = { url = "registry.gitlab.com/org/project", key = "cosign.key" }
```

When set (either form), images are pushed to `<registry>/<name>:latest` and devices pull from there. When absent, the LAN (SSH-tunnel) backend is used.

The URL is not a credential — safe to commit. The pull token is collected at provision time; the push auth is managed by `podman login` on the builder.

For the `key` signing field, see [Image signing](../concepts/signing.md).

### `remotes`

Array of SSH targets to deploy to and rotate credentials on. Each entry is either a bare connection string or an object with per-target SSH options:

```toml
remotes = [
  "admin@192.168.1.42",
  { remote = "admin@192.168.1.43", ssh_opts = ["-o", "StrictHostKeyChecking=accept-new"] },
  { remote = "admin@vps.example.com", takeover_login = "debian" },
]
```

The connection string format is anything `ssh` accepts as a destination: `[user@]host` or `ssh://[user@]host[:port]`.

In LAN mode, at least one remote is required. In registry mode, `remotes` may be empty — devices self-update on their timer — but listing them triggers an immediate `bootc upgrade` after each `deploy` push.

The object form accepts these per-target keys:

| Key | Purpose |
|---|---|
| `ssh_opts` | Extra `ssh` args prepended to every connection (identity file, port, host-key policy, …) |
| `takeover_login` | The stock cloud login (`debian`/`ubuntu`/`cloud-user`/`root`) [`bootcher takeover`](../workflows/takeover.md) uses for its *initial* connection, before the image's `admin` user replaces it. Per-host override of `--login`. Unused outside takeover — the steady-state identity stays `admin@`, so after takeover this is an ordinary remote |

---

## `[concurrency]`

Optional caps on parallel worker counts. When unset, each activity is unbounded — the pool scales up to `min(host parallelism, work items)`.

| Key | Default | Activity capped |
|---|---|---|
| `build` | unbounded | Parallel per-arch container builds (`build`, `provision`, `deploy`) |
| `disk` | unbounded | Parallel per-arch image-builder runs (the disk step of `provision`) |
| `upgrade` | unbounded | Parallel per-device upgrade SSHes (the rollout step of `deploy`) |
| `rotate` | unbounded | Parallel per-device rotate SSHes (`rotate pull-token`, `rotate key`, `rotate sign-key`) |
| `takeover` | unbounded | Parallel per-host takeover rollouts (`takeover`) — its own knob, since a takeover moves files in place on a live host (and may pull a multi-GB image per host) rather than wiping a disk |

"unbounded" means the pool scales to `min(host cores, work items)`; the values shown in the example above (`build = 2`, etc.) are illustrative caps, not defaults.

Each value must be ≥ 1. Setting a cap only ever lowers the pool — it is bounded by both the cap and the number of work items.

---

## `[hooks.<phase>]`

Pre/post shell commands wrapping a build phase. Each hook is run with `sh -c` from the project root; a non-zero exit aborts the run.

| Phase | Wraps |
|---|---|
| `[hooks.build]` | Container build (`build`, `provision`, `deploy`) |
| `[hooks.disk]` | image-builder disk-image step (`disk`, `provision`) |
| `[hooks.upgrade]` | Push + `bootc upgrade` per device (`upgrade`, `deploy`) |

Each `[hooks.<phase>]` table accepts `pre` and/or `post` string keys. Absent keys are no-ops.

### `BOOTCHER_METADATA`

Every hook is run with a single environment variable, `BOOTCHER_METADATA`, holding a JSON object describing the phase it brackets and the artifacts it concerns — so a portable recipe can find what bootcher just built without re-deriving the output layout from `bootcher.toml`. One schema spans all phases; a phase omits the fields it can't fill.

```jsonc
{
  "phase": "disk",            // "build" | "disk" | "upgrade"
  "stage": "post",            // "pre" | "post"
  "image_name": "kiosk",
  "arches": ["x86_64", "aarch64"],
  "image_ref": "registry.example.com/org/kiosk:latest",

  // disk phase only:
  "output_dir": "output",
  "targets": [
    { "arch": "x86_64",  "disk_type": "qcow2", "dir": "output/x86_64/qcow2", "file": "output/x86_64/qcow2/disk.qcow2" },
    { "arch": "aarch64", "disk_type": "raw",   "dir": "output/aarch64/raw",  "file": "output/aarch64/raw/disk.raw"    }
  ],

  // upgrade phase only:
  "remotes": ["root@10.0.0.2"]
}
```

| Field | Phase(s) | Notes |
|---|---|---|
| `phase`, `stage` | all | branch a shared script on `"\(.phase).\(.stage)"` |
| `image_name` | all | `[general] name` |
| `arches` | all | the `[general.disk_types]` keys |
| `image_ref` | all | suffix-free ref: local list ref at `build`, bootc-origin source ref at `disk`, pushed/served list ref at `upgrade` |
| `output_dir` | disk | base output dir, relative to the project root |
| `targets[]` | disk | the `(arch × disk_type)` build matrix; `file` is the resolved `disk.<ext>`, present only at `disk.post` (and omitted if a dir doesn't hold exactly one `disk.*` — fall back to `dir`) |
| `remotes[]` | upgrade | LAN ssh targets; omitted in pure-registry mode |

Paths are relative to the project root (the hook's working directory). A `disk.post` recipe walks the matrix straight from the JSON — e.g. embedding Raspberry Pi firmware into the aarch64 raw image:

```sh
echo "$BOOTCHER_METADATA" \
  | jq -r '.targets[] | select(.arch=="aarch64" and .disk_type=="raw") | .file' \
  | while read -r img; do embed_pi_firmware "$img"; done
```

See [`recipes/raspi4/`](https://github.com/cdellacqua/bootcher/tree/main/recipes/raspi4) for a complete, copy-pasteable recipe.

