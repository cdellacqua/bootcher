"""Tests for `recipes/ci/scripts/ghcr_cleanup.py`: the keep/delete decision, and
`main()` against a fake GitHub API and registry (no network).

Stdlib `unittest` only; run with `python3 -B -m unittest discover -s recipes/ci/tests`
(`cargo xtask test` does).
"""

import io
import json
import os
import unittest
import urllib.error
from unittest import mock

from support import load_script

cleanup = load_script("ghcr_cleanup")


def digest(label):
    """A deterministic fake sha256 digest for a readable label."""
    return "sha256:" + label.encode().hex().ljust(64, "0")[:64]


class Package:
    """A fake GHCR package: versions plus the child manifests each list references."""

    def __init__(self):
        self.versions = []
        self.children = {}
        self._clock = 0

    def _add(self, name, tags):
        self._clock += 1
        self.versions.append({
            "id": len(self.versions) + 1,
            "name": name,
            "created_at": f"2026-01-01T00:{self._clock // 60:02d}:{self._clock % 60:02d}Z",
            "metadata": {"container": {"tags": list(tags)}},
        })
        return name

    def signature(self, of):
        return self._add(digest(f"sig-{of}"), [f"sha256-{of.split(':')[1]}.sig"])

    def release(self, label, tags=(), arches=("amd64", "arm64"), signed=False):
        """A multi-arch list tagged `tags`, with untagged per-arch members."""
        members = [self._add(digest(f"{label}-{a}"), []) for a in arches]
        lst = self._add(digest(label), tags)
        self.children[lst] = members
        if signed:
            self.signature(lst)
            for m in members:
                self.signature(m)
        return lst

    def plan(self, keep, channels=("latest",)):
        return cleanup.plan(self.versions, keep, set(channels),
                            lambda d: self.children.get(d, []))

    def deleted(self, keep, channels=("latest",)):
        _, doomed = self.plan(keep, channels)
        return {v["name"] for v in doomed}

    def family(self, lst):
        """A list, its members, and every signature of those."""
        names = {lst, *self.children.get(lst, [])}
        return names | {v["name"] for v in self.versions if cleanup.subject(v) in names}


class PlanTest(unittest.TestCase):
    def test_signatures_do_not_use_up_release_slots(self):
        # The PR #9 regression: on a signed project, signature versions were counted
        # as releases, filled the KEEP_PREVIOUS slots, and real releases got deleted.
        pkg = Package()
        old = [pkg.release(f"r{i}", [f"2026.1.{i}"], signed=True) for i in range(1, 8)]
        latest = pkg.release("r8", ["latest", "2026.1.8"], signed=True)

        deleted = pkg.deleted(keep=5)

        for kept in [latest, *old[2:]]:  # latest + the 5 newest previous releases
            self.assertFalse(pkg.family(kept) & deleted, f"{kept} or its parts deleted")
        for pruned in old[:2]:
            self.assertEqual(pkg.family(pruned) - deleted, set(), f"{pruned} not fully pruned")

    def test_releases_rank_by_age_not_tag_across_calver_formats(self):
        # Tags minted before seconds were added (`YYYYMMDD.HHMM.g<sha>`, no git
        # `YYYYMMDD.HH.MM`) sit beside `YYYYMMDD.HHMMSS[.g<sha>]` ones. Compared as
        # strings, an old no-git tag sorts before every new tag of the same hour, even
        # a later one; the cleanup ranks by push time, so the mix doesn't matter.
        pkg = Package()
        oldest = pkg.release("a", ["20260113.120100"])
        old_git = pkg.release("b", ["20260113.1205.g0a1b2c3d4e5f"])
        old_nogit = pkg.release("c", ["20260113.12.59"])
        newest = pkg.release("d", ["latest", "20260113.130000.g0a1b2c3d4e5f"])

        deleted = pkg.deleted(keep=2)

        for kept in [newest, old_nogit, old_git]:
            self.assertFalse(pkg.family(kept) & deleted, f"{kept} deleted")
        self.assertEqual(deleted, pkg.family(oldest))

    def test_kept_lists_keep_their_untagged_arch_members(self):
        pkg = Package()
        pkg.release("old", ["2026.1.1"])
        latest = pkg.release("new", ["latest", "2026.1.2"])
        deleted = pkg.deleted(keep=0)
        self.assertFalse(pkg.family(latest) & deleted)
        self.assertEqual(deleted, pkg.family(digest("old")))

    def test_member_signatures_follow_their_member(self):
        pkg = Package()
        latest = pkg.release("new", ["latest"], signed=True)
        member_sigs = {v["name"] for v in pkg.versions
                       if cleanup.subject(v) in pkg.children[latest]}
        self.assertEqual(len(member_sigs), 2)
        pkg.release("old", ["2026.1.1"], signed=True)
        self.assertFalse(member_sigs & pkg.deleted(keep=0))

    def test_channel_and_scratch_tags_are_kept_and_take_no_slot(self):
        pkg = Package()
        stable = pkg.release("stable", ["stable"])
        scratch = pkg.release("scratch", ["ci-x86_64"])
        releases = [pkg.release(f"r{i}", [f"2026.1.{i}"]) for i in range(1, 4)]
        latest = pkg.release("latest", ["latest"])

        deleted = pkg.deleted(keep=2, channels=("latest", "stable"))

        for kept in [stable, scratch, latest, *releases[1:]]:
            self.assertFalse(pkg.family(kept) & deleted, f"{kept} deleted")
        self.assertEqual(deleted, pkg.family(releases[0]))

    def test_keep_zero_keeps_only_protected(self):
        pkg = Package()
        old = pkg.release("old", ["2026.1.1"])
        latest = pkg.release("new", ["latest"])
        deleted = pkg.deleted(keep=0)
        self.assertEqual(deleted, pkg.family(old))
        self.assertFalse(pkg.family(latest) & deleted)

    def test_orphan_signatures_and_dangling_untagged_versions_are_deleted(self):
        pkg = Package()
        latest = pkg.release("new", ["latest"])
        orphan = pkg.signature(digest("gone"))
        dangling = pkg._add(digest("dangling"), [])
        deleted = pkg.deleted(keep=5)
        self.assertEqual(deleted, {orphan, dangling})
        self.assertFalse(pkg.family(latest) & deleted)

    def test_children_are_only_resolved_for_kept_lists(self):
        pkg = Package()
        pkg.release("old", ["2026.1.1"])
        latest = pkg.release("new", ["latest"])
        asked = []
        cleanup.plan(pkg.versions, 0, {"latest"}, lambda d: asked.append(d) or [])
        self.assertEqual(asked, [latest])

    def test_refuses_to_delete_everything(self):
        pkg = Package()
        pkg.release("old", ["2026.1.1"])
        with self.assertRaises(cleanup.Refused):
            pkg.plan(keep=0)


