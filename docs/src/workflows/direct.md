# Direct devices

The simplest way to run a fleet: no registry, nothing hosted. You build on your machine
and push to the devices over SSH. A project without a `[registry]` is direct-only; one
with a registry can still keep some devices direct (the bench boards next to you) — see
[Direct and channel devices](../concepts/devices.md).

```sh
bootcher init my-os                  # "publish to a registry?" → no
bootcher device provision            # --direct is implied without a registry
# … write the disk, boot the device, list it in `remotes`
bootcher device deploy               # build here, push to the direct devices
bootcher device status               # what each device runs
bootcher device rotate ssh-key       # roll the admin key
bootcher device refresh              # base-image patches: rebuild what each device runs
```

## Deploying

```sh
bootcher device deploy
```

`device deploy` builds the current checkout (dirty trees welcome — this is the inner
loop) into `localhost/<name>:latest-<arch>`, assembles `localhost/<name>:latest`, and
ships it to every **direct** device in `remotes`, in parallel (`[concurrency] upgrade`):

1. identify the device's arch and pick the matching member;
2. transfer it, unless the device already stores it: the builder serves the image from a
   loopback-only registry, and the device pulls it through an `ssh -R` tunnel, so only
   the missing layers move and the bytes never cross the network in the clear;
3. `bootc switch --transport containers-storage localhost/<name>:latest`, `bootc upgrade`;
4. reboot into it and check with `bootc status` that it booted it (a device that came
   back on its old deployment fails the run; one already running the image isn't
   rebooted).

Channel and foreign devices are skipped and logged with their class; `--remote <name>`
naming one of them is an error pointing at `device switch --direct` — `deploy` never
moves a device. With no direct device at all, `deploy` says so and points at `release
publish`.

On a TTY, `deploy` shows a checklist of the direct devices (all pre-checked; the others
shown but not selectable) and prints the equivalent command. Off a TTY, `-y` is required.
`--skip-build` ships the images already in local storage — after a `bootcher build`, or
images a CI job `podman load`ed.

Rollback is `bootc rollback` on the device: direct devices keep no release history.

## Base-image patches

```sh
bootcher device refresh
```

`device refresh` rebuilds **the commit each direct device booted** — never your current
checkout — on today's base and ships it back to just the devices on that commit. It reads
each device's `bootcher.source` from the device, groups the devices by commit, rebuilds
each commit in a temporary worktree under that commit's own config, and runs the direct
device step. Commits whose devices are all on the current base are skipped (`--force`
rebuilds anyway, without cache).

A device deployed from a **dirty tree** records no commit: it's listed and skipped, with
a hint to re-run `device deploy`. Deploy from a clean commit when you want a device to be
refreshable. On a TTY the checklist groups devices by commit and pre-checks the stale
ones; off a TTY `-y` is required. See [Base-image patches](../concepts/base-patches.md).

## Lifecycle hooks

`[hooks.build]` wraps the build and `[hooks.upgrade]` the per-device rollout; see
[`[hooks.<phase>]`](../reference/bootcher-toml.md#hooksphase).

```toml
[hooks.upgrade]
pre  = "sh drain.sh"
post = "sh smoke-test.sh"     # runs after every device was attempted, with each outcome in `results`
```
