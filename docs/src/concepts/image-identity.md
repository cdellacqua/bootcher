# Image identity & provenance

A bootcher project is a single image, published as a multi-arch manifest list and tracked by every device. This page covers how that image is *named* (the registry tags), how it's *stamped* (the OCI provenance labels), and how to read either back — from a workstation or on a running device.

## The two registry tags

Each registry-mode push publishes the same manifest digest under two tags:

- **`:latest`** — the mutable channel every device tracks. `bootc upgrade` and the auto-update timer (`bootc-fetch-apply-updates.timer`) resolve this, so a push to `:latest` is what rolls out.
- **`:<version>`** — an immutable [CalVer](https://calver.org/) tag combining a UTC timestamp with the git commit, `YYYYMMDD.HHMM.g<short-sha>` (e.g. `20260113.1233.g0a1b2c3d4e5f`). The date keeps it human-readable and sortable; the commit suffix is what makes it a *stable identity* rather than a mere clock reading — it pins the exact source the image was built from, so the tag can never mean two different trees. (A build from an uncommitted tree keeps git's `-dirty` marker on the suffix, so it can't masquerade as the clean commit.) It points at the same digest as the `:latest` it accompanied, but never moves — a durable handle for rollback and audit that a registry garbage-collection pass won't reap (an untagged digest can be). To pin a device to a specific past build, point its bootc origin at the version tag instead of `:latest`.

  When the project isn't in git, there's no commit to pin to, so the tag falls back to a timestamp alone at minute resolution, `YYYYMMDD.HH.MM`. Two separate deploys within the same minute then collide on the tag, and since each rebuild produces a fresh digest, the later one silently re-points it — a residual mutability window the commit suffix is exactly what closes. UTC throughout keeps tags from different builder machines sorting and comparing unambiguously.

This is bootcher's whole versioning story: there's no `version` field to bump and no release ritual — the timestamp tag is generated on every push, and devices continuously track `:latest`. CalVer fits that continuous-rollout model, where nobody chooses a version to upgrade *to*.

## Provenance labels

On top of the tags, every built container is stamped with standard [OCI image labels](https://github.com/opencontainers/image-spec/blob/main/annotations.md) recording the git commit it came from:

| Label | Source | Notes |
|---|---|---|
| `org.opencontainers.image.revision` | `git rev-parse HEAD` | The commit SHA, `-dirty`-suffixed when the work tree has uncommitted changes — so a label can't silently claim a clean commit the image wasn't built from. |
| `org.opencontainers.image.version` | `git describe --tags --always --dirty` | A human description: the nearest tag (with commit distance/SHA when ahead of it), or a short SHA when the repo is untagged. |

Detection is best-effort: both labels are omitted when the project isn't a git work tree, `git` isn't installed, or the repo has no commit yet — a non-git project just builds without provenance rather than failing. The labels are computed once on the build host and stamped identically whether the container is built in-process or on a remote/VM builder (which has no `.git` of its own).

The same values are handed to lifecycle hooks as `revision` / `version` in [`BOOTCHER_METADATA`](../reference/bootcher-toml.md#bootcher_metadata), so a `build.post` or `disk.post` recipe can read them straight from the JSON.

## Reading the provenance

From any machine with registry read access — to see what a tag was built from without touching a device:

```sh
podman search --list-tags <registry>/<name>     # every immutable :<version> build

# Read a tag's provenance labels. podman inspects local storage, so pull first:
podman pull <registry>/<name>:latest
podman image inspect <registry>/<name>:latest --format '{{ json .Config.Labels }}'
```

The labels live in the image config, so `podman` has to pull the image to read them (`podman manifest inspect` shows the manifest, not the config labels). If you only want the metadata and have [skopeo](https://github.com/containers/skopeo), `skopeo inspect docker://<registry>/<name>:latest | jq '.Labels'` reads them without a pull.

On a device, `bootc status` reads the booted image's metadata directly. It surfaces the `org.opencontainers.image.version` label as its own `version` field, so the most common check needs nothing else:

```sh
bootc status                                                     # human-readable: image ref, digest, version
bootc status --format=json | jq -r '.status.booted.image.version'   # just the version label, for scripting
```

`bootc status` only breaks out `version` (and the image's creation timestamp), not arbitrary labels. To read the full label set — e.g. `org.opencontainers.image.revision` — make the booted image visible to podman and inspect its config:

```sh
sudo bootc image copy-to-storage   # copies the booted image into containers-storage (default: localhost/bootc)
sudo podman image inspect localhost/bootc --format '{{ json .Config.Labels }}'
```
