"""Tests for the keep/delete decision of `recipes/ci/scripts/ghcr_cleanup.py`.

Stdlib `unittest` only; run with `python3 -m unittest discover recipes/ci/tests`
(`cargo xtask test` does). Not part of what users copy into their repo.
"""

import importlib.util
import pathlib
import unittest

_SCRIPT = pathlib.Path(__file__).resolve().parent.parent / "scripts" / "ghcr_cleanup.py"
_spec = importlib.util.spec_from_file_location("ghcr_cleanup", _SCRIPT)
cleanup = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(cleanup)


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
            "created_at": f"2026-01-01T00:00:{self._clock:02d}Z",
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


if __name__ == "__main__":
    unittest.main()
