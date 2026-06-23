# Deploy backends: LAN vs registry

`bootcher deploy` pushes the freshly built image to running devices over one of two backends, chosen automatically based on `[deploy] registry` in `bootcher.toml`.

## LAN (default)

Active when `[deploy] registry` is **not** set.

The builder machine briefly runs a throwaway, loopback-only container registry and forwards it to the device over an SSH remote-forward (`ssh -R`). The device pulls only the layers it is missing, all through the existing SSH session — encryption and authentication ride the SSH channel, and the device requires no registry configuration.

At least one entry in `[deploy] remotes` is required: `deploy` reaches the device over SSH.

**When to use:** a workshop bench, an air-gapped LAN, or any setup where standing up a registry is more trouble than it is worth. In LAN mode, updates are always push-driven: the bootc origin points at the device's own local container storage, where only `bootcher deploy` ever lands new revisions.

## Registry

Active when `[deploy] registry` is set (e.g. `registry = "registry.gitlab.com/org/project"`).

The builder pushes the multi-arch manifest list to `<registry>/<name>:latest` with `podman push`. The device's bootc origin is pointed at that reference with `bootc switch`; future invocations of `bootc-fetch-apply-updates.timer` find new revisions automatically.

If `[deploy] remotes` lists any devices, `deploy` also SSHes into each one and runs `bootc upgrade` immediately after the push. In a fully registry-based fleet with no `remotes`, the push is the whole operation — devices self-update on their timer schedule.

**When to use:** OTA-capable fleets, CI/CD pipelines, or multi-device rollouts where you want push-from-CI and pull-on-timer. Image signing (via `[deploy] registry` inline table) is only available in registry mode.

## Both backends apply to takeover too

[`bootcher takeover`](../workflows/takeover.md) — which converts a live host to bootc in place — ships the image over whichever of these two backends the manifest selects, exactly as `deploy` does: a loopback-registry SSH tunnel in LAN mode, a direct registry pull (after pushing the list) in registry mode.

## Two credentials in registry mode

Registry mode introduces two distinct credentials:

| Credential | Who holds it | Privilege | How it gets there |
|---|---|---|---|
| **Pull token** | Every deployed device | Read-only | Injected at `provision`; rotatable with `bootcher rotate pull-token` |
| **Push auth** | Builder / CI | Read-write | `podman login <registry>` on the builder, kept off devices |

Use a least-privilege, read-only token for the pull credential (e.g. a GitLab deploy token with `read_registry` scope). The push auth stays on the machine running `bootcher deploy` — or in a CI secret — and bootcher leaves it to `podman login` to manage.

In a pipeline, log in before invoking `bootcher deploy`:

```sh
echo "$PUSH_TOKEN" | podman login registry.example.com \
  --username "$PUSH_USER" --password-stdin
bootcher deploy
```

`podman login` writes to `~/.config/containers/auth.json` (or `$REGISTRY_AUTH_FILE` if set), where `podman push` inside bootcher picks it up automatically. The credentials persist for the lifetime of the runner job and require no further configuration.
