"""Is the Fedora release the Containerfile pins still supported?

Part of the bootcher CI recipes; copy this directory into your repo as `ci/` and adapt
it as needed (the two settings below especially). Stdlib only; needs `skopeo`. Run it
from the repo root, where the Containerfile is.

It compares the release pinned in the Containerfile with the one the stable tag
currently points at:

  - same release (or newer): prints that and exits 0;
  - one release behind: prints a warning (prefixed with WARNING_PREFIX) and exits with
    BEHIND_EXIT_CODE, since the pinned release still gets updates for a while;
  - two or more behind (at or within weeks of end of life): exits 1 with an error;
  - no pin found: says so and exits 0.

Reads:
  WARNING_PREFIX    prefix of the "one release behind" message (default "warning: ");
                    GitHub Actions sets "::warning::" to make it a run annotation
  BEHIND_EXIT_CODE  exit code when one release behind (default 0); GitLab sets 3 and
                    lists it in the job's `allow_failure: exit_codes`, so the job shows
                    as a warning

The decision lives in `check()`, which takes the stable-release lookup as a function;
bootcher's repo tests it in `recipes/ci/tests/`.
"""

import json, os, re, subprocess, sys

# Adapt to your setup: where the Containerfile pins the release, and the tag that
# follows the current stable one (point it at your mirror if you use one).
PIN = re.compile(r"^\s*ARG FEDORA_VERSION=(\d+)\s*$", re.M)
STABLE = "docker://quay.io/fedora/fedora-bootc:latest"


def stable_release():
    """The current Fedora release, read from STABLE's version label."""
    run = subprocess.run(["skopeo", "inspect", STABLE], capture_output=True, text=True)
    if run.returncode:
        raise LookupError(run.stderr.strip())
    return int(json.loads(run.stdout)["Labels"]["org.opencontainers.image.version"].split(".")[0])


def check(containerfile, current_release):
    """Judge the Containerfile's pin: returns `(verdict, message)`.

    The verdict is "current", "behind" (one release behind), "eol" (two or more) or
    "unpinned". `current_release()` is only called when there's a pin to compare.
    """
    match = PIN.search(containerfile)
    if not match:
        return "unpinned", "no `ARG FEDORA_VERSION=<N>` in the Containerfile, nothing to check"
    pinned = int(match[1])
    current = current_release()
    if current <= pinned:
        return "current", f"Fedora {pinned} is the current release"
    if current == pinned + 1:
        return "behind", (f"Fedora {current} is out. Fedora {pinned} gets updates until about "
                          f"four weeks after Fedora {pinned + 2} ships: plan the upgrade (bump "
                          "FEDORA_VERSION, rebuild, test).")
    return "eol", (f"Fedora {pinned} is at or past end of life (the current release is Fedora "
                   f"{current}): devices stop getting security updates. Bump FEDORA_VERSION to "
                   f"{current}, rebuild, test.")


def main():
    warning = os.environ.get("WARNING_PREFIX") or "warning: "
    behind = int(os.environ.get("BEHIND_EXIT_CODE") or "0")
    try:
        verdict, message = check(open("Containerfile").read(), stable_release)
    except LookupError as e:
        sys.exit(f"can't read {STABLE}: {e}")
    if verdict == "eol":
        sys.exit(message)
    if verdict == "behind":
        print(f"{warning}{message}")
        sys.exit(behind)
    print(message)


if __name__ == "__main__":
    main()
