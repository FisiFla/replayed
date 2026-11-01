"""Tests for scripts/verify-updater-signature.py (run: python3 -m unittest discover -s scripts).

The fixtures are real: the signature below was made by `tauri signer sign --app-version 0.2.1`
(tauri-cli 2.12.4) with a throwaway key generated for this test, over the bytes
`installer bytes`. Its private half is not in the repository, and is not the release key.
Alongside it are the two signatures from the `minisign-verify` crate's own test suite (one
legacy, one prehashed) and two of RFC 8032's Ed25519 vectors.
"""

import base64
import contextlib
import importlib.util
import io
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location(
    "verify_updater_signature", os.path.join(HERE, "verify-updater-signature.py")
)
v = importlib.util.module_from_spec(spec)
spec.loader.exec_module(v)

# Made by the real Tauri CLI; see the module docstring.
PUBKEY = (
    "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDYzMEZGNjVGOEVCMjE2Q0MKUldUTUZyS09YL1lQWTJB"
    "Z3g0YkVVVW90dTZDMVZkSGhFKzRuUnE2R2xqbU15NTBzcUMvZlpXSDAK"
)
SIGNATURE = (
    "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVUTUZyS09YL1lQWTNxajlrMXpRbG8y"
    "T0FwY2Z0dWJrNDlqS1g1aUxXcXhqQlpiWVRTY3h2QTRiRCtzNko0OXFOTWlzSk5FcVFxN3l2SHNQaHhocXd5WXNPQlZzak4zbXcw"
    "PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkzNDc4MzQwCWZpbGU6cmVwbGF5ZWRfMC4yLjFfeDY0LXNldHVwLmV4ZQl2"
    "ZXJzaW9uOjAuMi4xCkNlWVEwejMyL3lVVGxKQ2tTb0lvRG1PaUhKWkRFSHpVZkZSc3JYTGZ6NmFDUWZJTW56RWpMT1pIb3pwejNl"
    "N0ZxMVdoeSs4TytRMFdsTEVGWDl6TUF3PT0K"
)
DATA = b"installer bytes"
NAME = "replayed_0.2.1_x64-setup.exe"

# From the minisign-verify crate's tests: key E7620F1842B4E81F.
CRATE_PUBKEY = base64.b64encode(
    b"untrusted comment: minisign public key E7620F1842B4E81F\n"
    b"RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n"
).decode()
LEGACY_SIG = base64.b64encode(
    b"untrusted comment: signature from minisign secret key\n"
    b"RWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=\n"
    b"trusted comment: timestamp:1555779966\tfile:test\n"
    b"QtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==\n"
).decode()
PREHASHED_SIG = base64.b64encode(
    b"untrusted comment: signature from minisign secret key\n"
    b"RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\n"
    b"trusted comment: timestamp:1556193335\tfile:test\n"
    b"y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==\n"
).decode()


