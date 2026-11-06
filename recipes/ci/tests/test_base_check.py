"""Tests for `recipes/ci/scripts/base_check.py`."""

import unittest

from support import load_script, run_script

base_check = load_script("base_check")

IMAGE = "registry.example/org/img"
BASE = "quay.io/fedora/fedora-bootc:45"
REV = "org.opencontainers.image.revision"


class Registry:
    """Fake registry contents, answering `skopeo inspect` the way `check()` calls it."""

    def __init__(self):
        self.table = {}

    def set(self, args, reply):
        self.table[args] = reply
        return self

    def publish(self, channel="latest", members=(("amd64", "sha256:bamd"), ("arm64", "sha256:barm")),
                revision="abc123", attestation=True):
        """A published list whose members recorded `BASE` at the given digests."""
        manifests = [{"digest": f"sha256:{arch}", "platform": {"architecture": arch}}
                     for arch, _ in members]
        if attestation:
            manifests.append({"digest": "sha256:att", "platform": {"architecture": "unknown"}})
        self.set(f"--raw docker://{IMAGE}:{channel}", {"json": {"manifests": manifests}})
        for arch, base_digest in members:
            ref = f"{IMAGE}@sha256:{arch}"
            labels = {REV: revision} if revision else {}
            self.set(f"--config docker://{ref}",
                     {"json": {"architecture": arch, "config": {"Labels": labels}}})
            self.set(f"--raw docker://{ref}", {"json": {"annotations": {
                "org.opencontainers.image.base.name": BASE,
                "org.opencontainers.image.base.digest": base_digest,
            }}})
        return self

    def base_now(self, amd64="sha256:bamd", arm64="sha256:barm"):
        """The base tag's current per-arch digests."""
        return self.set(f"--raw docker://{BASE}", {"json": {"manifests": [
            {"digest": amd64, "platform": {"architecture": "amd64"}},
            {"digest": arm64, "platform": {"architecture": "arm64"}},
        ]}})

    def inspect(self, *args):
        reply = self.table.get(" ".join(args), {"error": "manifest unknown"})
        if "error" in reply:
            raise LookupError(reply["error"])
        return reply["json"]

    def check(self, channel="latest"):
        """Run `check()`: returns (BASE_CHANGED, written outputs, log lines)."""
        written, logged = {}, []
        changed = base_check.check(IMAGE, channel, self.inspect, written.__setitem__,
                                   logged.append)
        return changed, written, logged


class CheckTest(unittest.TestCase):
    def test_unchanged_base_skips_and_reports_the_revision(self):
        changed, written, _ = Registry().publish().base_now().check()
        self.assertIs(changed, False)
        self.assertEqual(written, {"SOURCE_REV": "abc123"})

    def test_a_moved_arch_rebuilds(self):
        changed, _, logged = Registry().publish().base_now(arm64="sha256:new").check()
        self.assertIs(changed, True)
        self.assertIn("arm64", logged[-1])
        self.assertIn("moved", logged[-1])

    def test_attestation_entries_are_not_checked(self):
        # The "unknown" platform entry has no config/annotations in the fake: checking
        # it would count as "can't check" and rebuild.
        changed, _, _ = Registry().publish(attestation=True).base_now().check()
        self.assertIs(changed, False)

    def test_nothing_published_yet(self):
        self.assertIs(Registry().check("latest")[0], True)
        self.assertIs(Registry().check("stable")[0], False)

    def test_unreadable_image_fails(self):
        registry = Registry().set(f"--raw docker://{IMAGE}:latest", {"error": "unauthorized"})
        with self.assertRaisesRegex(base_check.Failed, "can't read it: unauthorized"):
            registry.check()

    def test_missing_base_annotations_rebuild(self):
        registry = Registry().publish().base_now()
        registry.set(f"--raw docker://{IMAGE}@sha256:amd64", {"json": {}})
        changed, _, logged = registry.check()
        self.assertIs(changed, True)
        self.assertIn("can't check the base", logged[-1])

    def test_unreadable_base_rebuilds(self):
        changed, _, logged = Registry().publish().check()  # base tag not in the registry
        self.assertIs(changed, True)
        self.assertIn("can't check the base", logged[-1])

    def test_dirty_revision(self):
        changed, written, _ = Registry().publish(revision="abc-dirty").base_now().check()
        self.assertIs(changed, False)
        self.assertEqual(written, {})
        with self.assertRaisesRegex(base_check.Failed, "no clean source revision"):
            Registry().publish("stable", revision="abc-dirty").base_now().check("stable")

    def test_channel_needs_a_revision(self):
        with self.assertRaisesRegex(base_check.Failed, "no clean source revision"):
            Registry().publish("stable", revision=None).base_now().check("stable")
        changed, written, _ = Registry().publish("stable").base_now().check("stable")
        self.assertEqual((changed, written), (False, {"SOURCE_REV": "abc123"}))

    def test_lone_image_and_single_arch_base(self):
        registry = Registry()
        registry.set(f"--raw docker://{IMAGE}:latest", {"json": {"annotations": {
            "org.opencontainers.image.base.name": BASE,
            "org.opencontainers.image.base.digest": "sha256:single",
        }}})
        registry.set(f"--config docker://{IMAGE}:latest",
                     {"json": {"architecture": "amd64", "config": {"Labels": {REV: "r1"}}}})
        registry.set(f"--raw docker://{BASE}", {"json": {"schemaVersion": 2}})
        registry.set(f"docker://{BASE}", {"json": {"Digest": "sha256:single"}})
        self.assertEqual(registry.check()[:2], (False, {"SOURCE_REV": "r1"}))

    def test_list_without_real_members_rebuilds(self):
        registry = Registry().set(f"--raw docker://{IMAGE}:latest", {"json": {"manifests": [
            {"digest": "sha256:att", "platform": {"architecture": "unknown"}}]}})
        changed, _, logged = registry.check()
        self.assertIs(changed, True)
        self.assertIn("lists no image to check", logged[-1])


class MainTest(unittest.TestCase):
    """The script end to end, with a fake `skopeo`: where the outputs land."""

    def test_writes_base_env_without_github_output(self):
        registry = Registry().publish().base_now()
        code, _, _, outputs = run_script("base_check", skopeo=registry.table,
                                         env={"IMAGE": IMAGE})
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"base.env": "SOURCE_REV=abc123\nBASE_CHANGED=false\n"})

    def test_writes_github_output_when_set(self):
        registry = Registry().publish().base_now(amd64="sha256:new")
        code, _, _, outputs = run_script(
            "base_check", skopeo=registry.table,
            env={"IMAGE": IMAGE, "GITHUB_OUTPUT": "{dir}/github_output"})
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"github": "SOURCE_REV=abc123\nBASE_CHANGED=true\n"})

    def test_failure_exits_non_zero_without_base_changed(self):
        code, _, err, outputs = run_script(
            "base_check", skopeo={}, env={"IMAGE": IMAGE, "CHANNEL": "stable"})
        # Nothing published to a non-latest channel is not a failure...
        self.assertEqual((code, outputs), (0, {"base.env": "BASE_CHANGED=false\n"}))
        registry = Registry().set(f"--raw docker://{IMAGE}:latest", {"error": "unauthorized"})
        code, _, err, outputs = run_script("base_check", skopeo=registry.table,
                                           env={"IMAGE": IMAGE})
        # ...but an unreadable image is, and records no decision.
        self.assertEqual(code, 1)
        self.assertIn(f"{IMAGE}:latest: can't read it: unauthorized", err)
        self.assertEqual(outputs, {"base.env": ""})


if __name__ == "__main__":
    unittest.main()
