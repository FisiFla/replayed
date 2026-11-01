#!/usr/bin/env python3
"""Verify an updater artifact's minisign signature against the key pinned in the app.

The release pipeline signs the installer with `tauri signer sign`, and the installed
application accepts an update only if that signature verifies against `plugins.updater.pubkey`
in `apps/desktop/src-tauri/tauri.conf.json`. `tauri build` and `tauri signer sign` do not
check the other direction: given a private key that does not belong to the pinned public
key, the CLI signs anyway and only *warns* ("does not match the public key"). The release
would then publish an update no installed copy will ever accept — and, worse, nobody finds out
until a user's update fails.

So the pipeline checks it itself, before anything is published, with this script. It repeats
what `tauri-plugin-updater` does when it installs an update (`verify_signature`, which calls
`minisign_verify::PublicKey::verify`), and is stricter in three places:

  - it accepts only the prehashed (`ED`) signature the Tauri CLI writes. The plugin passes
    `allow_legacy = true`; nothing in this pipeline makes a legacy signature, so one showing
    up is a surprise worth failing on, not a compatibility path to keep open;
  - it requires the trusted comment to name *this file*, so a signature for another
    artifact cannot be paired with this one;
  - it requires the trusted comment to carry the version being released
    (`requireSignedVersion` is on in `tauri.conf.json`, so an updater rejects a signature
    without one).

Ed25519 is implemented here, from RFC 8032 section 6, in the standard library, because the
runner has no minisign and installing one would be one more thing to trust at the one step
that guards the signing key. It is a verifier only (no secret ever passes through it), it is
slow (well under a second for a signature) and it is checked against the RFC's test vectors
and against signatures made by the real tools in `scripts/test_verify_updater_signature.py`.

Usage:
    verify-updater-signature.py --conf tauri.conf.json --version 0.2.1 \\
        [--sig FILE.sig] FILE
Exit status: 0 on a good signature, 1 on anything else.
"""

import argparse
import base64
import hashlib
import json
import os
import sys

