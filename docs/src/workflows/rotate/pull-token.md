# Pull token rotation

Replaces the registry pull token (`/etc/ostree/auth.json`) on every device in `[deploy] remotes`. Use this when a pull credential has expired or been revoked and you need to push a new one without reprovisioning.

*Registry mode only.* LAN deployments use no pull token and are not affected by this command.

## Command

```sh
bootcher rotate pull-token
```

### Flags

| Flag | Purpose |
|---|---|
| `--skip-pull-check` | Skip the pre-flight local validation (the per-device check still runs) |

### Environment variables

| Variable | Purpose |
|---|---|
| `BOOTCHER_PULL_USER` | Registry username |
| `BOOTCHER_PULL_TOKEN` | Registry pull token |

## What happens

1. The new credential is collected once, up front — env vars if set, else a TTY prompt, same as `provision`.
2. Unless `--skip-pull-check`, the new token is verified against the registry from the build machine before any device is contacted.
3. For each device in parallel:
   a. The candidate `auth.json` is staged to a temp file on the device.
   b. `podman manifest inspect --authfile <tmp>` confirms it authenticates and can reach the project image.
   c. Only on success is the temp file installed over `/etc/ostree/auth.json`.

A failed verification on a device aborts that device's rotation without committing anything — the existing token stays live and the device remains functional. Other devices continue in parallel.
