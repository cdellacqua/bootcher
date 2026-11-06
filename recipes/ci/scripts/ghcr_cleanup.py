"""Prune old published image versions from a GHCR package.

Part of the bootcher CI recipes (GitHub only); copy this directory into your repo as
`ci/` and adapt it as needed. Stdlib only (urllib/json); runs `bootcher --manifest
bootcher.ci.toml info` from the repo root for the package name and channels.

Keeps every version tagged with a channel (`latest` + any `[deploy] channels`) or a
`ci-<arch>` scratch tag, plus the newest KEEP_PREVIOUS other tagged versions (the
immutable CalVer releases), and deletes the rest.

Multi-arch safe: GHCR stores each arch's child manifest as its OWN version, usually
untagged, so a naive count/age prune would delete the arch members of a release it
means to keep and corrupt it (a device then pulls a `:latest` whose amd64/arm64 member
is gone). So the prune is by DIGEST: it builds the set of digests to keep, extends it
with the child manifests those kept lists reference (resolved from the registry), and
deletes only versions outside that set. cosign signatures and attachments
(`sha256-<digest>.sig|.att|.sbom`, also their own versions) aren't releases: each is
kept exactly when the image or arch member it belongs to is kept. Channels need no
special handling: every declared channel is a protected tag. Any unhandled error aborts
before deleting (fail closed), and it refuses to delete every version.

The keep/delete decision lives in `plan()`, which does no I/O; bootcher's repo tests
it in `recipes/ci/tests/`.

Reads:
  GH_TOKEN           token with `packages: write` (GITHUB_TOKEN works for a package
                     linked to the repo; an org-owned package may need a PAT with
                     `delete:packages`) (required)
  GITHUB_REPOSITORY  `<owner>/<repo>`, set by GitHub Actions; the owner owns the package
                     (required)
  KEEP_PREVIOUS      how many CalVer releases to keep besides the channels (default 5)
  DRY_RUN            "true" logs what would be deleted without deleting (default "false")
"""

import base64, json, os, re, subprocess, sys, urllib.error, urllib.request

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


def plan(versions, keep, channels, children_of):
    """Decide which package versions to delete; the pure core of the cleanup.

    `versions` are GitHub package versions (`name` is the digest), `keep` is
    KEEP_PREVIOUS, `channels` the protected channel tags, and `children_of(digest)`
    returns the child manifest digests of a kept list (only called for kept ones).
    Returns `(keep_all, doomed)`: the digests kept and the versions to delete.
    Raises `Refused` if nothing would be kept.
    """
    # Protected = a channel pointer (latest + [deploy] channels) or a ci-<arch> scratch
    # tag; those are mutable and never pruned by age.
    protected = lambda v: any(t in channels or t.startswith("ci-") for t in tags(v))
    images = [v for v in versions if not subject(v)]
    releases = sorted((v for v in images if tags(v) and not protected(v)),
                      key=lambda v: v["created_at"], reverse=True)
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
    dry_run = os.environ.get("DRY_RUN", "false") == "true"
    token = os.environ["GH_TOKEN"]
    owner = os.environ["GITHUB_REPOSITORY"].split("/")[0].lower()

    info = project_info()
    name, channels = info["name"], set(info["channels"])
    print(f"cleanup: {owner}/{name} — keeping :latest + {keep} previous release(s) (dry-run={dry_run})")

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

    def children_of(digest):
        manifest = fetch(f"https://ghcr.io/v2/{owner}/{name}/manifests/{digest}",
                         {"Authorization": f"Bearer {reg}", "Accept": accept})
        return [m["digest"] for m in manifest.get("manifests", [])]

    try:
        _, doomed = plan(versions, keep, channels, children_of)
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
