"""Tests for `recipes/ci/scripts/release_check.py`."""

import unittest

from support import load_script, run_script

release_check = load_script("release_check")

STABLE_45 = {
    "docker://quay.io/fedora/fedora-bootc:latest":
        {"json": {"Labels": {"org.opencontainers.image.version": "45.20261105.0"}}},
}


def pin(n):
    return f"ARG FEDORA_VERSION={n}\nFROM quay.io/fedora/fedora-bootc:${{FEDORA_VERSION}}\n"


class CheckTest(unittest.TestCase):
    def verdict(self, containerfile, current=45):
        return release_check.check(containerfile, lambda: current)[0]

    def test_verdicts_by_distance(self):
        self.assertEqual(self.verdict(pin(45)), "current")
        self.assertEqual(self.verdict(pin(46)), "current")  # e.g. a pre-release pin
        self.assertEqual(self.verdict(pin(44)), "behind")
        self.assertEqual(self.verdict(pin(43)), "eol")
        self.assertEqual(self.verdict(pin(40)), "eol")

    def test_messages_name_the_releases(self):
        _, behind = release_check.check(pin(44), lambda: 45)
        self.assertIn("Fedora 45 is out", behind)
        self.assertIn("four weeks after Fedora 46 ships", behind)
        _, eol = release_check.check(pin(43), lambda: 45)
        self.assertIn("Bump FEDORA_VERSION to 45", eol)

    def test_pin_line_variants(self):
        self.assertEqual(self.verdict("  ARG FEDORA_VERSION=44  \n"), "behind")
        for unpinned in ["FROM quay.io/fedora/fedora-bootc:latest\n",
                         "ARG FEDORA_VERSION=latest\n",
                         "# ARG FEDORA_VERSION=44\n",
                         ""]:
            self.assertEqual(self.verdict(unpinned), "unpinned", repr(unpinned))

    def test_no_lookup_without_a_pin(self):
        def boom():
            raise AssertionError("looked up the stable release")
        self.assertEqual(release_check.check("FROM scratch\n", boom)[0], "unpinned")


class MainTest(unittest.TestCase):
    """The script end to end, with a fake `skopeo`."""

    GITHUB = {"WARNING_PREFIX": "::warning::", "BEHIND_EXIT_CODE": "0"}
    GITLAB = {"WARNING_PREFIX": "warning: ", "BEHIND_EXIT_CODE": "3"}

    def run_with(self, containerfile, env=None, skopeo=STABLE_45):
        return run_script("release_check", files={"Containerfile": containerfile},
                          skopeo=skopeo, env=env)

    def test_current(self):
        code, out, _, _ = self.run_with(pin(45), self.GITLAB)
        self.assertEqual((code, out), (0, "Fedora 45 is the current release\n"))

    def test_behind_uses_the_platform_prefix_and_exit_code(self):
        code, out, _, _ = self.run_with(pin(44), self.GITHUB)
        self.assertEqual(code, 0)
        self.assertTrue(out.startswith("::warning::Fedora 45 is out."), out)
        code, out, _, _ = self.run_with(pin(44), self.GITLAB)
        self.assertEqual(code, 3)
        self.assertTrue(out.startswith("warning: Fedora 45 is out."), out)

    def test_empty_env_falls_back_to_the_defaults(self):
        code, out, _, _ = self.run_with(pin(44), {"WARNING_PREFIX": "", "BEHIND_EXIT_CODE": ""})
        self.assertEqual(code, 0)
        self.assertTrue(out.startswith("warning: Fedora 45 is out."), out)

    def test_end_of_life_fails(self):
        code, out, err, _ = self.run_with(pin(43), self.GITHUB)
        self.assertEqual((code, out), (1, ""))
        self.assertIn("Fedora 43 is at or past end of life", err)

    def test_unreadable_stable_tag_fails(self):
        code, _, err, _ = self.run_with(pin(45), self.GITHUB, skopeo={})
        self.assertEqual(code, 1)
        self.assertIn("can't read docker://quay.io/fedora/fedora-bootc:latest", err)

    def test_unpinned_passes_without_a_lookup(self):
        code, out, _, _ = self.run_with("FROM scratch\n", self.GITLAB, skopeo={})
        self.assertEqual(code, 0)
        self.assertIn("nothing to check", out)


if __name__ == "__main__":
    unittest.main()