API = "https://api.github.com"
OCI_INDEX = "application/vnd.oci.image.index.v1+json"
DOCKER_LIST = "application/vnd.docker.distribution.manifest.list.v2+json"


class FakeGitHub:
    """Answers `urlopen` like the GitHub packages API and GHCR would, and logs calls.

    The versions endpoint pages like the real one (`per_page`/`page`), the package is
    only served under the endpoint matching the owner's type, and each list is served
    only if the Accept header admits its media type (lists alternate OCI index /
    Docker manifest list), as GHCR answers an unacceptable type with an error.
    """

    def __init__(self, pkg, owner_type="User", delete_status=None):
        self.pkg, self.owner_type, self.delete_status = pkg, owner_type, delete_status
        kind = "orgs" if owner_type == "Organization" else "users"
        self.base = f"{API}/{kind}/me/packages/container/img"
        self.media = {lst: (OCI_INDEX, DOCKER_LIST)[i % 2]
                      for i, lst in enumerate(pkg.children)}
        self.deleted = []  # version ids, in order
        self.errors = []  # raised HTTPErrors, closed by `MainTest.run_main`

    def error(self, url, code):
        self.errors.append(urllib.error.HTTPError(url, code, "fake", {}, None))
        return self.errors[-1]

    def urlopen(self, request):
        url, method = request.full_url, request.get_method()
        if method == "DELETE":
            prefix = f"{self.base}/versions/"
            if not url.startswith(prefix):
                raise self.error(url, 404)
            if self.delete_status:
                raise self.error(url, self.delete_status)
            self.deleted.append(int(url[len(prefix):]))
            return self.reply(None)
        if url == f"{API}/users/me":
            return self.reply({"type": self.owner_type})
        if url.startswith(f"{self.base}/versions?"):
            query = dict(kv.split("=") for kv in url.split("?", 1)[1].split("&"))
            per_page, page = int(query["per_page"]), int(query["page"])
            return self.reply(self.pkg.versions[(page - 1) * per_page:page * per_page])
        if url == "https://ghcr.io/token?service=ghcr.io&scope=repository:me/img:pull":
            return self.reply({"token": "registry-token"})
        prefix = "https://ghcr.io/v2/me/img/manifests/"
        if url.startswith(prefix) and url[len(prefix):] in self.pkg.children:
            digest = url[len(prefix):]
            accepted = request.get_header("Accept", "").split(",")
            if (request.get_header("Authorization") != "Bearer registry-token"
                    or self.media[digest] not in accepted):
                raise self.error(url, 404)
            return self.reply({"mediaType": self.media[digest], "manifests": [
                {"digest": m} for m in self.pkg.children[digest]]})
        raise self.error(url, 404)

    @staticmethod
    def reply(body):
        return io.BytesIO(b"" if body is None else json.dumps(body).encode())