# ---------------------------------------------------------------------------------------
# Ed25519 verification (RFC 8032 section 6, "5.1.7. Verify").
# ---------------------------------------------------------------------------------------
P = 2**255 - 19
Q = 2**252 + 27742317777372353535851937790883648493
D = -121665 * pow(121666, P - 2, P) % P
SQRT_M1 = pow(2, (P - 1) // 4, P)


def _recover_x(y, sign):
    if y >= P:
        return None
    xx = (y * y - 1) * pow(D * y * y + 1, P - 2, P) % P
    if xx == 0:
        return None if sign else 0
    x = pow(xx, (P + 3) // 8, P)
    if (x * x - xx) % P != 0:
        x = x * SQRT_M1 % P
    if (x * x - xx) % P != 0:
        return None
    if (x & 1) != sign:
        x = P - x
    return x


_GY = 4 * pow(5, P - 2, P) % P
_GX = _recover_x(_GY, 0)
G = (_GX, _GY, 1, _GX * _GY % P)  # extended coordinates (X, Y, Z, T)


def _add(a, b):
    a1, b1, c1, d1 = a
    a2, b2, c2, d2 = b
    aa = (b1 - a1) * (b2 - a2) % P
    bb = (b1 + a1) * (b2 + a2) % P
    cc = 2 * d1 * d2 * D % P
    dd = 2 * c1 * c2 % P
    e, f, g, h = bb - aa, dd - cc, dd + cc, bb + aa
    return (e * f % P, g * h % P, f * g % P, e * h % P)


def _mul(s, point):
    result = (0, 1, 1, 0)
    while s:
        if s & 1:
            result = _add(result, point)
        point = _add(point, point)
        s >>= 1
    return result


def _encode(point):
    x, y, z, _ = point
    zi = pow(z, P - 2, P)
    x, y = x * zi % P, y * zi % P
    return (y | ((x & 1) << 255)).to_bytes(32, "little")


def _decode(raw):
    if len(raw) != 32:
        return None
    y = int.from_bytes(raw, "little")
    sign = y >> 255
    y &= (1 << 255) - 1
    x = _recover_x(y, sign)
    if x is None:
        return None
    return (x, y, 1, x * y % P)


def ed25519_verify(message, public_key, signature):
    """True only for a valid signature; any malformed input is a False."""
    if len(public_key) != 32 or len(signature) != 64:
        return False
    a = _decode(public_key)
    if a is None or _decode(signature[:32]) is None:
        return False
    s = int.from_bytes(signature[32:], "little")
    if s >= Q:
        return False
    h = int.from_bytes(hashlib.sha512(signature[:32] + public_key + message).digest(), "little") % Q
    # [S]B == R + [h]A, checked as [S]B - [h]A == R by comparing encodings.
    neg_a = ((P - a[0]) % P, a[1], a[2], (P - a[3]) % P)
    return _encode(_add(_mul(s, G), _mul(h, neg_a))) == signature[:32]


# ---------------------------------------------------------------------------------------
# minisign formats.
# ---------------------------------------------------------------------------------------
class SignatureError(Exception):
    pass


def _b64(text, what):
    try:
        return base64.b64decode(text.strip(), validate=True)
    except ValueError as err:
        raise SignatureError("%s is not valid base64: %s" % (what, err))


def decode_public_key(pubkey_b64):
    """The `plugins.updater.pubkey` value: base64 of a minisign `.pub` file."""
    lines = _b64(pubkey_b64, "the pinned public key").decode("utf-8", "strict").splitlines()
    if len(lines) < 2:
        raise SignatureError("the pinned public key is not a two-line minisign key file")
    raw = _b64(lines[1], "the pinned public key line")
    if len(raw) != 42 or raw[:2] not in (b"Ed", b"ED"):
        raise SignatureError("the pinned public key is not a minisign Ed25519 key")
    return raw[2:10], raw[10:]  # key id, key


def decode_signature(sig_text):
    """A `.sig` file: base64 of a minisign signature file (what `latest.json` carries)."""
    lines = _b64(sig_text, "the signature file").decode("utf-8", "strict").splitlines()
    if len(lines) < 4 or not lines[2].startswith("trusted comment: "):
        raise SignatureError("the signature is not a four-line minisign signature")
    first = _b64(lines[1], "the signature line")
    glob = _b64(lines[3], "the global signature line")
    if len(first) != 74 or len(glob) != 64:
        raise SignatureError("the signature has the wrong length")
    return {
        "alg": first[:2],
        "key_id": first[2:10],
        "sig": first[10:],
        "trusted": lines[2][len("trusted comment: ") :],
        "global": glob,
    }


def verify(data, pubkey_b64, sig_text, file_name, version):
    """Raise SignatureError unless `data` carries a good signature for this release."""
    key_id, key = decode_public_key(pubkey_b64)
    sig = decode_signature(sig_text)
    if sig["alg"] != b"ED":
        raise SignatureError(
            "the signature is not a prehashed (ED) minisign signature; the Tauri CLI never "
            "writes any other kind"
        )
    if sig["key_id"] != key_id:
        raise SignatureError(
            "the signature was made by key %s but tauri.conf.json pins key %s: the secret "
            "TAURI_SIGNING_PRIVATE_KEY is not the pinned key's private half; an installed "
            "app would reject this update"
            % (sig["key_id"][::-1].hex().upper(), key_id[::-1].hex().upper())
        )
    prehash = hashlib.blake2b(data, digest_size=64).digest()
    if not ed25519_verify(prehash, key, sig["sig"]):
        raise SignatureError("the signature does not verify against the pinned public key")
    # The global signature covers the trusted comment; only now is the comment trustworthy.
    if not ed25519_verify(sig["sig"] + sig["trusted"].encode("utf-8"), key, sig["global"]):
        raise SignatureError("the trusted comment is not covered by a valid signature")
    fields = dict(f.split(":", 1) for f in sig["trusted"].split("\t") if ":" in f)
    if fields.get("file") != file_name:
        raise SignatureError(
            "the signature is for %r, not for %r" % (fields.get("file"), file_name)
        )
    if fields.get("version") != version:
        raise SignatureError(
            "the signature is bound to version %r, but this release is %r (requireSignedVersion "
            "is on, so an updater would reject it)" % (fields.get("version"), version)
        )


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("file", help="the signed artifact (the NSIS installer)")
    parser.add_argument("--sig", help="signature file (default: FILE.sig)")
    parser.add_argument("--conf", required=True, help="tauri.conf.json holding the pinned key")
    parser.add_argument("--version", required=True, help="the version being released")
    args = parser.parse_args(argv)

    # Anything that cannot be read is a failed check with a sentence, never a traceback and
    # never a pass.
    sig_path = args.sig or args.file + ".sig"
    try:
        with open(args.conf, encoding="utf-8") as fh:
            pubkey = json.load(fh)["plugins"]["updater"]["pubkey"]
        with open(args.file, "rb") as fh:
            data = fh.read()
        with open(sig_path, encoding="utf-8") as fh:
            sig_text = fh.read()
    except OSError as err:
        print("updater signature check FAILED: cannot read %s: %s" % (err.filename, err.strerror), file=sys.stderr)
        return 1
    except (ValueError, KeyError, TypeError) as err:
        print(
            "updater signature check FAILED: %s has no plugins.updater.pubkey (%s)" % (args.conf, err),
            file=sys.stderr,
        )
        return 1
    try:
        verify(data, pubkey, sig_text, os.path.basename(args.file), args.version)
    except SignatureError as err:
        print("updater signature check FAILED: %s" % err, file=sys.stderr)
        return 1
    print(
        "updater signature OK: %s verifies against the key pinned in %s (version %s)"
        % (os.path.basename(args.file), args.conf, args.version)
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
