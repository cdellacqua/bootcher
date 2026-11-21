"""Prune old published image versions from a GHCR package.

Part of the bootcher CI recipes (GitHub only); copy this directory into your repo as
`ci/` and adapt it as needed. Stdlib only (urllib/json); runs `bootcher --manifest
bootcher.ci.toml info` from the repo root for the package name and channels.

Keeps every version tagged with a channel (`[registry] channels`), every release a
channel serves (the `bootcher.release` annotation on the entries of each channel's index,
even when the channel lags its release), the in-flight `ci-<sha12>-<arch>` hand-off tags (younger than
CI_MAX_AGE_HOURS), plus the newest KEEP_PREVIOUS other releases, and deletes the rest —
including older `ci-*` scratch tags and the untagged digests a refresh orphaned when it
re-pushed a release.

Multi-arch safe: GHCR stores each arch's child manifest as its OWN version, usually
untagged, so a naive count/age prune would delete the arch members of a release it
means to keep and corrupt it (a device then pulls a `:latest` whose amd64/arm64 member
is gone). So the prune is by DIGEST: it builds the set of digests to keep, extends it
with the child manifests those kept lists reference (resolved from the registry), and
deletes only versions outside that set. cosign signatures and attachments
(`sha256-<digest>.sig|.att|.sbom`, also their own versions) aren't releases: each is
kept exactly when the image or arch member it belongs to is kept. Channels need no
special handling: every declared channel is a protected tag. It refuses to delete every
version. An error while gathering and deciding (reading the package and its versions,
resolving the kept lists) aborts before anything is deleted; an error during the
deletions stops the run with the earlier deletions done. A re-run picks up where it
left off: it lists the package again and recomputes the plan from what is left.

The keep/delete decision lives in `plan()`, which does no I/O; bootcher's repo tests
it in https://github.com/cdellacqua/bootcher/tree/main/recipes/ci/tests

Reads:
  GH_TOKEN           token with `packages: write` (GITHUB_TOKEN works for a package
                     linked to the repo; an org-owned package may need a PAT with
                     `delete:packages`) (required)
  GITHUB_REPOSITORY  `<owner>/<repo>`, set by GitHub Actions; the owner owns the package
                     (required)
  KEEP_PREVIOUS      how many releases to keep besides the served ones (default 5)
  CI_MAX_AGE_HOURS   how long a `ci-*` hand-off tag counts as in flight (default 24)
  DRY_RUN            "true" logs what would be deleted without deleting (default "false")
"""

import base64, json, os, re, subprocess, sys, urllib.error, urllib.request
from datetime import datetime, timedelta, timezone

# cosign signatures/attestations are their own versions, tagged
# `sha256-<subject hex>.sig|.att|.sbom`. They aren't releases (counting them would
# spend KEEP_PREVIOUS slots on signatures): each one follows its subject's fate.
ATTACHMENT = re.compile(r"^sha256-([0-9a-f]{64})\.(sig|att|sbom)$")


class Refused(Exception):
    """The plan would delete every version: a bug upstream shouldn't wipe the package."""


def tags(v):
    return v["metadata"]["container"]["tags"]


def subject(v):
    """The digest a signature/attestation version belongs to, or None for an image."""
    return next((f"sha256:{m[1]}" for t in tags(v) if (m := ATTACHMENT.match(t))), None)


def created(v):
    return datetime.fromisoformat(v["created_at"].replace("Z", "+00:00"))


def plan(versions, keep, channels, served, children_of, now, ci_max_age=timedelta(hours=24)):
    """Decide which package versions to delete; the pure core of the cleanup.

    `versions` are GitHub package versions (`name` is the digest), `keep` is
    KEEP_PREVIOUS, `channels` the declared channel tags, `served` the release names
    those channels serve, `children_of(digest)` the child manifest digests of a kept
    list (only called for kept ones), and `now` the time the `ci-*` age is measured at.
    Returns `(keep_all, doomed)`: the digests kept and the versions to delete.
    Raises `Refused` if nothing would be kept.
    """
    def scratch(v):
        return any(t.startswith("ci-") for t in tags(v))

    def in_flight(v):
        return scratch(v) and now - created(v) < ci_max_age

    # Protected: a channel pointer, a release a channel serves, or an in-flight hand-off.
    def protected(v):
        return in_flight(v) or any(t in channels or t in served for t in tags(v))

    images = [v for v in versions if not subject(v)]
    # Releases: the other tagged images, newest first (scratch tags aren't releases).
    releases = sorted((v for v in images if tags(v) and not protected(v) and not scratch(v)),
                      key=created, reverse=True)
    keep_lists = {v["name"] for v in images if protected(v)} | {v["name"] for v in releases[:keep]}

    # Extend the keep-set with the arch children the kept lists reference, so a
    # multi-arch image we keep never loses a member.
    children = set()
    for digest in keep_lists:
        children |= set(children_of(digest))

    keep_all = keep_lists | children
    # Keep the signatures of every kept image, arch members included.
    keep_all |= {v["name"] for v in versions if subject(v) in keep_all}
    doomed = [v for v in versions if v["name"] not in keep_all]
    # Either condition alone suffices today; the second is a deliberate backstop in case
    # the keep-set logic above is ever wrong (keep names that match no version).
    if not keep_all or len(doomed) >= len(versions):
        raise Refused(f"cleanup: refusing to delete all {len(versions)} version(s) "
                      "(keep-set empty) — aborting")
    return keep_all, doomed


