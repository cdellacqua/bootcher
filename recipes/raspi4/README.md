# Recipe: Raspberry Pi 4 UEFI firmware

A `disk.post` hook that embeds the [pftf/RPi4](https://github.com/pftf/RPi4) UEFI
firmware into the ESP of the built **aarch64 raw** image, so a Raspberry Pi 4 can
boot the bootc disk. It's the template for any board that needs files written to a
disk image outside the Containerfile's reach.

## How it works

bootcher runs every hook with a single `BOOTCHER_METADATA` environment variable —
a JSON object describing the phase and the artifacts it concerns (see the
[`bootcher.toml` reference](../../docs/src/reference/bootcher-toml.md#bootcher_metadata)).
At `disk.post` it lists each built disk, including the resolved file path:

```json
{
  "phase": "disk",
  "stage": "post",
  "targets": [
    { "arch": "aarch64", "disk_type": "raw", "dir": "output/aarch64/raw", "file": "output/aarch64/raw/disk.raw" }
  ]
}
```

[`embed-firmware.sh`](embed-firmware.sh) picks the aarch64 raw disk straight out
of that JSON with `jq`, then loop-mounts its ESP and overlays the firmware — so the
recipe never has to know bootcher's output layout, and is inert for a project that
doesn't build such a target.

## Wire it in

Add to your project's `bootcher.toml`:

```toml
[targets]
aarch64 = "raw"

[hooks.disk]
post = "recipes/raspi4/embed-firmware.sh"
```

Then `bootcher disk` (or `bootcher provision`) builds the image and runs the hook.

## Requirements

- `jq`, `curl`, `unzip`, `python3`, `sfdisk`
- `sudo` (the hook loop-mounts the raw image's ESP; bootcher hands the hook the
  terminal, so a `sudo` prompt works)

## Customising

- Pin a different firmware build by editing `FIRMWARE_URL` in the script.
- Targeting another board? Copy this directory, adjust the `select(...)` filter for
  the arch/disk_type you build, and swap the firmware steps.
