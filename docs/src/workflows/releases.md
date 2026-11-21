# Publishing releases

With a `[registry]`, devices follow channels and you move channels by promoting
releases. See [Releases and channels](../concepts/releases.md) for the model.

```sh
bootcher init my-os                  # registry → yes, channels → stable, next
bootcher device deploy               # bench boards stay direct: a mixed fleet is normal
git tag v1.4 && git push --tags      # CI: per-arch build → release publish v1.4 --skip-build
bootcher release publish v1.4 --promote next     # …or the same thing locally
bootcher channel promote v1.4 stable             # CI manual job, or locally with the device step
bootcher device provision --channel stable --target aarch64 --disk raw
bootcher device takeover --channel stable --remote vps1
# scheduled CI: channel status → channel refresh (base patches)
bootcher channel promote v1.3 stable             # rollback; refuses a stale base…
bootcher release refresh v1.3                    # …so refresh it first
```

## Configure the registry

```toml
[registry]
url = "registry.example.com/org/project"   # images are <url>/<name>:<tag>
channels = ["stable", "next"]              # required, non-empty
```

Push auth is yours to manage with `podman login <registry>` (skopeo reads the same
credentials). The release, channel and signing commands need `skopeo` as well as `podman`.

## Publish

```sh
git commit -am "…"
bootcher release publish v1.4
```

- A dirty tree is refused, as is a name that isn't a valid OCI tag, is a declared
  channel, or starts with `sha256-`/`ci-`, and a git tag `v1.4` at another commit.
- `:v1.4` absent: every arch is built (or `--skip-build` takes the local `latest-<arch>`
  images, each of which must record HEAD), the index must hold exactly one member per
  `[targets]` arch, and it's pushed signed as an OCI index annotated `bootcher.release`.
  `[hooks.publish]` wraps the push.
- `:v1.4` already records HEAD: "already published", nothing is built — retries are free.
  `--rebuild` rebuilds it anyway.
- `:v1.4` records another commit (or none): refused.
- Not a git repository: allowed only while `:v1.4` is absent; it then records no source
  and can never be overwritten or refreshed.

Afterwards:

- `--git-tag push|local|no` — create an annotated git tag `v1.4` at HEAD (and push it to
  the branch's upstream remote, else `origin`). Off a TTY the default is `no`; on a TTY
  you're asked, with "don't create" pre-selected. With the CI recipes, `--git-tag push`
  triggers a redundant CI build of the same tag: use `--git-tag local` and push the tag
  yourself, or let CI publish.
- `--promote <ch>` — run `channel promote v1.4 <ch>` (device step included; `-y`,
  `--remote`, `--skip-remotes` apply). On a TTY you're offered it, "don't promote"
  pre-selected.

On a TTY with no name, `publish` asks for one, pre-filled with the git tag at HEAD (if
exactly one), validated as you type; a name already published from HEAD offers to
promote it instead.

## Promote

```sh
bootcher channel promote v1.4 stable
```

Checks `:v1.4` exists, is signed by the configured key (signing on) and is on the current
base (`--allow-stale-base` overrides), then copies it onto `:stable` by digest and prints
what `:stable` served before — your rollback target. It then updates the devices on
`:stable` (the channel device step), even when the channel didn't move;
`--skip-remotes` leaves them to their auto-update timer. Moving a channel needs `-y` off a
TTY — even with `--skip-remotes`, since a moved channel moves the fleet on its next
auto-update. It needs neither the signing key nor a build environment.

On a TTY, a missing release is completed from `release list`, a missing channel picked
from an annotated list (`stable: serves v1.3`), a stale base offers "cancel"
(pre-selected) or "promote anyway", and the confirmation lists the affected devices.

## Base patches and rollback

```sh
bootcher channel status                    # stale? lagging?
bootcher channel refresh                   # rebuild what each channel serves on the current base
bootcher release refresh v1.3              # rebuild one release; moves no channel
```

See [Base-image patches](../concepts/base-patches.md). To roll a channel back, promote
the previous release; if its base has moved since, `release refresh` it first.

## Provision and take over channel devices

```sh
bootcher device provision --channel stable --target aarch64 --disk raw
bootcher device takeover --channel stable --remote vps1
```

A channel install never builds: it pulls the members of `:stable` (by digest, every
time) into `localhost/<name>:pull-stable-<arch>`, bakes the pull credential (unless
`--anonymous`) and the trust files (signing on), and records `:stable` as the origin. See
[Project setup](setup.md#2-provision) and [Takeover](takeover.md).