def fetch(url, headers, method="GET"):
    with urllib.request.urlopen(urllib.request.Request(url, headers=headers, method=method)) as r:
        body = r.read()
    return json.loads(body) if body else None


def project_info():
    argv = ["bootcher", "--manifest", "bootcher.ci.toml", "info", "--format", "json"]
    return json.loads(subprocess.run(argv, capture_output=True, text=True, check=True).stdout)


def main():
    keep = int(os.environ.get("KEEP_PREVIOUS", "5"))
    ci_max_age = timedelta(hours=int(os.environ.get("CI_MAX_AGE_HOURS", "24")))
    dry_run = os.environ.get("DRY_RUN", "false") == "true"
    token = os.environ["GH_TOKEN"]
    owner = os.environ["GITHUB_REPOSITORY"].split("/")[0].lower()

    info = project_info()
    name, channels = info["name"], set(info["channels"])
    print(f"cleanup: {owner}/{name} — keeping the channels, their releases and {keep} other "
          f"release(s) (dry-run={dry_run})")

    api = "https://api.github.com"
    gh_headers = {
        "Authorization": f"Bearer {token}",
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    gh = lambda url, method="GET": fetch(url, gh_headers, method)

    # The package-versions endpoint differs for a user vs an org owner — detect once.
    kind = "orgs" if gh(f"{api}/users/{owner}")["type"] == "Organization" else "users"
    base = f"{api}/{kind}/{owner}/packages/container/{name}"

    versions, page = [], 1
    while (batch := gh(f"{base}/versions?per_page=100&page={page}")):
        versions += batch
        page += 1
    print(f"cleanup: {len(versions)} version(s) currently published")
    if not versions:
        print("cleanup: nothing to do")
        sys.exit(0)

    # Kept lists' children are read from the registry, with a pull token.
    basic = base64.b64encode(f"{owner}:{token}".encode()).decode()
    reg = fetch(f"https://ghcr.io/token?service=ghcr.io&scope=repository:{owner}/{name}:pull",
                {"Authorization": f"Basic {basic}"})["token"]
    accept = ",".join([
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.docker.distribution.manifest.v2+json",
    ])

    def manifest(ref):
        return fetch(f"https://ghcr.io/v2/{owner}/{name}/manifests/{ref}",
                     {"Authorization": f"Bearer {reg}", "Accept": accept})

    def children_of(digest):
        return [m["digest"] for m in manifest(digest).get("manifests", [])]

    # The releases the channels serve (a channel that lags its release still names it).
    tagged = {t for v in versions for t in tags(v)}
    served = set()
    for channel in channels & tagged:
        # Every entry of a release's index names it (podman 4.9 can't annotate the index).
        names = {e.get("annotations", {}).get("bootcher.release")
                 for e in manifest(channel).get("manifests", [])}
        if len(names) == 1 and None not in names:
            served.add(names.pop())
    print(f"cleanup: channels serve {', '.join(sorted(served)) or 'nothing yet'}")

    try:
        _, doomed = plan(versions, keep, channels, served, children_of,
                         datetime.now(timezone.utc), ci_max_age)
    except Refused as e:
        sys.exit(str(e))
    print(f"cleanup: keeping {len(versions) - len(doomed)} version(s), deleting {len(doomed)}")

    for v in doomed:
        label = f"{v['name']} tags=[{','.join(tags(v))}]"
        if dry_run:
            print(f"  would delete: {v['id']} {label}")
            continue
        try:
            gh(f"{base}/versions/{v['id']}", method="DELETE")
        except urllib.error.HTTPError as e:
            if e.code == 403:
                sys.exit("cleanup: GITHUB_TOKEN refused (403) — for an org-owned package set a "
                         "CLEANUP_TOKEN PAT with delete:packages and use it as GH_TOKEN.")
            raise
        print(f"  deleted: {v['id']} {label}")
    print("cleanup: done")


if __name__ == "__main__":
    main()
