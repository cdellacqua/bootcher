# Moving devices

`bootcher device switch` is the only command that moves a device: onto a channel, or to
direct. Every other device step acts on the devices already in its class and never
changes what a device follows.

## Direct → channel

```sh
bootcher device switch --channel stable --remote bench1
```

`:stable` must be declared and published. For each device:

1. a pull credential, if the device has none (`/etc/ostree/auth.json`): one credential is
   collected for the whole run (from `BOOTCHER_PULL_USER`/`BOOTCHER_PULL_TOKEN`, or
   prompted), checked from this machine unless `--skip-pull-check`, then verified on each
   device against the channel before it's committed — as `device rotate pull-token`
   does. `--anonymous` skips it for a public registry;
2. with signing on, the trust files;
3. the channel device step: `bootc switch [--enforce-container-sigpolicy] :stable`,
   upgrade, reboot.

A device already on `:stable` is a no-op.

## Channel → direct

```sh
bootcher device switch --direct --remote kiosk-3 [--skip-build]
```

Builds here (or `--skip-build`) and runs the direct device step: ships the image over SSH
and switches the origin to `localhost/<name>:latest` under `containers-storage`. With
signing on, the confirmation warns that the device stops verifying signatures — a direct
device's trust is the SSH channel.

## Selection and confirmation

The confirmation shows `current → target` for each device. On a TTY with no mode, you
pick a channel or "direct"; with no `--remote`, a checklist lists the devices not already
there, none pre-checked. Off a TTY, both the mode and `--remote` are required, and so is
`-y`.

A device made **foreign** by renaming the project or a channel comes back the same way:
`device switch --channel <ch> --remote <it>`.
