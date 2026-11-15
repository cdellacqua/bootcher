# Image identity & provenance

A bootcher project is a single image, published as a multi-arch manifest list and tracked by every device. This page covers how that image is *named* (the registry tags), how it's *stamped* (the OCI provenance labels), and how to read either back — from a workstation or on a running device.

## Registry tags

A registry-mode push publishes the multi-arch manifest list under one or two tags:

- **A channel** — `:latest` by default, or `:<channel>` with `deploy --channel <channel>` (see [`channels`](../reference/bootcher-toml.md#channels)). A channel is mutable: it moves to each new build deployed to it. `bootc upgrade` and the auto-update timer (`bootc-fetch-apply-updates.timer`) resolve the channel a device tracks, so a push to it is what rolls out.
- **A build tag** — `:<tag>` with `deploy --tag <tag>`. You pick the name (a git tag, a CI counter, anything that is a valid OCI tag); bootcher doesn't generate one. It names that one build, for testing it on a single device (`bootc switch <registry>/<name>:<tag>`), for rollback, or to move a channel onto it later without rebuilding. bootcher doesn't stop a later `--tag` push from reusing a name, so keeping build tags immutable is up to the pipeline that picks them.

  `--tag` alone pushes only the build tag: no channel moves and no `[deploy] remotes` device is upgraded. `--tag <name> --channel <channel>` pushes the build tag **first** and moves the channel only once it has landed, for the same digest, so a build devices follow always has its durable name. A build tag can't be `latest` or a declared channel, and can't start with `ci-` (scratch tags in CI pipelines) or `sha256-` (where signature attachments are stored).

With signing configured, the first push is the signed one (the build tag with `--tag`, otherwise the channel). Signatures are keyed by manifest digest, so the one signature covers every tag for that digest, including a channel later moved onto it with a digest-preserving copy.

A plain `deploy` without `--tag` pushes the channel alone, so nothing keeps a build that a channel has since moved past: use `--tag` for builds you may need to return to. `takeover` likewise pushes only its channel.

## Provenance labels

On top of the tags, every built container is stamped with standard [OCI image labels](https://github.com/opencontainers/image-spec/blob/main/annotations.md) recording the git commit it came from:

| Label | Source | Notes |
|---|---|---|
| `org.opencontainers.image.revision` | `git rev-parse HEAD` | The commit SHA, `-dirty`-suffixed when the work tree has uncommitted changes — so a label can't silently claim a clean commit the image wasn't built from. |
| `org.opencontainers.image.version` | `git describe --tags --always --dirty` | A human description: the nearest tag (with commit distance/SHA when ahead of it), or a short SHA when the repo is untagged. |

The same two keys are also written as manifest annotations on each per-arch image, so tools that read annotations rather than config labels (registry UIs, scanners, `skopeo inspect --raw`) see the same values.

Detection is best-effort: both labels are omitted when the project isn't a git work tree, `git` isn't installed, or the repo has no commit yet — a non-git project just builds without provenance rather than failing. The labels are computed once on the build host and stamped identically whether the container is built in-process or on a remote/VM builder (which has no `.git` of its own).

The same values are handed to lifecycle hooks as `revision` / `version` in [`BOOTCHER_METADATA`](../reference/bootcher-toml.md#bootcher_metadata), so a `build.post` or `disk.post` recipe can read them straight from the JSON.

## Reading the provenance

From any machine with registry read access — to see what a tag was built from without touching a device:

```sh
podman search --list-tags <registry>/<name>     # every channel and build tag

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
