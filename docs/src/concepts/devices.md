# Direct and channel devices

Every device bootcher manages is listed in the top-level `remotes` array of
`bootcher.toml`. There is no per-device mode or channel in the config: before every
device step, bootcher reads what each device actually follows from the device itself
(`bootc status`, its `spec.image` — what the next upgrade pulls) and acts on that.

| Class | The device follows | Updated by |
|---|---|---|
| **channel `ch`** | `<registry>/<name>:<ch>` (registry transport) for a channel declared in `[registry] channels`, with or without signature verification | its own auto-update timer, or a [channel device step](#device-steps) after `channel promote`/`refresh` |
| **direct** | `localhost/<name>:latest`, under any transport | `bootcher device deploy` / `device refresh` from a machine that can reach it over SSH |
| **foreign** | anything else: another image or name, an undeclared tag, a digest pin | nothing — bootcher leaves it alone |
| **unreachable** | (no SSH, or an unreadable status) | — listed, never blocking the others |

`bootcher device status` shows each device's class, origin, booted and staged digests,
its `version` label and — for a channel device — which release it runs and whether it
lags its channel:

```console
$ bootcher device status
DEVICE           CLASS     ARCH     RELEASE  STATE                         VERSION
admin@rpi-1      direct    aarch64  -        direct                        v1.4-3-g9f3a1c2-dirty
admin@kiosk-2    channel   aarch64  v1.3     lags :stable (channel serves v1.4)  v1.3
admin@vps1       channel   x86_64   v1.4     current on :stable (v1.4)     v1.4
admin@vps3       unreachable -      -        unreachable                   -
```

A freshly provisioned **cross-arch** channel device boots a small local derivative of the
release (a build-time crutch the local cross-arch disk build adds), so no published
digest matches it; `device status` shows it as "provisioned, not yet updated" rather than
foreign, and its first update brings it onto the release.

Renaming `[general] name`, or removing a channel from `[registry] channels`, makes the
devices that followed it **foreign**. Bring them back with
[`device switch`](../workflows/moving.md).

## Direct devices

A direct device runs what you build on your machine. `bootcher device deploy` builds the
current checkout — dirty trees welcome, this is the inner loop — and ships it over SSH:
the builder serves the image from a loopback-only registry and the device pulls it
through an `ssh -R` tunnel, so only the missing layers cross the wire, the bytes ride the
trusted SSH channel, and the device needs no registry configuration or credential. The
device stores it as `localhost/<name>:latest` and `bootc switch`es to it under the
`containers-storage` transport.

**When to use:** the bench board on your desk, an air-gapped LAN, or any project without
a registry. A project with no `[registry]` has only direct devices. Direct devices keep no
release history, aren't covered by [signing](signing.md), and get base patches only when
someone runs [`device refresh`](base-patches.md#direct-devices) from a machine that can
reach them — CI can't. Rollback is `bootc rollback` on the device.

## Channel devices

A channel device follows `<registry>/<name>:<ch>` and updates itself on its
`bootc-fetch-apply-updates.timer`: moving the channel *is* the deploy. You move it with
[`channel promote`](releases.md#promoting), from CI or a workstation; base patches arrive
with [`channel refresh`](base-patches.md#channel-devices). Devices outside `remotes` (an
edge fleet behind NAT) simply aren't listed — they self-update like any other channel
device.

## Mixed fleets

One project can have both: the bench boards direct, the field units on `:stable`, a
canary rack on `:next`. Each command reaches exactly its own devices and never moves a
device between them:

| Command | Default targets | `--remote` |
|---|---|---|
| `device deploy` | direct devices | restricts within them; naming a non-direct one is an error pointing at `device switch --direct` |
| `device refresh` | direct devices whose booted base is stale | restricts within direct devices |
| `device switch` | none: a TTY checklist; off a TTY `--remote` is required | the devices to move |
| `device takeover` | none: a TTY checklist; off a TTY `--remote` is required | the hosts to convert |
| `channel promote`/`refresh`, `release publish --promote` | devices on the moved channel(s) | restricts within them |
| `device rotate ssh-key` | every reachable remote | the subset |
| `device rotate pull-token` | channel devices | the subset |
| `signing enroll`/`rotate`/`retire` | channel devices | the subset |

Skipped devices are logged with their class (`admin@kiosk-2: on :next, skipped`), and a
run that skips every device says so. Unreachable devices never fail silently and never
block the others: the reachable ones are attempted, the unreachable ones listed, and the
run exits non-zero afterwards. `--remote` names a device by its full destination or its
bare host.

Only [`device switch`](../workflows/moving.md) moves a device: onto a channel, or to
direct.

## Device steps

Every device-touching command is built from two steps, both idempotent — re-running one
on a device already there stages nothing and doesn't reboot it, which is what lets a
re-run retry exactly the devices a failed run missed:

- **The direct device step**: transfer (skipped when the device already stores the
  image), `bootc switch --transport containers-storage localhost/<name>:latest`, `bootc
  upgrade`, reboot.
- **The channel device step**: with signing on, make sure the
  [trust files](signing.md#on-device-files) are current; then `bootc switch
  [--enforce-container-sigpolicy] <registry>/<name>:<ch>`, `bootc upgrade`, reboot.

After a reboot bootcher checks with `bootc status` that the device booted what was
staged; a device that came back on its old deployment fails the run. Device steps run in
parallel, capped by `[concurrency] upgrade`. Every connection uses trust-on-first-use
host-key checking; on an untrusted network, pin the host key instead — see
[Host-key verification](../reference/bootcher-toml.md#host-key-verification).

## Two credentials for channel devices

| Credential | Who holds it | Privilege | How it gets there |
|---|---|---|---|
| **Pull token** | every channel device (unless the registry is public) | read-only | baked at `device provision --channel`/`takeover --channel`, installed by `device switch --channel`, rotated with `device rotate pull-token` |
| **Push auth** | your workstation / CI | read-write | `podman login <registry>` (and `skopeo login`), never on a device |

Use a least-privilege, read-only token for the pull credential (e.g. a GitLab deploy token
with `read_registry`, created under *Settings → Repository → Deploy tokens*). bootcher
leaves push auth to `podman login`, which writes `~/.config/containers/auth.json` (or
`$REGISTRY_AUTH_FILE`), read by podman and skopeo alike.
