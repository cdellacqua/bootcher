"""Which published build is a commit's? Prints its version tag, for `bootcher promote`.

Part of the bootcher CI recipes; copy this directory into your repo as `ci/` and adapt
it as needed. Stdlib only (urllib/json); runs `bootcher --manifest bootcher.ci.toml
info` from the repo root for the image name and ref.

Every deploy tags its build with an immutable version, `YYYYMMDD.HHMMSS.g<short sha>`
(older bootchers: `YYYYMMDD.HHMM.g<short sha>`). This finds the version tags whose
commit is COMMIT and prints the one pushed most recently: the build the default
branch's pipeline deployed to `latest` for that commit, which the canary fleet ran. A
later base-image refresh of the same commit is a newer build of it and wins. Builds
are ranked by when the registry first received them, not by the tag text. Tags from a
dirty tree (`g<sha>-dirty`) never match: they aren't that commit.

It never rebuilds: with no build of COMMIT published it exits non-zero, saying to
deploy the commit to `latest` first.

The registry API has no push times, so it asks the platform's package API:
  - GitHub (GITHUB_ACTIONS=true): the package versions, each a digest with its tags
    and `created_at`. Reads GH_TOKEN (`packages: read`) and GITHUB_REPOSITORY.
  - GitLab (GITLAB_CI=true): the project's container repository whose location is the
    image, then each matching tag's details (`created_at`, which GitLab documents as
    when the manifest was created in the registry). Reads CI_API_V4_URL, CI_PROJECT_ID
    and CI_JOB_TOKEN.

Reads:
  COMMIT  the full commit SHA whose build to find (required)

The choice lives in `pick()`, which does no I/O; bootcher's repo tests it in
https://github.com/cdellacqua/bootcher/tree/main/recipes/ci/tests
"""

import datetime, json, os, re, subprocess, sys, urllib.parse, urllib.request

# A version tag and the commit prefix it carries.
VERSION = re.compile(r"^\d{8}\.\d{4}(?:\d{2})?\.g([0-9a-f]{7,40})$")


class NotFound(Exception):
    """No build of the commit is published."""


def commit_of(tag):
    """The short SHA a version tag carries, or None if `tag` isn't one."""
    m = VERSION.match(tag)
    return m[1] if m else None


def pick(commit, candidates):
    """The version tag of `commit`'s most recently pushed build.

    `candidates` are `(tag, pushed_at)` pairs, `pushed_at` an ISO 8601 timestamp; any
    tag may be among them (channels, signatures, other commits). Ties (one build
    carrying two version tags) go to the later tag. Raises `NotFound`.
    """
    commit = commit.lower()
    matching = [(parse_time(at), tag) for tag, at in candidates
                if (sha := commit_of(tag)) and commit.startswith(sha)]
    if not matching:
        raise NotFound(f"no build of commit {commit[:12]} is published: deploy it to "
                       "`latest` first (push it to the default branch), then tag it")
    return max(matching)[1]


def parse_time(at):
    return datetime.datetime.fromisoformat(at.replace("Z", "+00:00"))


def fetch(url, headers):
    with urllib.request.urlopen(urllib.request.Request(url, headers=headers)) as r:
        return json.loads(r.read())


def pages(url, headers):
    """Every item of a paginated GitHub/GitLab list endpoint (`per_page`/`page`)."""
    page = 1
    sep = "&" if "?" in url else "?"
    while batch := fetch(f"{url}{sep}per_page=100&page={page}", headers):
        yield from batch
        page += 1


def github_candidates(name):
    owner = os.environ["GITHUB_REPOSITORY"].split("/")[0].lower()
    api = "https://api.github.com"
    headers = {
        "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    kind = "orgs" if fetch(f"{api}/users/{owner}", headers)["type"] == "Organization" else "users"
    for v in pages(f"{api}/{kind}/{owner}/packages/container/{name}/versions", headers):
        for tag in v["metadata"]["container"]["tags"]:
            yield tag, v["created_at"]


def gitlab_candidates(commit, image):
    api = os.environ["CI_API_V4_URL"]
    project = os.environ["CI_PROJECT_ID"]
    headers = {"JOB-TOKEN": os.environ["CI_JOB_TOKEN"]}
    repos = f"{api}/projects/{project}/registry/repositories"
    repo = next((r for r in pages(repos, headers) if r["location"] == image), None)
    if repo is None:
        raise NotFound(f"no container repository {image} in this project: nothing deployed yet")
    tags = f"{repos}/{repo['id']}/tags"
    # The list has names only; read the details (with created_at) of the matching ones.
    for t in pages(tags, headers):
        sha = commit_of(t["name"])
        if sha and commit.lower().startswith(sha):
            detail = fetch(f"{tags}/{urllib.parse.quote(t['name'], safe='')}", headers)
            yield t["name"], detail["created_at"]


def project_info():
    argv = ["bootcher", "--manifest", "bootcher.ci.toml", "info", "--format", "json"]
    return json.loads(subprocess.run(argv, capture_output=True, text=True, check=True).stdout)


def main():
    commit = os.environ["COMMIT"]
    info = project_info()
    if not info.get("image"):
        sys.exit("no registry in bootcher.toml [deploy]: promotion is registry-mode only")
    try:
        if os.environ.get("GITHUB_ACTIONS") == "true":
            candidates = github_candidates(info["name"])
        elif os.environ.get("GITLAB_CI") == "true":
            candidates = gitlab_candidates(commit, info["image"])
        else:
            sys.exit("find_version: run it in GitHub Actions or GitLab CI")
        version = pick(commit, candidates)
    except NotFound as e:
        sys.exit(f"find_version: {e}")
    print(f"find_version: {info['image']}:{version} is the newest build of {commit[:12]}",
          file=sys.stderr)
    print(version)


if __name__ == "__main__":
    main()
