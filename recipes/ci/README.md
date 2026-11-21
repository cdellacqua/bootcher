# Recipe: CI/CD for a bootcher project

Multi-arch pipeline templates for a project with a `[registry]`: they publish
**releases** from git tags, **promote** them to the channels your devices follow, keep
those channels on a patched base with scheduled **refreshes**, and build **disk images**
for new channel devices. Every build runs on a native runner per arch (x86_64 and
aarch64), never under emulation.

| Trigger | Jobs |
|---|---|
| push a `v*` git tag | per-arch `build --target` → hand-off → `release publish "$TAG" --skip-build` |
| manual | `channel promote -y --skip-remotes <release> <channel>` |
| manual, with a release | per-arch build of the release's commit → hand-off → `release refresh <release> --skip-build` |
| schedule, one per channel | `channel status <channel>` → if its base moved: per-arch build of the commit it serves → hand-off → `channel refresh <channel> -y --skip-remotes --skip-build` |
| manual, per arch | `device provision --channel <channel> --target <arch> --disk <type>` |
| branch push | nothing |

Branch pushes run nothing on purpose: **direct** devices (the bench board on your desk)
are updated from a workstation with `bootcher device deploy`, and **channel** devices
only ever get releases someone promoted. CI never reaches a device either
(`--skip-remotes`): a fleet behind NAT isn't routable from a cloud runner, and channel
devices update themselves on their `bootc-fetch-apply-updates.timer`. From a workstation
that does reach them, run the same `channel promote` without `--skip-remotes` to update
them right away.

| Platform | File | Copy it to |
|---|---|---|
| GitHub Actions | [`github-actions/release.yml`](github-actions/release.yml) | `.github/workflows/bootcher-release.yml` |
| GitHub Actions | [`github-actions/promote.yml`](github-actions/promote.yml) | `.github/workflows/bootcher-promote.yml` |
| GitHub Actions | [`github-actions/refresh.yml`](github-actions/refresh.yml) | `.github/workflows/bootcher-refresh.yml` |
| GitHub Actions | [`github-actions/provision.yml`](github-actions/provision.yml) | `.github/workflows/bootcher-provision.yml` |
| GitLab CI | [`gitlab-ci.yml`](gitlab-ci.yml) | `.gitlab-ci.yml` (repo root) |
| Both | [`bootcher.ci.toml`](bootcher.ci.toml) | `bootcher.ci.toml` (repo root, beside `bootcher.toml`) |
| Both | [`scripts/`](scripts/) | `ci/` (repo root) |

The pipelines run inside the published bootcher image,
[`ghcr.io/cdellacqua/bootcher`](https://ghcr.io/cdellacqua/bootcher) (multi-arch,
bundling podman, skopeo, git and qemu), so there's nothing to install on the runner. They
reference `:latest`; pin a released `:vX.Y.Z` for reproducible pipelines.

## The flow

```sh
git tag v1.4 && git push origin v1.4     # CI publishes <registry>/<name>:v1.4
# Actions → "bootcher promote" (GitHub) / the promote:next play button (GitLab)
#   → :next serves v1.4; canary devices update on their next timer run
# … later, the same for stable
# scheduled: the base moved → v1.4 is rebuilt from its commit, :v1.4 re-pushed, the
#   channels serving it re-copied
```

A release is named by its git tag, built from the tag's commit, and recorded at it
(`bootcher.source`). Republishing a tag that's already published from the same commit is
a no-op, so re-running a pipeline is safe.

**Rollback** is a promote of the previous release (`channel promote` prints it). If that
release's base has moved since, the promote refuses: refresh it first (the manual
refresh with that release), then promote it. On a direct device, `bootc rollback` is the
only rollback.

**Pausing a channel's refresh**: drop its line from the schedules (GitHub: `schedule:`
and the `SCHEDULES` table in `refresh.yml`; GitLab: its pipeline schedule). From a
workstation, `bootcher channel refresh --exclude <channel>` does the same for one run.

**More than three tags at once**: GitHub sends no push events when more than three tags
are pushed in one `git push`. Push release tags one at a time, or run `release.yml` by
hand on the tag.

**`--git-tag push` costs a CI build**: publishing from a workstation with
`bootcher release publish v1.5 --git-tag push` pushes the tag, which triggers the tag
pipeline — a second, redundant build (the publish itself is then a no-op). Use
`--git-tag local` and push the tag yourself when you want CI to do the publishing, or
publish only from CI.

## Point the manifest at your repo's registry

bootcher reads the registry from `bootcher.toml` (committed, not a secret). The image is
`<url>/<name>`, where `<name>` is `[general] name`; channels are declared there too.

