"""Tests for scripts/make-update-manifest.py (run: python3 -m unittest discover -s scripts)."""

import base64
import contextlib
import importlib.util
import io
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("make_update_manifest", os.path.join(HERE, "make-update-manifest.py"))
mum = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mum)

SIG = base64.b64encode(b"untrusted comment: x\nRUQ...\ntrusted comment: t\nabc\n").decode()
CHANGELOG = """# Changelog

## [Unreleased]

- not yet

## [0.2.1] - 2026-10-24

### Fixed
- the thing

## [0.2.0] - 2026-10-20

- older
"""


class MakeUpdateManifest(unittest.TestCase):
    def build(self, version="0.2.1", tag="v0.2.1", installer="replayed_0.2.1_x64-setup.exe", changelog=CHANGELOG,
              sig=SIG + "\n"):
        with tempfile.TemporaryDirectory() as tmp:
            sig_path = os.path.join(tmp, installer + ".sig")
            with open(sig_path, "w") as fh:
                fh.write(sig)
            log = os.path.join(tmp, "CHANGELOG.md")
            with open(log, "w") as fh:
                fh.write(changelog)
            out = os.path.join(tmp, "latest.json")
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                status = mum.main([
                    "--version", version, "--tag", tag, "--repo", "FisiFla/replayed",
                    "--installer", installer, "--sig", sig_path, "--changelog", log, "--out", out,
                    "--pub-date", "2026-10-30T10:00:00.000Z",
                ])
            if status != 0:
                return status, None
            with open(out, encoding="utf-8") as fh:
                return status, json.load(fh)

    def test_the_manifest_names_the_setup_exe_the_bundler_writes(self):
        # Tauri 2 with createUpdaterArtifacts: true signs the NSIS installer itself, so the
        # updater downloads `*-setup.exe`. The `*.nsis.zip` of the v1 layout does not exist,
        # and a manifest that points at it is a 404 for every installed copy.
        status, manifest = self.build()
        self.assertEqual(status, 0)
        for key in ("windows-x86_64-nsis", "windows-x86_64"):
            self.assertEqual(
                manifest["platforms"][key]["url"],
                "https://github.com/FisiFla/replayed/releases/download/v0.2.1/replayed_0.2.1_x64-setup.exe",
            )
            self.assertNotIn(".zip", manifest["platforms"][key]["url"])

    def test_the_signature_is_the_sig_file_without_a_newline(self):
        _, manifest = self.build()
        self.assertEqual(manifest["platforms"]["windows-x86_64-nsis"]["signature"], SIG)
        self.assertEqual(manifest["platforms"]["windows-x86_64"]["signature"], SIG)

    def test_version_notes_and_date(self):
        _, manifest = self.build()
        self.assertEqual(manifest["version"], "0.2.1")
        self.assertEqual(manifest["pub_date"], "2026-10-30T10:00:00.000Z")
        self.assertEqual(manifest["notes"], "### Fixed\n- the thing")

    def test_a_prerelease_with_no_section_of_its_own_uses_its_releases(self):
        _, manifest = self.build(
            version="0.3.0-rc.1", tag="v0.3.0-rc.1", installer="replayed_0.3.0-rc.1_x64-setup.exe",
            changelog="## [0.3.0] - unreleased\n- coming\n",
        )
        self.assertEqual(manifest["notes"], "- coming")
        self.assertIn("v0.3.0-rc.1/replayed_0.3.0-rc.1_x64-setup.exe", manifest["platforms"]["windows-x86_64-nsis"]["url"])

    def test_the_installer_must_carry_the_version(self):
        self.assertEqual(self.build(installer="replayed_0.2.0_x64-setup.exe")[0], 1)

    def test_the_artifact_must_be_the_setup_exe(self):
        self.assertEqual(self.build(installer="replayed_0.2.1_x64-setup.nsis.zip")[0], 1)
        self.assertEqual(self.build(installer="replayed_0.2.1_x64_en-US.msi")[0], 1)

    def test_an_empty_signature_is_refused(self):
        self.assertEqual(self.build(sig="\n")[0], 1)

    def test_a_signature_that_is_not_one_line_of_base64_is_refused(self):
        self.assertEqual(self.build(sig="not base64!\n")[0], 1)
        self.assertEqual(self.build(sig=SIG + "\n" + SIG + "\n")[0], 1)

    def test_a_tag_that_does_not_match_the_version_is_refused(self):
        self.assertEqual(self.build(tag="v0.2.2")[0], 1)

    def test_a_missing_changelog_section_is_a_failure(self):
        # The release step checks this first (check-release-version.py); the manifest
        # does not paper over it with empty notes.
        self.assertEqual(self.build(changelog="## [Unreleased]\n")[0], 1)


if __name__ == "__main__":
    unittest.main()
