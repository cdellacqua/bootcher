# Recipe: CI/CD for a bootcher project

Pipeline templates that run bootcher in CI for the two outward-facing steps of a
project's lifecycle:

- **deploy** — `bootcher deploy` builds the container and pushes
  `<registry>/<name>:latest` to the **registry associated with your repo** (GHCR
  for GitHub, the project Container Registry for GitLab). Runs automatically on the
  default branch and on tags; devices self-update from there.
- **provision** — `bootcher provision` builds the container *and* the disk image,
  and publishes `output/` as a downloadable CI artifact for you to flash or upload.
  Runs on demand (manual trigger), since you only need a fresh disk when enrolling
  a new device.

| Platform | File | Copy it to |
|---|---|---|
| GitHub Actions | [`github-actions.yml`](github-actions.yml) | `.github/workflows/bootcher.yml` |
| GitLab CI | [`gitlab-ci.yml`](gitlab-ci.yml) | `.gitlab-ci.yml` (repo root) |

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

The templates build a single architecture matching the runner. The bootcher image
is multi-arch, so the same tag runs on either — just pick the runner's arch: on
GitHub, `runs-on: ubuntu-24.04` (x86_64) or `ubuntu-24.04-arm` (aarch64); on GitLab,
tag the job for a hosted arm64 runner — `tags: [saas-linux-small-arm64]` (the small
size is available on all tiers; medium/large are Premium/Ultimate only) — or leave
it untagged for the default x86_64 runner. If
`[general.disk_types]` lists one arch, run the job on a runner of that arch. For a
**multi-arch** image, either run the build on a runner whose `[builder]` routes the
foreign arch to a `vm`/remote, or split into a per-arch job matrix on native
runners. Emulated cross-arch image-builder runs are slow and fragile — prefer
native runners.

## Requirements & troubleshooting

- Both pipelines run bootcher **inside its container image**, which needs a
  **privileged** container so podman can build images (and the disk step can launch
  the nested image-builder container).
  - **GitHub**: the templates set `container.options: --privileged`; GitHub-hosted
    runners allow it. No `/dev/kvm`, so the default in-process `local` builder is
    the supported path.
  - **GitLab**: the job container must be privileged. GitLab.com's hosted
    `saas-linux-*` runners already run in privileged mode (each job in an isolated,
    ephemeral VM), so both jobs work as-is — no runner setup needed. A self-managed
    runner (`privileged = true`, or a shell-executor on a podman host) is only needed
    if you want to narrow the privilege grant or route a build step to a `vm`/`/dev/kvm`.
