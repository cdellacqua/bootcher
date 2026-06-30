# Recipes

Copy-pasteable templates for flows that go beyond a single bootcher command —
CI pipelines, release channels, and board-specific disk tweaks. Each lives under
[`recipes/`](https://github.com/cdellacqua/bootcher/tree/main/recipes) in the repo
with a README and the files to copy; this page maps each to the flow it covers.

| Recipe | Flow it covers | Start here |
|---|---|---|
| **CI/CD pipeline** | Build, deploy, and provision a multi-arch project unattended in CI — native per-arch runners, one job that needs push credentials, devices self-update from the registry. | [`recipes/ci/`](https://github.com/cdellacqua/bootcher/tree/main/recipes/ci) |
| **Release channels** | Promote a proven commit from a rolling `latest` (canary) channel to a `stable` channel a production fleet tracks — the trigger picks the channel. | [`recipes/ci/channels/`](https://github.com/cdellacqua/bootcher/tree/main/recipes/ci/channels) |
| **Raspberry Pi 4 firmware** | Embed UEFI firmware into the ESP of the built aarch64 raw image with a `disk.post` hook — the template for any board that needs files written outside the `Containerfile`'s reach. | [`recipes/raspi4/`](https://github.com/cdellacqua/bootcher/tree/main/recipes/raspi4) |

## How recipes are wired in

- **CI recipes** are workflow files you copy into your repo (`.github/workflows/`
  or `.gitlab-ci.yml`) plus a small committed [`bootcher.ci.toml`](../reference/bootcher-toml.md#extend-top-level-optional)
  override. They drive the same [`deploy`](../workflows/deploy.md) /
  [`provision`](../workflows/setup.md) commands you run locally, just unattended — see
  [Continuous deployment](../workflows/deploy.md#continuous-deployment). The channels
  variant adds the [`channels`](../reference/bootcher-toml.md#channels) model on top.
- **Hook recipes** like Raspberry Pi firmware plug into a
  [lifecycle hook](../reference/bootcher-toml.md#hooks) in your `bootcher.toml` and
  read the [`BOOTCHER_METADATA`](../reference/bootcher-toml.md#bootcher_metadata) JSON
  bootcher hands every hook, so they never hard-code the output layout.
