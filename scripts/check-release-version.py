#!/usr/bin/env python3
"""Check that every place a version is written agrees, and that the release tag names it.

The version of replayed is written in six places, and a release is only coherent if they
all say the same thing:

  - `Cargo.toml`, `[workspace.package]`: the version of every crate in the workspace, and so
    of `replayed --version` and of the logs in a bug report;
  - `apps/desktop/src-tauri/Cargo.toml`, `[package]`: the desktop shell, a separate workspace
    that does not inherit the one above;
  - `apps/desktop/package.json` and `apps/desktop/package-lock.json` (twice: the top-level
    field and `packages[""]`);
  - `apps/desktop/src-tauri/tauri.conf.json`: the version the installer is named after, the
    one the updater compares, and the one `requireSignedVersion` binds a signature to;
  - `CHANGELOG.md`: a `## [<version>]` section, which is what the update panel shows. A
    pre-release (`0.3.0-rc.1`) may use its release's section (`## [0.3.0]`) instead, because
    nobody writes a changelog entry per candidate.

An earlier release step compared the tag with `tauri.conf.json` alone and told the maintainer
to bump the rest by hand; forgetting one shipped an installer whose own metadata disagreed
with the tag. `Cargo.lock` is not read here: `cargo --locked` fails the build when the lock
and the manifests disagree, which is the same check done by the tool that owns the file.

Usage:
    check-release-version.py [--root DIR] [--tag vX.Y.Z[-pre]]
Without `--tag` it checks that the sources agree. Prints `version=<x.y.z>` on success, so a
workflow can read it; exit status 1 when anything disagrees, listing every disagreement.
"""

import argparse
import json
import os
import re
import sys

# SemVer 2.0.0 without build metadata (`+...`): numeric identifiers carry no leading zero, and
# a pre-release is dot-separated alphanumerics. A GitHub release tag is this with a `v`.
SEMVER = re.compile(
    r"^(?P<core>(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))"
    r"(?:-(?P<pre>(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?$"
)


def toml_version(text, section):
    """The `version` key of one `[section]`: a string, or the marker for `version.workspace`."""
    in_section = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_section = stripped == "[%s]" % section
            continue
        if not in_section:
            continue
        match = re.match(r'^version\s*=\s*"([^"]*)"', stripped)
        if match:
            return match.group(1)
        if re.match(r"^version\.workspace\s*=\s*true", stripped):
            return "workspace"
    return None


def read(root, path):
    with open(os.path.join(root, path), encoding="utf-8") as fh:
        return fh.read()


def sources(root):
    """(label, version) for each place a version is written; None where it is not found."""
    found = []
    found.append(("Cargo.toml [workspace.package]", toml_version(read(root, "Cargo.toml"), "workspace.package")))
    shell = toml_version(read(root, "apps/desktop/src-tauri/Cargo.toml"), "package")
    if shell != "workspace":  # inherits: then it is the line above, not a second source
        found.append(("apps/desktop/src-tauri/Cargo.toml [package]", shell))
    package = json.loads(read(root, "apps/desktop/package.json"))
    found.append(("apps/desktop/package.json", package.get("version")))
    lock = json.loads(read(root, "apps/desktop/package-lock.json"))
    found.append(("apps/desktop/package-lock.json", lock.get("version")))
    found.append(("apps/desktop/package-lock.json packages[\"\"]", lock.get("packages", {}).get("", {}).get("version")))
    conf = json.loads(read(root, "apps/desktop/src-tauri/tauri.conf.json"))
    found.append(("apps/desktop/src-tauri/tauri.conf.json", conf.get("version")))
    return found


def changelog_has(root, version):
    wanted = {version}
    match = SEMVER.match(version)
    if match and match.group("pre"):
        wanted.add(match.group("core"))
    for line in read(root, "CHANGELOG.md").splitlines():
        heading = re.match(r"^## \[([^\]]+)\]", line)
        if heading and heading.group(1) in wanted:
            return True
    return False


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--root", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
    parser.add_argument("--tag", help="the release tag, e.g. v0.2.1 or v0.3.0-rc.1")
    args = parser.parse_args(argv)

    problems = []
    expected = None
    if args.tag is not None:
        if not args.tag.startswith("v") or not SEMVER.match(args.tag[1:]):
            print(
                "the release tag %r is not a version tag: it must be v<major>.<minor>.<patch> "
                "with an optional -<pre-release> (v0.2.1, v0.3.0-rc.1)" % args.tag,
                file=sys.stderr,
            )
            return 1
        expected = args.tag[1:]

    found = sources(args.root)
    reference = expected if expected is not None else next((v for _, v in found if v), None)
    for label, value in found:
        if value != reference:
            problems.append(
                "%s says %r; %s is %r"
                % (label, value, "the tag" if expected is not None else "the first source", reference)
            )
    if reference and not changelog_has(args.root, reference):
        problems.append("CHANGELOG.md has no `## [%s]` section (the update panel shows it)" % reference)

    if problems:
        print("the version is not consistent:", file=sys.stderr)
        for problem in problems:
            print("  - " + problem, file=sys.stderr)
        print(
            "Bump every one of them in the same commit, then tag that commit.", file=sys.stderr
        )
        return 1
    print("version=%s" % reference)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
