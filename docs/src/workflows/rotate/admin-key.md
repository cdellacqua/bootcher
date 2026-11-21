# Admin SSH key rotation

Replaces the admin authorized key (`/etc/ssh/authorized_keys.d/admin`) on every reachable device in `remotes` (or the `--remote` subset). Use this when the admin keypair is being rolled. The rotation is **lockout-safe**: both the old and new key are valid simultaneously until the new key is proven to work.

Applies to every device, direct or channel — the admin SSH key is the one credential every device carries.

## Command

```sh
bootcher device rotate ssh-key --ssh-key ~/.ssh/id_ed25519_new -y
```

The `<path>.pub` sibling is injected into the device's `authorized_keys`, and the private key itself is used to validate the new login before the old key is retired.

Omit `--ssh-key` on a TTY to pick from a list of available `~/.ssh/` keys:

```sh
bootcher device rotate ssh-key
```

### Flags

| Flag | Purpose |
|---|---|
| `--ssh-key <path>` | New admin SSH private key; the `<path>.pub` sibling is injected. Omit to pick on a TTY |
| `--remote <name>` | Restrict to these devices (repeatable) |
| `-y` | Proceed without asking (required off a TTY) |

## The lockout-safe protocol

SSH keys are proven by a fresh login only, so the rotation runs as a lockout-safe three-step protocol that keeps the old key valid until the new one is confirmed:

1. **Stage** — write the new key to the device, followed by a sentinel comment (`# bootcher-retiring`), followed by the existing keys. Both old and new keys authenticate now. A crash at this step leaves both keys valid — no lockout.

2. **Validate** — open a fresh login to the device using *only* the `--ssh-key` private key, with the SSH agent and any config identity disabled. This is a bare `true` command — a successful session is the proof. Retried up to 3 times with a short pause to absorb transient network blips.

3. **Commit** — the new key has proven it works. Delete the sentinel and everything below it, leaving the file with only the new key.

If validation fails (wrong key, typo, unreachable host after the stage), the old keys are restored over the still-live old key. The error reports that nothing was retired and no lockout occurred. A crashed run between stage and commit is self-healing: the next rotation's stage strips any stale sentinel and carries the current keys down.
