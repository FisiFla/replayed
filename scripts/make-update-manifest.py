#!/usr/bin/env python3
"""Write `latest.json`, the update manifest the installed application reads.

Its shape is the one `tauri-plugin-updater` parses:

    {
      "version": "0.2.1",
      "notes": "...this version's changelog section...",
      "pub_date": "2026-10-24T10:00:00.000Z",
      "platforms": {
        "windows-x86_64-nsis": { "signature": "...", "url": "..." },
        "windows-x86_64":      { "signature": "...", "url": "..." }
      }
    }

The two platform keys are one artifact under the two names the updater looks for: it tries
`{os}-{arch}-{bundle type}` first (`windows-x86_64-nsis`, which the bundler records in a
released binary) and falls back to `{os}-{arch}` for a build with no bundle type, such as a
development build.

THE ARTIFACT IS THE INSTALLER. With `bundle.createUpdaterArtifacts: true` (the Tauri 2 mode)
the bundler signs `replayed_<version>_x64-setup.exe` itself and writes `<name>.sig` beside it;
the updater downloads that executable and runs it. The `*.nsis.zip` of the Tauri 1 layout is
only produced under `"v1Compatible"`, which this project does not use. The release step this
replaces looked for the zip, found nothing, and could never publish; this script refuses any
artifact that is not the `-setup.exe`, so the same mistake cannot come back as a manifest that
names a file the release does not carry.

The signature is the content of the `.sig` file: base64 of a minisign signature, on one line.
The application checks the bytes it downloads against it and the key pinned in
`tauri.conf.json`; `scripts/verify-updater-signature.py` makes the same check in the pipeline
before anything is published.
"""

import argparse
import base64
import datetime
import json
import re
import sys


def fail(message):
    print("update manifest: " + message, file=sys.stderr)
    return 1


def changelog_notes(text, version):
    """This version's section of the changelog (or its release's, for a pre-release), or None."""
    candidates = [version]
    core = re.match(r"^(\d+\.\d+\.\d+)-", version)
    if core:
        candidates.append(core.group(1))
    for candidate in candidates:
        pattern = r"(?ms)^## \[%s\][^\n]*\n(.*?)(?=^## \[|\Z)" % re.escape(candidate)
        match = re.search(pattern, text)
        if match:
            return match.group(1).strip()
    return None


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--version", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--repo", required=True, help="owner/name, for the download URL")
    parser.add_argument("--installer", required=True, help="file name of the signed -setup.exe")
    parser.add_argument("--sig", required=True, help="the installer's .sig file")
    parser.add_argument("--changelog", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--pub-date", help="RFC 3339 timestamp (default: now, UTC)")
    args = parser.parse_args(argv)

    if args.tag != "v" + args.version:
        return fail("the tag %r does not name version %r" % (args.tag, args.version))
    if not re.fullmatch(r"replayed_[0-9A-Za-z.\-]+_x64-setup\.exe", args.installer):
        return fail(
            "%r is not the NSIS installer (replayed_<version>_x64-setup.exe): that file is the "
            "updater artifact under createUpdaterArtifacts: true" % args.installer
        )
    if "_%s_" % args.version not in args.installer:
        return fail("the installer %r does not carry the version %s" % (args.installer, args.version))

    with open(args.sig, encoding="utf-8") as fh:
        lines = [line for line in fh.read().splitlines() if line.strip()]
    if len(lines) != 1:
        return fail("the signature file must hold one line of base64, and holds %d" % len(lines))
    signature = lines[0].strip()
    try:
        base64.b64decode(signature, validate=True)
    except ValueError:
        return fail("the signature file is not base64")

    with open(args.changelog, encoding="utf-8") as fh:
        notes = changelog_notes(fh.read(), args.version)
    if notes is None:
        return fail("CHANGELOG.md has no section for %s" % args.version)

    now = datetime.datetime.now(datetime.timezone.utc)
    pub_date = args.pub_date or now.strftime("%Y-%m-%dT%H:%M:%S.") + "%03dZ" % (now.microsecond // 1000)
    url = "https://github.com/%s/releases/download/%s/%s" % (args.repo, args.tag, args.installer)
    platform = {"signature": signature, "url": url}
    manifest = {
        "version": args.version,
        "notes": notes,
        "pub_date": pub_date,
        "platforms": {"windows-x86_64-nsis": dict(platform), "windows-x86_64": dict(platform)},
    }
    with open(args.out, "w", encoding="utf-8", newline="\n") as fh:
        json.dump(manifest, fh, indent=2, ensure_ascii=False)
        fh.write("\n")
    print("wrote %s: version %s, %s" % (args.out, args.version, url))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
