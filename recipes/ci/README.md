# Recipe: CI/CD for a bootcher project

Multi-arch pipeline templates that run bootcher in CI — building **x86_64 and
aarch64 on native runners** (no cross-arch emulation), in three stages:

- **build** — one job per arch, in parallel on a native-arch runner each. `bootcher
  build --target <arch>` builds just that arch's container and stops at the per-arch
  member tag `<name>:latest-<arch>` (no multi-arch list yet), which the job `podman
  save`s and hands to **deploy** as an artifact (a run artifact on GitHub, a job
  artifact on GitLab). These jobs need **no registry credentials**. Why split per
  arch? A single-runner multi-arch build would emulate the foreign arch under
  qemu-user — slow and occasionally fragile; native runners are faster and sounder.
- **deploy** — `podman load`s both per-arch members and `bootcher deploy
  --skip-build` **assembles the multi-arch manifest list from them** and pushes
  `<registry>/<name>:latest` (+ an immutable CalVer tag) to the **registry associated
  with your repo** (GHCR for GitHub, the project Container Registry for GitLab). The
  **only** job that needs registry push credentials (and the signing key, if
  enabled). Runs automatically on the default branch and on tags; devices self-update
  from there.
- **provision** — one job per arch (e.g. a qcow2 for x86_64, a raw for aarch64).
  `bootcher provision --target <arch> --disk <type> --skip-build` builds the disk
  natively and publishes `output/` (packed as a single `tar`+`zstd` file) for you to
  download and flash — to a **per-commit GitHub Release asset** on GitHub, or the
  project's **generic Package Registry** on GitLab. Both sidestep the run-artifact
  size limits (GitLab.com caps job artifacts at 1 GB; a sparse bootc disk easily
  exceeds that), give a stable versioned download, and don't expire the way an
  artifact does. Runs on demand, since you only need a fresh disk when enrolling a new
  device. `--skip-build` **reuses the image `deploy` pushed** — it pulls that arch's
  member out of `<registry>/<name>:latest` instead of rebuilding the container — so
  the job skips the multi-minute container build and the flashed disk is byte-for-byte
  what devices auto-update to (a rebuild from source could drift). Because a disk build
  should fire only when you want to flash a device, it's **manually gated** — but the
  gate differs by platform, because GitHub Free can't pause a job inside a push pipeline
  (Environment "Required reviewers" is public-repo-only on Free). On **GitLab** provision
  stays in the pipeline, **chained behind deploy** (`needs`) with a `when: manual` play
  button, so it reuses *this pipeline's* freshly built image. On **GitHub** provision is
  its **own `workflow_dispatch` workflow** (`provision.yml`) you launch by hand from the
  Actions tab — the "Run workflow" button is the gate, on every plan — and, decoupled from
  a single run, it builds from the **current** `<registry>/<name>:<channel>` tip, so
  dispatch it once the build→deploy run for the image you want has finished. The job logs
  in to the registry to pull (read access is enough);
  for a **public** registry, drop that login — the pull is anonymous — and separately
  pass `--anonymous` to `bootcher provision` so it bakes no pull credential into the
  disk either (the device pulls updates anonymously too).

The CI owns the `podman save`/`load` that moves the per-arch members between jobs, so
bootcher's own `build`/`deploy` on a dev box are unchanged. Single-arch project?
Drop one arch from the matrix (GitHub) / delete the second `build:`/`provision:` job
(GitLab), and list only that arch in `[targets]`.

| Platform | File | Copy it to |
|---|---|---|
| GitHub Actions (deploy) | [`github-actions/deploy.yml`](github-actions/deploy.yml) | `.github/workflows/bootcher-deploy.yml` |
| GitHub Actions (provision) | [`github-actions/provision.yml`](github-actions/provision.yml) | `.github/workflows/bootcher-provision.yml` |
| GitLab CI | [`gitlab-ci.yml`](gitlab-ci.yml) | `.gitlab-ci.yml` (repo root) |
| Both | [`bootcher.ci.toml`](bootcher.ci.toml) | `bootcher.ci.toml` (repo root, beside `bootcher.toml`) |