```toml
[registry]
url = "ghcr.io/<owner>"                      # GitHub: lowercase owner
# url = "registry.gitlab.com/<group>/<project>"  # GitLab: equals $CI_REGISTRY_IMAGE
channels = ["stable", "next"]

[targets]
x86_64  = "qcow2"   # a VM image
aarch64 = "raw"     # a raw image for an edge device
```

Keep the channel list in sync with the GitHub `channel` input options (`promote.yml`,
`provision.yml`), the `SCHEDULES` table, and the GitLab `promote:<channel>` jobs.

The jobs never restate `<registry>/<name>`: each derives it at runtime with `bootcher
info --format '{{ .Image }}'` (and the member name from `{{ .Name }}`), which follows the
`extend` chain, so the value CI tags scratch members and prunes packages by can't drift
from the one bootcher publishes.

## The hand-off between jobs

Each per-arch build job pushes its member under a scratch tag keyed by the commit,
`<image>:ci-<sha12>-<arch>`; the publish or refresh job pulls them back as the local
`localhost/<name>:latest-<arch>` images `--skip-build` takes. `--skip-build` checks that
each one records the release's commit, so a stale or mismatched member is refused rather
than published. The registry, not CI artifacts, because a full OS-image member easily
exceeds the free-tier artifact caps (GitHub's ~500 MB storage, GitLab.com's 1 GB per
job); the scratch tag shares its layers with the release, so it costs almost nothing.
The push uses the platform's automatic token (`GITHUB_TOKEN` with `packages: write`,
GitLab's `CI_REGISTRY_*`).

Publishing and moving channels never run at the same time for one image: GitHub puts
`release.yml`, `promote.yml` and `refresh.yml` in one `concurrency` group without
`cancel-in-progress`, GitLab uses a `resource_group`.

## Base-image patches

The `Containerfile` builds `FROM` a fedora-bootc tag that keeps moving with Fedora's
updates, but a release only picks those up when it's rebuilt. The scheduled refresh does
that per channel, without shipping unreleased source:

1. `bootcher channel status <channel> --format '{{ .Stale }} …'` reads the release the
   channel serves, its source commit, and whether each member's recorded base
   (`org.opencontainers.image.base.digest`) is still the base tag's current digest for
   that arch. Anything it can't verify counts as moved.
2. If it moved, the build jobs check out **that release's commit** (a full clone) and
   build it under **that commit's own** `bootcher.ci.toml`, on today's base.
3. `bootcher channel refresh <channel> --skip-build` re-pushes the release (signed, same
   name, same source) and re-copies every channel serving it. A channel a promote moved
   in the meantime is left alone.

Stagger the schedules so the canary channel runs a new base for a few days before
production does. A refresh moves the channel's digest, so its devices download it and
reboot on their next timer run: time the schedule ahead of the window you want that in.

A refresh builds old source with the current bootcher and that commit's config. A
breaking change to the build sections, or a commit without `bootcher.ci.toml`, makes that
refresh fail loudly; publish a new release then. Gitignored or generated build inputs
don't exist in the checkout either. Releases published before bootcher recorded
`bootcher.source` can't be refreshed: publish them again.

**The pinned Fedora release.** The scaffold pins one Fedora release (`ARG
FEDORA_VERSION`), and that release's tag stops moving at end of life — from then on no
channel is ever stale again. The scheduled `release-check` job
([`ci/release_check.py`](scripts/release_check.py)) compares the pin with the release
`fedora-bootc:latest` points at: it warns at one release behind and fails at two. It
gates nothing; the failure is the alert. Bump `FEDORA_VERSION`, commit, tag a release,
promote it.

## Disk images for new devices

The manual provision jobs build one disk per arch with `bootcher device provision
--channel <channel>`: the disk is built from exactly what the channel serves (pulled by
digest, never rebuilt), with the read-only pull credential and, with signing on, the
trust files baked in, and its bootc origin set to the channel. The `.zip` (each disk
`zstd`-compressed inside) lands on a GitHub Release `disk-<channel>-<release>`, or in
GitLab's generic Package Registry under `disk/<channel>-<release>/`. Extract it natively,
then hand the `disk.*.zst` to rpi-imager / balenaEtcher, which read `zstd` directly.

For a **public** registry, drop the pull credential and pass `--anonymous`: no
`auth.json` is baked and the device pulls anonymously.

## Secrets and variables

