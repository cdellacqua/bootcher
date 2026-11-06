"""Tests for `recipes/ci/scripts/find_version.py`: the commit → version choice, and
`main()` against fake GitHub and GitLab APIs (no network).

Stdlib `unittest` only; run with `python3 -B -m unittest discover -s recipes/ci/tests`
(`cargo xtask test` does).
"""

import io
import json
import os
import unittest
import urllib.parse
from unittest import mock

from support import load_script

find = load_script("find_version")

COMMIT = "0a1b2c3d4e5f60718293a4b5c6d7e8f901234567"
OTHER = "fedcba9876543210fedcba9876543210fedcba98"


class PickTest(unittest.TestCase):
    def test_newest_push_wins_not_the_tag_text(self):
        # The later-pushed build has the *smaller* tag (e.g. a timestamp minted when
        # its pipeline started): push time decides.
        version = find.pick(COMMIT, [
            ("20261106.150000.g0a1b2c3d4e5f", "2026-11-06T15:00:30Z"),
            ("20261106.140000.g0a1b2c3d4e5f", "2026-11-06T16:00:00Z"),
        ])
        self.assertEqual(version, "20261106.140000.g0a1b2c3d4e5f")

    def test_only_this_commits_version_tags_count(self):
        version = find.pick(COMMIT, [
            ("latest", "2026-11-07T00:00:00Z"),
            ("stable", "2026-11-07T00:00:00Z"),
            ("ci-x86_64", "2026-11-07T00:00:00Z"),
            ("20261107.000000.gfedcba987654", "2026-11-07T00:00:00Z"),  # another commit
            ("20261107.000001.g0a1b2c3d4e5f-dirty", "2026-11-07T00:00:01Z"),  # dirty tree
            ("20261107.000002", "2026-11-07T00:00:02Z"),  # no git
            ("sha256-" + "a" * 64 + ".sig", "2026-11-07T00:00:03Z"),
            ("20261101.000000.g0a1b2c3d4e5f", "2026-11-01T00:00:00Z"),
        ])
        self.assertEqual(version, "20261101.000000.g0a1b2c3d4e5f")

    def test_minute_resolution_tags_and_mixed_timestamp_formats(self):
        version = find.pick(COMMIT.upper(), [
            ("20261101.1200.g0a1b2c3d4e5", "2026-11-01T12:00:00.123+00:00"),
            ("20261101.110000.g0a1b2c3d4e5f", "2026-11-01T11:00:00Z"),
        ])
        self.assertEqual(version, "20261101.1200.g0a1b2c3d4e5")

    def test_a_shorter_sha_must_still_be_a_prefix(self):
        with self.assertRaises(find.NotFound):
            find.pick(COMMIT, [("20261101.000000.g1a1b2c3d4e5f", "2026-11-01T00:00:00Z")])

    def test_nothing_published_says_to_deploy_first(self):
        with self.assertRaises(find.NotFound) as caught:
            find.pick(COMMIT, [("latest", "2026-11-01T00:00:00Z")])
        self.assertIn("deploy it to `latest` first", str(caught.exception))
        self.assertIn(COMMIT[:12], str(caught.exception))


class FakeApi:
    """A fake `urlopen` answering from a `{url: body}` table, with `per_page`/`page`
    pagination for list bodies."""

    def __init__(self, table, headers):
        self.table, self.headers = table, headers

    def urlopen(self, request):
        for k, v in self.headers.items():
            assert request.get_header(k.capitalize()) == v, (k, request.headers)
        url = request.full_url
        base, _, query = url.partition("?")
        params = dict(urllib.parse.parse_qsl(query))
        body = self.table.get(base if "page" in params else url)
        if body is None:
            raise AssertionError(f"unexpected request {url}")
        if "page" in params:
            size, page = int(params["per_page"]), int(params["page"])
            body = body[(page - 1) * size:page * size]
        return io.BytesIO(json.dumps(body).encode())


