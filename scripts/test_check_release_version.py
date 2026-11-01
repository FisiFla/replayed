"""Tests for scripts/check-release-version.py (run: python3 -m unittest discover -s scripts)."""

import contextlib
import importlib.util
import io
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("check_release_version", os.path.join(HERE, "check-release-version.py"))
crv = importlib.util.module_from_spec(spec)
spec.loader.exec_module(crv)


def tree(root, workspace="0.2.1", shell="0.2.1", package="0.2.1", lock="0.2.1", lock_root="0.2.1",
         conf="0.2.1", changelog="## [Unreleased]\n\n## [0.2.1] - 2026-10-24\n- a change\n"):
    """A minimal repository holding the six places a version is written."""

    def put(path, text):
        full = os.path.join(root, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w", encoding="utf-8") as fh:
            fh.write(text)

    put("Cargo.toml", '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "%s"\nedition = "2021"\n' % workspace)
    put("apps/desktop/src-tauri/Cargo.toml", '[package]\nname = "d"\nversion = "%s"\n\n[dependencies]\nserde = { version = "1" }\n' % shell)
    put("apps/desktop/package.json", json.dumps({"name": "d", "version": package}))
    put("apps/desktop/package-lock.json", json.dumps({"name": "d", "version": lock, "packages": {"": {"name": "d", "version": lock_root}}}))
    put("apps/desktop/src-tauri/tauri.conf.json", json.dumps({"version": conf}))
    put("CHANGELOG.md", changelog)


class CheckReleaseVersion(unittest.TestCase):
    def run_check(self, argv, **sources):
        with tempfile.TemporaryDirectory() as root:
            tree(root, **sources)
            out, err = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                status = crv.main(["--root", root] + argv)
            return status, out.getvalue(), err.getvalue()

    def test_agreeing_sources_and_tag_pass(self):
        status, out, _ = self.run_check(["--tag", "v0.2.1"])
        self.assertEqual(status, 0)
        self.assertIn("0.2.1", out)

    def test_without_a_tag_the_sources_must_still_agree(self):
        self.assertEqual(self.run_check([])[0], 0)
        self.assertEqual(self.run_check([], package="0.2.0")[0], 1)

    def test_every_source_is_compared_with_the_tag(self):
        for field in ("workspace", "shell", "package", "lock", "lock_root", "conf"):
            status, _, err = self.run_check(["--tag", "v0.2.1"], **{field: "0.2.0"})
            self.assertEqual(status, 1, field)
            self.assertIn("0.2.0", err, field)

    def test_the_tag_must_name_the_version_built(self):
        status, _, err = self.run_check(["--tag", "v0.2.2"])
        self.assertEqual(status, 1)
        self.assertIn("0.2.2", err)

    def test_a_missing_changelog_section_fails(self):
        status, _, err = self.run_check(["--tag", "v0.2.1"], changelog="## [Unreleased]\n")
        self.assertEqual(status, 1)
        self.assertIn("CHANGELOG", err)

    def test_a_heading_that_only_mentions_the_version_is_not_a_section(self):
        status, _, _ = self.run_check(["--tag", "v0.2.1"], changelog="## [0.2.10] - 2026\n- x\n")
        self.assertEqual(status, 1)

    def test_a_prerelease_tag_needs_matching_sources_and_a_changelog_section(self):
        every = dict(workspace="0.3.0-rc.1", shell="0.3.0-rc.1", package="0.3.0-rc.1", lock="0.3.0-rc.1",
                     lock_root="0.3.0-rc.1", conf="0.3.0-rc.1")
        status, out, _ = self.run_check(["--tag", "v0.3.0-rc.1"], changelog="## [0.3.0-rc.1] - 2026\n- x\n", **every)
        self.assertEqual(status, 0)
        self.assertIn("0.3.0-rc.1", out)
        # The sources say 0.3.0 and the tag says a pre-release of it: not the same version.
        status, _, _ = self.run_check(["--tag", "v0.3.0-rc.1"], changelog="## [0.3.0] - 2026\n", workspace="0.3.0",
                                      shell="0.3.0", package="0.3.0", lock="0.3.0", lock_root="0.3.0", conf="0.3.0")
        self.assertEqual(status, 1)

    def test_a_prerelease_may_use_its_release_section_in_the_changelog(self):
        every = dict(workspace="0.3.0-rc.1", shell="0.3.0-rc.1", package="0.3.0-rc.1", lock="0.3.0-rc.1",
                     lock_root="0.3.0-rc.1", conf="0.3.0-rc.1")
        status, _, _ = self.run_check(["--tag", "v0.3.0-rc.1"], changelog="## [0.3.0] - unreleased\n- x\n", **every)
        self.assertEqual(status, 0)

    def test_tags_that_are_not_a_version_are_refused(self):
        for tag in ("v1", "1.2.3", "v1.2", "v1.2.3.4", "vfoo", "v1.2.3; echo", "v01.2.3", "v1.2.3-", "v1.2.3+meta"):
            self.assertEqual(self.run_check(["--tag", tag])[0], 1, tag)

    def test_all_failures_are_reported_together(self):
        _, _, err = self.run_check(["--tag", "v0.2.1"], package="0.1.0", conf="0.1.1")
        self.assertIn("0.1.0", err)
        self.assertIn("0.1.1", err)

    def test_a_crate_that_inherits_the_workspace_version_is_not_a_second_source(self):
        with tempfile.TemporaryDirectory() as root:
            tree(root)
            path = os.path.join(root, "apps/desktop/src-tauri/Cargo.toml")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write('[package]\nname = "d"\nversion.workspace = true\n')
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(crv.main(["--root", root, "--tag", "v0.2.1"]), 0)

    def test_the_real_repository_agrees_with_itself(self):
        repo = os.path.dirname(HERE)
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(crv.main(["--root", repo]), 0)


if __name__ == "__main__":
    unittest.main()