| Secret | Used by | What it is |
|---|---|---|
| `ADMIN_SSH_PUBKEY` | provision | The admin SSH **public** key (the `.pub` line), baked in as the `admin` login. Not secret: a plain repository/CI **variable** is enough. |
| `PULL_USER` / `PULL_TOKEN` (GitHub)<br>`BOOTCHER_PULL_USER` / `BOOTCHER_PULL_TOKEN` (GitLab) | provision | A **long-lived, read-only** registry credential baked into the disk so the device can pull updates. GitHub: your username and a PAT with `read:packages`. GitLab: a **Deploy Token** with `read_registry` (Settings → Repository → Deploy tokens; copy both fields from the one-time banner). **Not** the job token — it expires with the run. |
| `SIGN_KEY` / `SIGN_PASSPHRASE` | publish, refresh | The cosign private key and its passphrase, with [image signing](../../docs/src/concepts/signing.md) on. The `*.key` is git-ignored, so CI restores it from a secret (uncomment the step); the passphrase is read from `BOOTCHER_SIGN_PASSPHRASE`. On GitLab, protect your `v*` tags and mark both variables protected, so they reach the tag pipelines and nothing else. |

If you restore files with GitLab's *secure files*, add `.secure_files/` to `.gitignore`:
`release publish` refuses a dirty tree.

Promote needs neither the signing key nor a build environment: it checks the release's
signature against the public key and copies by digest. Gate who may promote with an
Environment per channel (GitHub `environment:` on the promote job; GitLab protected
environments for the `promote:<channel>` jobs).

## Pruning old images

Releases, refreshes and scratch tags accumulate.

**GitHub** — the `cleanup` job (after a publish or a refresh, with
[`ci/ghcr_cleanup.py`](scripts/ghcr_cleanup.py)) keeps every channel, every release a
channel serves (read from each channel's `bootcher.release` annotation, so a channel that
lags its release keeps both), the in-flight `ci-*` scratch tags (younger than
`CI_MAX_AGE_HOURS`, default 24), and the newest `KEEP_PREVIOUS` other releases. It
deletes the rest: older releases, old scratch tags, and the untagged digests a refresh
orphaned when it re-pushed a release. It's multi-arch-safe — GHCR stores each arch's
member as its own (untagged) package version, so it deletes by digest, keeping the members
the kept lists reference — and each cosign signature (`sha256-<digest>.sig`) follows the
image it signs. Set `DRY_RUN: "true"` to log the plan first. An org-owned package may
need a `CLEANUP_TOKEN` PAT with `delete:packages` as `GH_TOKEN`.

**GitLab** — use the project's built-in **Container Registry cleanup policy**
(Settings → Packages and registries → Clean up image tags) instead of a job. Keep the
signature attachments (`sha256-.*`), your channel names, and recent `ci-.*` tags in *Keep
tags matching*; let it remove older release tags beyond the number you keep. GitLab's
registry garbage collection then reclaims unreferenced layers.

## The scripts

The pipelines run two stdlib-only Python scripts from `ci/`:
[`release_check.py`](scripts/release_check.py) (both platforms) and
[`ghcr_cleanup.py`](scripts/ghcr_cleanup.py) (GitHub). Each docstring says what it does
and which environment variables it reads. [`tests/`](tests/) holds bootcher's own tests
for them (stdlib `unittest`, with a fake `skopeo` and a fake GitHub API); copy it as
`ci/tests/` if you change a script's decision logic (`check()`, `plan()`), and run
`python3 -B -m unittest discover -s ci/tests`.

## The `bootcher.ci.toml` override

Every job runs with `--manifest bootcher.ci.toml`, a committed override that
[`extend`s](../../docs/src/reference/bootcher-toml.md#extend-top-level-optional) your
`bootcher.toml` and changes one thing: podman uses the host network for both build roles
(`--network=host`), sidestepping nested podman's netavark/nftables setup inside a
privileged CI container, a common source of opaque build failures. Everything else is
inherited. A refresh builds an older commit under **its** `bootcher.ci.toml`, so keep the
file committed.

## Requirements and troubleshooting

- The jobs run bootcher in its container image, which must be **privileged** so podman
  can build (and image-builder can run nested). GitHub: `container.options:
  --privileged` (hosted runners allow it). GitLab.com's hosted `saas-linux-*` runners are
  privileged already; a self-managed runner needs `privileged = true`.
- The aarch64 jobs run on native arm64 runners: `ubuntu-24.04-arm` on GitHub,
  `tags: [saas-linux-small-arm64]` on GitLab. Single-arch project? List one arch in
  `[targets]` and drop the other job (and the other arch in the pull loops).
- The build jobs `chown` the checkout to the container user: git refuses a checkout
  owned by another uid ("dubious ownership"), and a build without git records no source,
  which `release publish` then refuses.