class MainTest(unittest.TestCase):
    info = {"name": "img", "image": "registry.example.com/me/repo/img"}

    def run_main(self, api, **env):
        """Run `main()`; returns `(exit code, stdout)` (code None when it returns)."""
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, {"COMMIT": COMMIT, **env}, clear=True), \
                mock.patch.object(find, "project_info", lambda: self.info), \
                mock.patch("urllib.request.urlopen", api.urlopen), \
                mock.patch("sys.stdout", out), mock.patch("sys.stderr", err):
            try:
                find.main()
                code = None
            except SystemExit as e:
                code = e.code
        return code, out.getvalue()

    def github(self, versions, owner_type="User"):
        api = "https://api.github.com"
        kind = "orgs" if owner_type == "Organization" else "users"
        return FakeApi({
            f"{api}/users/me": {"type": owner_type},
            f"{api}/{kind}/me/packages/container/img/versions": versions,
        }, {"Authorization": "Bearer gh-token"})

    def github_env(self):
        return {"GITHUB_ACTIONS": "true", "GH_TOKEN": "gh-token", "GITHUB_REPOSITORY": "Me/repo"}

    @staticmethod
    def version(tags, at):
        return {"created_at": at, "metadata": {"container": {"tags": tags}}}

    def test_github_picks_the_newest_build_across_pages(self):
        filler = [self.version([f"20260101.0000{i:02d}.g{OTHER[:12]}"], "2026-01-01T00:00:00Z")
                  for i in range(150)]
        versions = [
            self.version(["latest", "20261106.120000.g0a1b2c3d4e5f"], "2026-11-06T12:00:05Z"),
            *filler,
            self.version(["20261105.120000.g0a1b2c3d4e5f"], "2026-11-05T12:00:05Z"),
        ]
        code, out = self.run_main(self.github(versions), **self.github_env())
        self.assertIsNone(code)
        self.assertEqual(out, "20261106.120000.g0a1b2c3d4e5f\n")

    def test_github_org_owner(self):
        versions = [self.version(["20261106.120000.g0a1b2c3d4e5f"], "2026-11-06T12:00:05Z")]
        code, out = self.run_main(self.github(versions, "Organization"), **self.github_env())
        self.assertIsNone(code)
        self.assertEqual(out, "20261106.120000.g0a1b2c3d4e5f\n")

    def test_github_no_build_fails_without_output(self):
        versions = [self.version(["latest", f"20261106.120000.g{OTHER[:12]}"], "2026-11-06T12:00:05Z")]
        code, out = self.run_main(self.github(versions), **self.github_env())
        self.assertIsInstance(code, str)
        self.assertIn("deploy it to `latest` first", code)
        self.assertEqual(out, "")

    def gitlab(self, tags, created):
        api = "https://gitlab.example.com/api/v4"
        repos = f"{api}/projects/42/registry/repositories"
        table = {
            repos: [{"id": 1, "location": "registry.example.com/me/repo/other"},
                    {"id": 7, "location": self.info["image"]}],
            f"{repos}/7/tags": [{"name": t} for t in tags],
        }
        for tag, at in created.items():
            table[f"{repos}/7/tags/{tag}"] = {"name": tag, "created_at": at}
        return FakeApi(table, {"Job-token": "job-token"})

    def gitlab_env(self):
        return {"GITLAB_CI": "true", "CI_API_V4_URL": "https://gitlab.example.com/api/v4",
                "CI_PROJECT_ID": "42", "CI_JOB_TOKEN": "job-token"}

    def test_gitlab_reads_details_only_for_matching_tags(self):
        tags = ["latest", "stable", f"20261107.000000.g{OTHER[:12]}",
                "20261106.090000.g0a1b2c3d4e5f", "20261106.080000.g0a1b2c3d4e5f"]
        # Only the two matching tags have details: any other lookup is an error.
        api = self.gitlab(tags, {
            "20261106.090000.g0a1b2c3d4e5f": "2026-11-06T09:00:10.000+00:00",
            "20261106.080000.g0a1b2c3d4e5f": "2026-11-06T09:30:00.000+00:00",
        })
        code, out = self.run_main(api, **self.gitlab_env())
        self.assertIsNone(code)
        self.assertEqual(out, "20261106.080000.g0a1b2c3d4e5f\n")

    def test_gitlab_without_the_repository_says_nothing_is_deployed(self):
        api = self.gitlab([], {})
        self.info = {**self.info, "image": "registry.example.com/me/repo/missing"}
        code, _ = self.run_main(api, **self.gitlab_env())
        self.assertIn("nothing deployed yet", code)

    def test_lan_mode_and_unknown_platform_fail(self):
        self.info = {"name": "img", "image": None}
        code, _ = self.run_main(FakeApi({}, {}), **self.github_env())
        self.assertIn("registry-mode only", code)
        self.info = MainTest.info
        code, _ = self.run_main(FakeApi({}, {}))
        self.assertIn("GitHub Actions or GitLab CI", code)


if __name__ == "__main__":
    unittest.main()
