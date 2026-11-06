# Recipe: CI/CD with release channels (`latest` + `stable`)

A channels-aware variant of the [base CI recipe](../README.md). Same three-stage,
multi-arch pipeline (**build → deploy → provision**, native per-arch runners, loopback
container store, path gate) — the only addition is that **the trigger picks the
[release channel](../../../docs/src/reference/bootcher-toml.md#channels)** a push
publishes to:

| Trigger | Channel | Audience |
|---|---|---|
| push to the default branch (`main`) | `latest` (bootcher's default — no flag) | rolling / **canary** fleet |
| push a `v*` git tag | `stable` (`bootcher promote <version> --channel stable`, no rebuild) | **production** fleet |
| Saturday schedule (if the base moved) | `latest` | base-image updates for canary |
| Tuesday schedule (if the base moved) | `stable` | base-image updates for production, no new release |
| manual run on the default branch / a `v*` tag | `latest` / `stable` | an unconditional rebuild (a tag's too) |

Each channel gets its own weekly base-image refresh, staggered so the canary fleet runs a
new Fedora base for a few days before production does. A refresh rebuilds **the exact
commit the channel currently ships**, read from the `org.opencontainers.image.revision`
label bootcher stamps on every image, on today's base. It never ships unreleased source,
and it works however a channel is fed: tags, a branch, or a rollback pushed by hand. It
moves `:<channel>` and adds a new CalVer tag; nothing immutable is overwritten. Each
refresh skips the build when the base hasn't moved since that channel was last published.

If the published image has no usable label (built before bootcher v0.2.6 shipped git, or
deployed from a dirty tree), `latest` falls back to the default branch. Any other channel
fails the run rather than guess what to ship. Publish it once the normal way and the
next refresh has a label to follow.

- **GitHub** — one cron line per channel, mirrored in the `changes` job's `SCHEDULES`
  table (`<channel> <cron>` per line), which maps the firing cron to its channel.
- **GitLab** — one pipeline schedule per channel on the default branch. The non-`latest`
  ones set the variable `SCHEDULE_CHANNEL=<channel>`. Needs git and skopeo in the bootcher
  image (v0.2.6+).

When you add a channel (below), give it a schedule too. See
[Weekly rebuilds](../README.md#weekly-rebuilds-for-base-image-updates) for the
base-image check itself.

A channel is a mutable registry tag `<registry>/<name>:<channel>` that a *subset* of
devices tracks; the same digest is **also** stamped with its immutable `:CalVer` tag
regardless of channel, so the version tag stays the durable rollback/audit handle and
the channel is just a moving pointer. See the
[`channels` reference](../../../docs/src/reference/bootcher-toml.md#channels) for the
full model; the [base recipe README](../README.md) covers everything *not* specific to
channels (the per-arch split, the loopback store, secrets, the `bootcher.ci.toml`
override, why deploy uses `--skip-bootc-upgrade`).

## The promotion model

1. **Merge to `main`** → CI publishes `:latest`. Devices you provisioned on the default
   channel (dev boxes, QA rigs) auto-update on their timer. This is your canary.
2. **Tag a proven commit `vX.Y.Z`** → the tag pipeline **promotes** the build of that
   commit that canaried on `:latest` to `:stable`, without rebuilding. Production devices
   (provisioned with `--channel stable`) auto-update only now, to the exact image the
   canary fleet ran.

The tag pipeline runs two steps:

1. [`ci/find_version.py`](../scripts/find_version.py) finds that build: the most recently
   pushed version tag whose `g<short-sha>` is the tagged commit. It ranks by push time,
   read from the platform's package API (GitHub's package versions, GitLab's container
   registry API with the job token), not by the tag text. A later base-image refresh of
   `latest` for the same commit is a newer build of it and wins.
2. [`bootcher promote <version> --channel stable`](../../../docs/src/reference/commands.md#bootcher-promote-version---channel-channel---tls-verify-bool)
   points `:stable` at that build's exact digest. With signing, it first verifies the
   build's signature, and the signature made when it was deployed to `latest` covers
   `:stable` too, since both tags name the same digest.

If no build of the tagged commit was published (it never reached `latest`, or cleanup
has pruned it), the run **fails** with "deploy it to `latest` first" rather than
rebuilding: push the commit to the default branch, let it canary, then tag it. Run the
tag's pipeline by hand if you do want a fresh build on `:stable`; that path still
rebuilds.

To roll `:stable` back, or promote a build by hand, run `bootcher promote` from your
machine with any earlier version tag:

```bash
bootcher promote 20261106.142233.g0a1b2c3d4e5 --channel stable
```

## Setup

Beyond the [base recipe's setup](../README.md), **declare the channel** in `bootcher.toml`
so `--channel stable` validates (a typo is rejected, not silently minted):

```toml
[deploy]
registry = "ghcr.io/<owner>"        # or registry.gitlab.com/<group>/<project>
channels = ["stable"]               # the named channel beyond the implicit `latest`

[targets]
x86_64  = "qcow2"
aarch64 = "raw"
```

`latest` is always a valid channel without being listed, and is the default when no
`--channel` is given — so the `main` path needs no manifest change. Reuse the **same**
[`bootcher.ci.toml`](../bootcher.ci.toml) override as the base recipe (it only swaps the
builder networking; `channels` is inherited from `bootcher.toml` via `extend`).

| Platform | File | Copy it to |
|---|---|---|
| GitHub Actions (deploy) | [`github-actions/deploy.yml`](github-actions/deploy.yml) | `.github/workflows/bootcher-deploy.yml` |
| GitHub Actions (provision) | [`github-actions/provision.yml`](github-actions/provision.yml) | `.github/workflows/bootcher-provision.yml` |
| GitLab CI | [`gitlab-ci.yml`](gitlab-ci.yml) | `.gitlab-ci.yml` (repo root) |
| Both | [`../bootcher.ci.toml`](../bootcher.ci.toml) | `bootcher.ci.toml` (repo root, beside `bootcher.toml`) |
| Both | [`../scripts/`](../scripts/) | `ci/` (repo root) |

Like the base recipe, the deploy pipelines run the shared Python scripts from `ci/`, so
copy the base recipe's [`scripts/`](../scripts/) directory into your repo as `ci/`
alongside the workflow file(s). They're the same scripts: the channel a refresh checks
comes in through the `CHANNEL` environment variable the workflow sets, and the
`promote` job runs [`find_version.py`](../scripts/find_version.py), which only this
variant uses.

## Enrolling a device onto a channel

Channels are a **registry-mode** concept (no LAN equivalent), set on the device at
provision/takeover time from your machine — not in CI:

```bash
bootcher provision --ssh-key admin_key --channel stable   # device follows :stable
bootcher provision --ssh-key admin_key                    # device follows :latest (default)
```

The CI `provision` step bakes the channel into the disk, but you select it differently
per platform: on **GitLab** the trigger picks it (a **tag** pipeline produces
stable-tracking disks, a **default-branch** pipeline latest-tracking ones), while on
**GitHub** provision is a manual `workflow_dispatch` workflow with a **`channel` input**
you choose when you launch it (`latest` or `stable`). Either way the published disk
artifacts are namespaced by channel (e.g. a `disk-stable-<sha>` GitHub Release / a
`stable-<sha>` Package Registry path) so the two audiences' disks never overwrite each
other.

## Adding more channels

`stable` is just the example. Add `next`, `testing`, etc. to `[deploy] channels` and map
them to triggers however you like — e.g. a `next` channel published from a `next` branch,
or a `testing` channel from `pre-release` tags. The pattern is the same: resolve a
`CHANNEL` value (from the trigger for `deploy`, or the `workflow_dispatch` input on
GitHub `provision`) and pass it through — and add each new channel to the provision
input's `options:` list so you can pick it.