class MainTest(unittest.TestCase):
    """`main()` end to end against `FakeGitHub`, with the caller's env shut out."""

    def run_main(self, github, **env):
        """Run `main()`; returns `(exit code, stdout)` (code None when it returns)."""
        env = {"GH_TOKEN": "gh-token", "GITHUB_REPOSITORY": "Me/repo", **env}
        info = {"name": "img", "channels": ["latest"]}
        out = io.StringIO()
        with mock.patch.dict(os.environ, env, clear=True), \
                mock.patch.object(cleanup, "project_info", lambda: info), \
                mock.patch("urllib.request.urlopen", github.urlopen), \
                mock.patch("sys.stdout", out):
            try:
                cleanup.main()
                code = None
            except SystemExit as e:
                code = e.code
            finally:
                for error in github.errors:
                    error.close()
        return code, out.getvalue()

    def big_package(self):
        """20 signed releases (120 versions, two API pages), `latest` the newest."""
        pkg = Package()
        old = [pkg.release(f"r{i:02d}", [f"2026.1.{i}"], signed=True) for i in range(19)]
        pkg.release("r19", ["latest", "2026.1.19"], signed=True)
        self.assertGreater(len(pkg.versions), 100)
        return pkg, old

    def ids(self, pkg, lists):
        """The version ids of `lists` with their members and signatures."""
        names = set().union(*(pkg.family(lst) for lst in lists))
        return {v["id"] for v in pkg.versions if v["name"] in names}

    def test_deletes_exactly_the_versions_outside_the_keep_set(self):
        pkg, old = self.big_package()
        github = FakeGitHub(pkg)
        code, out = self.run_main(github)
        self.assertIsNone(code, out)
        # latest + the 5 newest previous releases stay whole, members and signatures.
        self.assertEqual(set(github.deleted), self.ids(pkg, old[:-5]))
        self.assertEqual(len(github.deleted), len(set(github.deleted)))
        self.assertIn(f"cleanup: {len(pkg.versions)} version(s) currently published",
                      out)
        self.assertTrue(out.endswith("cleanup: done\n"), out)

    def test_keep_previous(self):
        pkg, old = self.big_package()
        github = FakeGitHub(pkg)
        self.assertIsNone(self.run_main(github, KEEP_PREVIOUS="2")[0])
        self.assertEqual(set(github.deleted), self.ids(pkg, old[:-2]))

    def test_org_owner_uses_the_orgs_endpoint(self):
        pkg, old = self.big_package()
        github = FakeGitHub(pkg, owner_type="Organization")
        self.assertIsNone(self.run_main(github)[0])
        self.assertEqual(set(github.deleted), self.ids(pkg, old[:-5]))

    def test_dry_run_deletes_nothing(self):
        pkg, old = self.big_package()
        github = FakeGitHub(pkg)
        code, out = self.run_main(github, DRY_RUN="true")
        self.assertIsNone(code, out)
        self.assertEqual(github.deleted, [])
        would = {int(line.split()[2]) for line in out.splitlines()
                 if line.startswith("  would delete: ")}
        self.assertEqual(would, self.ids(pkg, old[:-5]))

    def test_forbidden_delete_exits_with_the_token_hint(self):
        pkg, _ = self.big_package()
        github = FakeGitHub(pkg, delete_status=403)
        code, _ = self.run_main(github)
        self.assertIsInstance(code, str)
        self.assertIn("refused (403)", code)
        self.assertIn("CLEANUP_TOKEN", code)

    def test_other_http_errors_propagate(self):
        pkg, _ = self.big_package()
        github = FakeGitHub(pkg, delete_status=500)
        with self.assertRaises(urllib.error.HTTPError) as caught:
            self.run_main(github)
        self.assertEqual(caught.exception.code, 500)

    def test_empty_package_is_nothing_to_do(self):
        github = FakeGitHub(Package())
        code, out = self.run_main(github)
        self.assertEqual(code, 0)
        self.assertIn("cleanup: nothing to do", out)
        self.assertEqual(github.deleted, [])

    def test_refuses_without_deleting(self):
        pkg = Package()
        pkg.release("old", ["2026.1.1"], signed=True)  # no `latest`, nothing protected
        github = FakeGitHub(pkg)
        code, _ = self.run_main(github, KEEP_PREVIOUS="0")
        self.assertIsInstance(code, str)
        self.assertIn("refusing to delete all", code)
        self.assertEqual(github.deleted, [])


if __name__ == "__main__":
    unittest.main()
