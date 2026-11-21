# Base-image patches

The scaffold's `Containerfile` builds `FROM` a fedora-bootc tag that keeps moving with
Fedora's updates, but your image only picks those up when it's rebuilt. bootcher keeps
both kinds of device patched **without shipping unreleased source**: it rebuilds the
commit a device or channel already runs, on today's base.

Every user-facing build passes `podman build --pull=newer`, so a cached base is never
silently reused.

## Is the base stale?

One check, shared by `channel status`, `release list`, `channel promote` and the
refreshes: for each member, compare the `org.opencontainers.image.base.digest`
annotation buildah recorded at build time with the base tag's **current** digest for
that arch (the matching platform entry of the base's index). Equal means current;
different means stale; anything unverifiable — no annotations, a base that can't be
read — counts as moved, so a broken check never costs you a security update.

## Channel devices

```sh
bootcher channel status                    # per channel: release, commit, base current/stale
bootcher channel refresh                   # rebuild what each channel serves, re-copy it
```

`channel refresh` (default: every declared channel):

1. reads the release and its source commit from each `:ch`, and groups the channels by
   release;
2. for each release whose base moved (or every one with `--force`, which also builds
   `--no-cache`), checks the commit out into a temporary `git worktree` (fetched by SHA
   if missing, submodules initialised), loads **that commit's** config from the same
   relative `--manifest` path, and builds the stale arches tagged
   `localhost/<name>:refresh-<sha12>-<arch>` — never touching `latest-<arch>`. It fails
   loudly if the result still isn't on the current base;
3. assembles the rebuilt members plus the release's untouched ones (added by digest, no
   pull), annotates the index and pushes it over `:X`, signed;
4. copies `:X` to every selected channel serving it whose digest differs — re-checking
   each channel first, and leaving alone one a promote moved in the meantime;
5. runs the channel device step for the selected channels (`--skip-remotes` skips it).

`--exclude <ch>` pauses a channel; releases with no recorded source are skipped with a
warning. With `--skip-build` (the CI path) the selected channels must serve a single
release, and the local `latest-<arch>` images — each recording that release's commit —
are published instead of building.

The root config decides *what* to refresh and owns publishing (registry, signing key,
remotes); each release builds under **its own** commit's config — no merging, no
fallback. Gitignored or generated build inputs don't exist in a worktree, and the
worktree's own `[hooks.build]` runs. A refresh builds old source with the current
bootcher: a breaking change to the build sections, or a commit without the manifest the
CI uses, makes that refresh fail loudly — publish a new release then.

`release refresh X` is the same rebuild-and-push for one release, moving no channel —
the first half of the rollback flow (`release refresh v1.3`, then `channel promote v1.3
stable`).

Scheduled CI runs `channel status <ch>` per channel and refreshes only when it reports
stale; see the [CI recipes](../recipes/index.md).

## Direct devices

```sh
bootcher device refresh
```

`device refresh` does the same for direct devices, with the devices playing the
registry's role:

1. for every reachable direct device, it reads the **booted** image's
   `bootcher.source`, arch and base annotations straight from the device's ostree repo
   (`ostree container image metadata`), which works even on a never-deployed disk and
   copies nothing;
2. it groups the devices **by source commit**, not by digest: two devices on one commit
   can run different digests (one already refreshed, or a cross-arch disk booting its
   local derivative). Each commit gets one worktree build, for the arches its devices
   need — skipped when all its devices already have a current base, unless `--force`;
3. it builds each commit in a temporary worktree under that commit's config, tagged
   `localhost/<name>:refresh-<sha12>-<arch>`, and ships it with the direct device step
   to just the devices on that commit.

A device deployed from a dirty tree (or a project outside git) records no source and is
listed and skipped with a hint to re-run `device deploy`. `device refresh` needs a build
environment but no registry, and no clean checkout — nothing is built from it. Because
it has to reach the devices, CI can't do it: run it from a workstation.

## When the base stops moving

The scaffold pins one Fedora release (`ARG FEDORA_VERSION`), and that release's tag stops
moving at end of life. From then on nothing is ever stale again, and devices stop getting
security updates while every check stays green. The CI recipes' `release-check` job warns
once a newer release is out and fails when the pin is two releases behind; see
[Fedora releases](../workflows/setup.md#fedora-releases).
