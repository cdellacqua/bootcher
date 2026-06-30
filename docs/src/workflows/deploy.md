# Deploy updates

After first provisioning, rolling an update to running devices is:

```sh
bootcher deploy
```

`deploy` builds the container and then pushes it + triggers `bootc upgrade`, in sequence. Pass `--skip-build` to skip the container build and ship an image an earlier `bootcher build` already produced.

## What deploy does

1. **Build** the container image with `podman build` (same as `bootcher build`; skipped with `--skip-build`).
2. Collect the freshly-built per-arch images into a local multi-arch manifest list (`localhost/<name>:latest`).
3. **Push + upgrade** — push the update and trigger `bootc upgrade` on each device. The push backend depends on the manifest:
   - **LAN mode** — serve the image from a loopback registry and forward it over SSH; see [LAN backend](../concepts/deploy-backends.md).
   - **Registry mode** — `podman push` to `<registry>/<name>:latest`, *and* to an immutable `<registry>/<name>:<version>` tag for the same digest (see [Image identity & provenance](../concepts/image-identity.md)); SSH into each `[deploy] remotes` device and run `bootc upgrade`.

In registry mode with no `[deploy] remotes` configured, the push to the registry is the whole operation. Devices self-update on their `bootc-fetch-apply-updates.timer` schedule.

Both tags carry the git commit they were built from as OCI labels, and you can read either tag or label back from a workstation or a running device — see [Image identity & provenance](../concepts/image-identity.md).

## Skipping the build

Every action command builds the container first by default. `--skip-build` opts out, acting on an image an earlier `bootcher build` already produced:

| Command | What it does |
|---|---|
| `bootcher build` | Build the container image and assemble the local multi-arch manifest list |
| `bootcher deploy --skip-build` | Push the already-built image and trigger `bootc upgrade` on remotes (no rebuild) |
| `bootcher provision --skip-build` | Build the disk artifact from the already-built container, pulling it from the registry when it isn't already local (no rebuild) |
| `bootcher takeover --skip-build` | Convert live hosts from the already-built container (no rebuild) |

These are useful when iterating on a single phase, or when orchestrating a pipeline that runs `build` and the deploy/provision step as separate jobs.

For `provision --skip-build` the "already-built container" can come from the registry, not just local storage: if the per-arch member isn't in the local store, it's pulled from `<registry>/<name>:latest` (the multi-arch list `deploy` pushes) and used as-is. That lets a manual provision job in CI reuse the exact image the automatic `deploy` already built and pushed — no second container build, and the flashed disk is byte-for-byte what devices auto-update to. It's registry mode only (there's nowhere else to pull from) and the machine needs registry read access for a private registry; with no registry a missing local image is still an error pointing back at `bootcher build`.

## Lifecycle hooks

Each phase can be wrapped with `pre` / `post` shell commands defined in `bootcher.toml`. Hooks fire wherever the phase runs — so `[hooks.build]` wraps the container build in both `build` and `deploy`.

```toml
[hooks.build]
pre  = "echo 'prepare build artifacts'"
post = "echo 'clean up after build'"

[hooks.disk]
pre  = "echo 'pre-disk step'"
post = "dd if=output/... of=/dev/sdX   # embed into target"

[hooks.upgrade]
pre  = "echo 'drain traffic before rollout'"
post = "echo 'run post-deploy smoke test'"
```

Each hook runs with `sh -c` from the project root. The terminal is handed over — the hook may print freely, prompt, or `sudo`. A non-zero exit aborts the run.

## Continuous deployment

`deploy` (push an update to the registry) and `provision` (build a disk artifact)
both run unattended given the right env: the admin key via `--ssh-key`, the pull
credential via `BOOTCHER_PULL_USER` / `BOOTCHER_PULL_TOKEN`, and a signing
passphrase via `BOOTCHER_SIGN_PASSPHRASE` (so a missing TTY never blocks them).
The [`recipes/ci/`](https://github.com/cdellacqua/bootcher/tree/main/recipes/ci)
recipe has copy-pasteable GitHub Actions and GitLab CI pipelines that wire this up
against the registry associated with your repo; the
[channels variant](https://github.com/cdellacqua/bootcher/tree/main/recipes/ci/channels)
adds a `latest`→`stable` promotion flow. See [Recipes](../recipes/index.md) for the
full catalog.

## Multi-arch builds

If `[targets]` lists more than one architecture, `build` fans out per-arch builds in parallel (up to `[concurrency] build` workers), then assembles them into a single multi-arch manifest list. `upgrade` does the same for the per-device SSH rollout (up to `[concurrency] upgrade` workers).

## Signing

In registry mode with signing configured (via `[deploy] registry` inline table), the `upgrade` step signs the pushed manifest list before any device sees it. See [Image signing](../concepts/signing.md).
