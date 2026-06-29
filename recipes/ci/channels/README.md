# Recipe: CI/CD with release channels (`latest` + `stable`)

A channels-aware variant of the [base CI recipe](../README.md). Same three-stage,
multi-arch pipeline (**build → deploy → provision**, native per-arch runners, loopback
container store, path gate) — the only addition is that **the trigger picks the
[release channel](../../../docs/src/reference/bootcher-toml.md#channels)** a push
publishes to:

| Trigger | Channel | Audience |
|---|---|---|
| push to the default branch (`main`) | `latest` (bootcher's default — no flag) | rolling / **canary** fleet |
| push a `v*` git tag | `stable` (`bootcher deploy --channel stable`) | **production** fleet |

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
2. **Tag a proven commit `vX.Y.Z`** → the tag pipeline rebuilds that commit and publishes
   it to `:stable`. Production devices (provisioned with `--channel stable`) auto-update
   only now.

Because the tag is on the same source commit that rode `latest`, the stable image is that
validated revision, and `git describe` stamps `vX.Y.Z` as its version label. bootcher
**rebuilds on the tag** rather than re-tagging `main`'s exact digest — same source, fresh
`CalVer`; the immutable `CalVer` tag is the per-build identity either way. If you need
bit-identical promotion (no rebuild), pull `main`'s `CalVer` image and `podman manifest
push` it to `:stable` by hand instead of tagging.

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
| GitHub Actions | [`github-actions.yml`](github-actions.yml) | `.github/workflows/bootcher.yml` |
| GitLab CI | [`gitlab-ci.yml`](gitlab-ci.yml) | `.gitlab-ci.yml` (repo root) |
| Both | [`../bootcher.ci.toml`](../bootcher.ci.toml) | `bootcher.ci.toml` (repo root, beside `bootcher.toml`) |

## Enrolling a device onto a channel

Channels are a **registry-mode** concept (no LAN equivalent), set on the device at
provision/takeover time from your machine — not in CI:

```bash
bootcher provision --ssh-key admin_key --channel stable   # device follows :stable
bootcher provision --ssh-key admin_key                    # device follows :latest (default)
```

The `provision` job in each pipeline bakes whichever channel matches its trigger, so a
**tag** pipeline produces stable-tracking disks and a **default-branch** pipeline
latest-tracking ones. The published disk artifacts are namespaced by channel (e.g. a
`disk-stable-<sha>` GitHub Release / a `stable-<sha>` Package Registry path) so the two
audiences' disks never overwrite each other.

## Adding more channels

`stable` is just the example. Add `next`, `testing`, etc. to `[deploy] channels` and map
them to triggers however you like — e.g. a `next` channel published from a `next` branch,
or a `testing` channel from `pre-release` tags. The pattern is the same: resolve a
`CHANNEL` value from the trigger and pass it to `deploy`/`provision`.
