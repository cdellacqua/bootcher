"""Did the base image move since the last publish?

Part of the bootcher CI recipes; copy this directory into your repo as `ci/` and adapt
it as needed. Stdlib only; needs `skopeo` (already logged in to the registry).

For every per-arch member of the published `<IMAGE>:<CHANNEL>`, it compares the base
digest buildah recorded (the `org.opencontainers.image.base.name`/`.digest` annotations)
with the base tag's current digest for that arch. Anything it can't verify about the
base (first run, an image built before the check existed, a base it can't inspect)
counts as "moved", so a broken check never costs a security update. If it can't read
the published image at all (an expired token, a registry outage), it fails instead of
quietly skipping. It also reports the commit the channel ships (its
`org.opencontainers.image.revision` label), so a refresh can rebuild that commit on the
new base.

Reads:
  IMAGE          the published image, `<registry>/<name>` (required)
  CHANNEL        the channel tag to check; empty or unset means `latest`
  GITHUB_OUTPUT  where to write the results; unset (e.g. on GitLab) means `base.env`,
                 a dotenv file

Writes (appended to GITHUB_OUTPUT or base.env):
  BASE_CHANGED   `true` to rebuild, `false` to skip
  SOURCE_REV     the clean commit the channel ships, if it records one

Exits 0 with BASE_CHANGED set, or non-zero (no BASE_CHANGED) when the published image
can't be read, or when a non-`latest` channel records no clean source revision.

The decision lives in `check()`, which takes the registry lookups as a function;
bootcher's repo tests it in
https://github.com/cdellacqua/bootcher/tree/main/recipes/ci/tests
"""

import json, os, subprocess, sys


class Failed(Exception):
    """The check can't decide: the job should fail rather than guess."""


def skopeo_inspect(*args):
    run = subprocess.run(["skopeo", "inspect", *args], capture_output=True, text=True)
    if run.returncode:
        raise LookupError(run.stderr.strip())
    return json.loads(run.stdout)


def check(image, channel, inspect, write, log=print):
    """Decide whether `<image>:<channel>`'s base moved; returns BASE_CHANGED.

    `inspect(*args)` runs `skopeo inspect` (raising LookupError with its stderr on
    failure), `write(key, value)` records an output (SOURCE_REV) and `log` prints
    progress. Raises `Failed` when the job should fail instead.
    """
    try:
        top = inspect("--raw", f"docker://{image}:{channel}")
    except LookupError as e:
        if "manifest unknown" not in str(e):
            raise Failed(f"can't read it: {e}") from None
        log(f"nothing published to {image}:{channel} yet")
        return channel == "latest"

    # A list's per-arch members (attestations have no real platform), or a lone image.
    if "manifests" in top:
        refs = [f"{image}@{m['digest']}" for m in top["manifests"]
                if m.get("platform", {}).get("architecture", "unknown") != "unknown"]
    else:
        refs = [f"{image}:{channel}"]

    # The commit the channel ships: a refresh rebuilds it on the new base.
    try:
        labels = inspect("--config", f"docker://{refs[0]}")["config"].get("Labels") or {}
    except (LookupError, IndexError, KeyError, ValueError):
        labels = {}
    rev = labels.get("org.opencontainers.image.revision", "")
    if rev and not rev.endswith("-dirty"):
        log(f"{image}:{channel} ships {rev}")
        write("SOURCE_REV", rev)
    elif channel != "latest":
        raise Failed(f"no clean source revision recorded ({rev!r}); publish it once with a normal deploy")
    if not refs:
        log(f"{image}:{channel} lists no image to check, rebuilding")
        return True

    for ref in refs:
        try:
            arch = inspect("--config", f"docker://{ref}")["architecture"]
            annotations = inspect("--raw", f"docker://{ref}").get("annotations", {})
            base = annotations["org.opencontainers.image.base.name"]
            built = annotations["org.opencontainers.image.base.digest"]
            current = next((m["digest"] for m in inspect("--raw", f"docker://{base}").get("manifests", [])
                            if m.get("platform", {}).get("architecture") == arch), None) \
                or inspect(f"docker://{base}")["Digest"]
        except (LookupError, KeyError, ValueError) as e:
            log(f"{ref}: can't check the base ({e!r}), rebuilding")
            return True
        if current != built:
            log(f"{arch}: {base} moved ({built} -> {current}), rebuilding")
            return True
        log(f"{arch}: {base} unchanged")
    log("base image unchanged since the last publish, skipping the rebuild")
    return False


def main():
    image, channel = os.environ["IMAGE"], os.environ.get("CHANNEL") or "latest"
    out = open(os.environ.get("GITHUB_OUTPUT") or "base.env", "a")
    write = lambda key, value: out.write(f"{key}={value}\n")
    try:
        changed = check(image, channel, skopeo_inspect, write)
    except Failed as e:
        sys.exit(f"{image}:{channel}: {e}")
    write("BASE_CHANGED", str(changed).lower())


if __name__ == "__main__":
    main()