**Release channels?** For a workflow where the default branch publishes the rolling
`latest` channel and a `v*` git tag promotes to a `stable` channel for a production
fleet, see the [`channels/`](channels/) variant of this recipe — same pipeline shape,
with the trigger choosing which [channel](../../docs/src/reference/bootcher-toml.md#channels)
each push publishes to.

Both pipelines run **inside the published bootcher image**,
[`ghcr.io/cdellacqua/bootcher`](https://ghcr.io/cdellacqua/bootcher) — multi-arch
(amd64 + arm64), bundling podman and qemu — so there's nothing to install in the
runner. They reference `:latest`; pin it to a released `:vX.Y.Z` for reproducible
builds.

## Point the manifest at your repo's registry

bootcher reads the registry from `bootcher.toml` (it's committed, not a secret), so
set it to your repo's namespace. The image published is `<registry>/<name>:latest`,
where `<name>` is `[general] name`.

```toml
# GitHub — lowercase owner; yields ghcr.io/<owner>/<name>
[deploy]
registry = "ghcr.io/<owner>"

# GitLab — equals $CI_REGISTRY_IMAGE; yields registry.gitlab.com/<group>/<project>/<name>
[deploy]
registry = "registry.gitlab.com/<group>/<project>"
```

CI authenticates separately with `podman login` (GHCR via `GITHUB_TOKEN`, GitLab
via the job's `CI_REGISTRY_*`), so the URL above carries no credential.

These templates are multi-arch, so `bootcher.toml` must also list **both** arches in
`[targets]`, mapped to the disk type each should produce — the `build`/`provision`
jobs scope to one arch with `--target`:

```toml
[targets]
x86_64  = "qcow2"   # a VM image
aarch64 = "raw"     # a raw image for an edge device
```

## The `bootcher.ci.toml` override

Both pipelines run with `--manifest bootcher.ci.toml`, a small committed override
that [`extend`s](../../docs/src/reference/bootcher-toml.md#extend-top-level-optional)
your `bootcher.toml` and changes one thing: it forces podman onto the host network
(`--network=host`) for the build steps.

```toml
extend = "bootcher.toml"

[builder]
build = { type = "local", podman_opts = ["--network=host"] }
image = { type = "local", podman_opts = ["--network=host"] }
```

Inside a privileged CI container, rootless/nested podman's per-container networking
(netavark + nftables) is a common source of opaque build failures. `--network=host`
makes podman reuse the runner's network namespace instead of programming its own,
sidestepping the nftables path entirely. It's set on **both** roles so it covers the
`build` jobs (the container build) and `provision` (the image-builder disk step).
Everything else — name, registry, `[targets]`, deploy targets, hooks — is inherited
from `bootcher.toml`, so this file never drifts: edit your real config there, not here.

Local runs (`bootcher deploy` / `provision` with no `--manifest`) are unaffected —
they still use the plain `bootcher.toml`, where podman's default networking works
fine.

## Secrets & variables

Set these under GitHub *Settings → Secrets and variables → Actions* or GitLab
*Settings → CI/CD → Variables*. Mask the credentials below; `ADMIN_SSH_PUBKEY` is a
public key, so add it as a plain (unmasked) **variable** rather than a secret.

| Secret | Used by | What it is |
|---|---|---|
| `ADMIN_SSH_PUBKEY` | provision | Admin SSH **public** key (the `.pub` line), baked in as the `admin` login. **Not secret** — provision reads only the public half, so a plain repository **variable** is enough (GitHub: the *Variables* tab; GitLab: an unmasked CI/CD variable). Keep the matching private key wherever you SSH to devices from. |
| `PULL_USER` / `PULL_TOKEN` (GitHub)<br>`BOOTCHER_PULL_USER` / `BOOTCHER_PULL_TOKEN` (GitLab) | provision, registry mode | A **long-lived, read-only** registry credential baked into the disk so the device can pull updates. **GitHub:** `PULL_USER` is your GitHub username (or org), `PULL_TOKEN` a PAT with `read:packages`. **GitLab:** create a **Deploy Token** with `read_registry` scope under *Settings → Repository → Deploy tokens* — **not** *Settings → Access tokens* (that page gives no username). After creating, copy **both** fields from the one-time banner: the username (`gitlab+deploy-token-…`, or a custom name if you set one) → `BOOTCHER_PULL_USER`, and the secret → `BOOTCHER_PULL_TOKEN`. The username isn't shown again, so if you navigate away first, delete and recreate the token. **Not** the ephemeral `GITHUB_TOKEN` / `CI_JOB_TOKEN` — those expire when the run ends, leaving a device that can't update. Omit for a public registry and pass `--anonymous` (see the comments in each file). |
| `SIGN_KEY` / `SIGN_PASSPHRASE` | both, optional | The cosign private key and its passphrase — only if you enabled [image signing](../../docs/src/concepts/signing.md). The `*.key` is git-ignored, so CI must restore it from a secret; the passphrase is read from `BOOTCHER_SIGN_PASSPHRASE`. |

The push credential for **deploy** is the platform's built-in job token — no manual
secret needed.

## Why deploy uses `--skip-bootc-upgrade`

In registry mode `deploy` pushes the image and then SSHes into each `[deploy]
remotes` device as `admin@host` to run `bootc upgrade`. The templates skip that
second half because it's the wrong place to do it, not because it's impossible:

- it needs the admin **private key** in the runner and a pinned host key, and
- it needs CI to actually reach each device — fine for a public-IP VPS fleet, but
  edge/kiosk/home devices behind NAT/LAN/VPN aren't routable from a cloud runner.

And it's unnecessary: registry mode means devices self-update on their
`bootc-fetch-apply-updates.timer`, so publishing the image *is* the deploy. Drop
`--skip-bootc-upgrade` (and supply the key + reachability) only if you want CI to
roll the fleet immediately; otherwise run a plain `bootcher deploy` from an
operator machine that can reach the devices.

(LAN mode — no `registry` in the manifest — can't be driven from CI at all: it
tunnels the image to devices over SSH from the operator's machine. These recipes
assume registry mode.)

## Architecture

The templates build **both** arches, each on a native-arch runner, and assemble them
into one multi-arch image — no cross-arch emulation. The arch fan-out is the runner
each job lands on: on GitHub a matrix over `runs-on: ubuntu-24.04` (x86_64) and
`ubuntu-24.04-arm` (aarch64); on GitLab a job per arch, the aarch64 one tagged for a
hosted arm64 runner — `tags: [saas-linux-small-arm64]` (the small size is on all
tiers; medium/large are Premium/Ultimate only) — the x86_64 one on the default runner.
Each `build`/`provision` job scopes bootcher to its runner's arch with `--target`
(and `provision` picks the disk type with `--disk`), all from the **one shared
`bootcher.toml`** — no per-arch manifest.

The split exists because building a foreign arch on a single runner means qemu-user
emulation: slow and occasionally fragile. The per-arch members are handed between
jobs via the CI's artifact store (`podman save`/`load`), so the registry only ever
receives the final assembled `:latest`, and the build jobs need no registry
credentials. (If you'd rather not fan out — e.g. you only have x86_64 runners — point
the aarch64 `[builder]` at a `vm`/remote and build everything in one job; expect the
emulation cost.)

**Single-arch project?** List only that arch in `[targets]`, and drop the other arch:
on GitHub remove it from the `build`/`provision` `matrix.include`; on GitLab delete
the second `build:`/`provision:` job. `--target` then names your one arch (a build for
an arch not in `[targets]` is a hard error).

## Only rebuild when the image actually changes

Both templates path-gate the pipeline so a push that can't change the image — docs, a
README tweak, an unrelated script — doesn't spin up the whole multi-arch build →
deploy. The build context is the **entire project root**, so the gate is an
**allowlist** of the inputs that genuinely re-image:

- `Containerfile`
- `sysroot/**` — the overlay baked into the image
- `bootcher.toml` and `bootcher.ci.toml` — a registry/`[targets]`/`[hooks]` change re-images too
- the CI file itself, so editing the pipeline re-runs it

**A `v*` tag always builds**, path filter or not — a tag is a deliberate release.

The gate is applied consistently across stages so the `needs` chain never breaks:
`build` and `deploy` share it. On **GitLab**, `provision` inherits it transitively (it
`needs` deploy, so when deploy is filtered out provision drops too — a disk build has
nothing new to pull anyway). On **GitHub**, `provision` is a separate manual
`workflow_dispatch` workflow, so the gate doesn't apply to it at all — you launch it only
when you actually want a disk, and it builds from whatever image is already published.

**Tune the allowlist to your project.** If your `Containerfile` `COPY`s other paths, or
a `[hooks]` script lives outside `sysroot/`, add those paths — anything in the build
context that ends up in the image belongs in the list.

- **GitLab** — a hidden `.image-rules` job holds the `rules:` (`changes:` on the
  default-branch rule, no `changes:` on the tag rule), `!reference`d from both `.build`
  and `deploy`.
- **GitHub** — a cheap `changes` pre-job (`dorny/paths-filter`, on a plain runner)
  outputs a boolean that `build` gates on with
  `if: needs.changes.outputs.image == 'true' || startsWith(github.ref, 'refs/tags/')`.

## Requirements & troubleshooting

- Both pipelines run bootcher **inside its container image**, which needs a
  **privileged** container so podman can build images (and the disk step can launch
  the nested image-builder container).
  - **GitHub**: the templates set `container.options: --privileged`; GitHub-hosted
    runners allow it. No `/dev/kvm`, so the default in-process `local` builder is
    the supported path.
  - **GitLab**: the job container must be privileged. GitLab.com's hosted
    `saas-linux-*` runners already run in privileged mode (each job in an isolated,
    ephemeral VM), so the jobs work as-is — no runner setup needed. A self-managed
    runner (`privileged = true`, or a shell-executor on a podman host) is only needed
    if you want to narrow the privilege grant or route a build step to a `vm`/`/dev/kvm`.
- **`podman build` networking errors** (netavark/nftables, "failed to set up
  network", iptables/chain errors) inside the privileged container are handled up
  front by [the `bootcher.ci.toml` override](#the-bootcherci-toml-override), which
  runs the build steps with `--network=host`. If you drop that override, expect to
  hit these on runners where nested podman can't program its own network.