class Ed25519(unittest.TestCase):
    # RFC 8032 section 7.1, tests 1 and 2.
    T1 = (
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        "",
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    )
    T2 = (
        "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        "72",
        "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    )

    def test_rfc_vectors_verify(self):
        for pk, msg, sig in (self.T1, self.T2):
            self.assertTrue(
                v.ed25519_verify(bytes.fromhex(msg), bytes.fromhex(pk), bytes.fromhex(sig))
            )

    def test_a_changed_message_a_changed_signature_and_a_changed_key_fail(self):
        pk, msg, sig = (bytes.fromhex(x) for x in self.T2)
        self.assertFalse(v.ed25519_verify(b"\x73", pk, sig))
        flipped = bytearray(sig)
        flipped[40] ^= 1
        self.assertFalse(v.ed25519_verify(msg, pk, bytes(flipped)))
        self.assertFalse(v.ed25519_verify(msg, bytes.fromhex(self.T1[0]), sig))

    def test_malformed_input_is_a_failure_not_an_exception(self):
        pk, msg, sig = (bytes.fromhex(x) for x in self.T2)
        self.assertFalse(v.ed25519_verify(msg, pk[:31], sig))
        self.assertFalse(v.ed25519_verify(msg, pk, sig[:63]))
        # S >= the group order is not a valid signature (malleability).
        malleable = sig[:32] + (int.from_bytes(sig[32:], "little") + v.Q).to_bytes(32, "little")
        self.assertFalse(v.ed25519_verify(msg, pk, malleable))


class Release(unittest.TestCase):
    def good(self, **overrides):
        args = dict(
            data=DATA, pubkey_b64=PUBKEY, sig_text=SIGNATURE, file_name=NAME, version="0.2.1"
        )
        args.update(overrides)
        v.verify(**args)

    def fails_with(self, needle, **overrides):
        with self.assertRaises(v.SignatureError) as ctx:
            self.good(**overrides)
        self.assertIn(needle, str(ctx.exception))

    def test_the_tauri_cli_signature_verifies(self):
        self.good()

    def test_a_tampered_artifact_is_rejected(self):
        self.fails_with("does not verify", data=DATA + b"x")

    def test_a_signature_from_another_key_is_rejected(self):
        # The pinned key in tauri.conf.json is a different key from the test signer's: the
        # case the Tauri CLI only warns about.
        with open(os.path.join(HERE, "..", "apps/desktop/src-tauri/tauri.conf.json")) as fh:
            pinned = json.load(fh)["plugins"]["updater"]["pubkey"]
        self.fails_with("pins key", pubkey_b64=pinned)

    def test_a_signature_for_another_file_is_rejected(self):
        self.fails_with("not for", file_name="replayed_0.2.1_x64-setup.nsis.zip")

    def test_a_signature_bound_to_another_version_is_rejected(self):
        self.fails_with("bound to version", version="0.2.2")

    def test_a_legacy_signature_is_rejected(self):
        self.fails_with("prehashed", pubkey_b64=CRATE_PUBKEY, sig_text=LEGACY_SIG, data=b"test", file_name="test")

    def test_a_prehashed_signature_without_a_version_is_rejected(self):
        # The cryptography passes (this is minisign's own vector) and the version binding is
        # what refuses it: `requireSignedVersion` is on in the app.
        self.fails_with(
            "bound to version", pubkey_b64=CRATE_PUBKEY, sig_text=PREHASHED_SIG, data=b"test", file_name="test"
        )

    def test_garbage_is_a_signature_error(self):
        self.fails_with("base64", sig_text="not base64 !!")
        self.fails_with("four-line", sig_text=base64.b64encode(b"one line").decode())

    def test_the_command_line_exit_codes(self):
        with tempfile.TemporaryDirectory() as tmp:
            conf = os.path.join(tmp, "tauri.conf.json")
            with open(conf, "w") as fh:
                json.dump({"plugins": {"updater": {"pubkey": PUBKEY}}}, fh)
            exe = os.path.join(tmp, NAME)
            with open(exe, "wb") as fh:
                fh.write(DATA)
            with open(exe + ".sig", "w") as fh:
                fh.write(SIGNATURE)
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(v.main(["--conf", conf, "--version", "0.2.1", exe]), 0)
                self.assertEqual(v.main(["--conf", conf, "--version", "0.9.9", exe]), 1)

    def test_a_missing_or_unreadable_input_is_a_failure_with_a_message(self):
        # The release step runs this with `set -e`; an unsigned installer (no .sig) must stop
        # the release with a sentence, not with a Python traceback.
        with tempfile.TemporaryDirectory() as tmp:
            conf = os.path.join(tmp, "tauri.conf.json")
            with open(conf, "w") as fh:
                json.dump({"plugins": {"updater": {"pubkey": PUBKEY}}}, fh)
            exe = os.path.join(tmp, NAME)
            with open(exe, "wb") as fh:
                fh.write(DATA)
            for argv, needle in (
                (["--conf", conf, "--version", "0.2.1", exe], ".sig"),
                (["--conf", os.path.join(tmp, "nope.json"), "--version", "0.2.1", exe], "nope.json"),
                (["--conf", conf, "--version", "0.2.1", os.path.join(tmp, "missing.exe")], "missing.exe"),
            ):
                err = io.StringIO()
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
                    self.assertEqual(v.main(argv), 1, argv)
                self.assertIn(needle, err.getvalue())
            with open(conf, "w") as fh:
                fh.write("{}")
            with open(exe + ".sig", "w") as fh:
                fh.write(SIGNATURE)
            err = io.StringIO()
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
                self.assertEqual(v.main(["--conf", conf, "--version", "0.2.1", exe]), 1)
            self.assertIn("plugins.updater.pubkey", err.getvalue())


if __name__ == "__main__":
    unittest.main()
