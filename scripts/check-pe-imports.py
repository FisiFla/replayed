#!/usr/bin/env python3
"""Anti-cheat guard, binary half (docs/anti-cheat.md).

`scripts/check-forbidden-apis.sh` reads *our* source. That leaves two holes: a dependency
that calls a forbidden API (the source never mentions it), and a call the grep pattern does
not name. This script closes the first one at the only place that is true of every
dependency at once: the **import table of the executable that ships**.

An import table is the list of Windows functions the loader will bind when the program
starts. A function the program can call without resolving it by hand has to be on that
list, so whatever a dependency does through the ordinary linker route shows up here whatever
the dependency calls its own wrapper. `LoadLibrary` + `GetProcAddress` by name is the other
route, and it is why the **string scan** below exists: the name of a function that is looked
up at run time has to be in the file as text.

It reads the PE file itself, in Python's standard library, instead of calling `dumpbin` or
`llvm-objdump`. That is deliberate: the same check then runs on a Windows CI runner, on a
developer's Mac and in a unit test, with no Visual Studio shell and no LLVM install, and
a parser this small can be read in a review. `dumpbin /imports` is the cross-check to run by
hand if the result is ever in doubt.

Both tables are read: the ordinary import table and the delay-load table. A delay-loaded
import is bound on the first call instead of at start-up, and it is still an import.

Usage:
    check-pe-imports.py [--list] [--allow FILE] <exe-or-dll> [...]

Exit status: 0 when no forbidden function is imported or named, 1 when one is, 2 when the
file cannot be read as a PE image. An unreadable or import-less file is a failure and never a
pass: a check that passes because it understood nothing is the failure this exists to avoid.

The allowlist (`--allow FILE`, one `Name  # reason` per line) is the only way past a match.
It is explicit and it is per name, so an exemption is a reviewed line in the repository and
not a silent default.
"""

import argparse
import re
import struct
import sys

# What the policy forbids a *binary* to import: reaching into another process, hooking, and
# synthesising input. Whole names, compared case-insensitively, with the A/W/Ex spellings
# spelled out, so `OpenProcessToken` (a process asking about itself) is not `OpenProcess`.
#
# Deliberately NOT here: `PostMessage`, `SendMessage`, `GetProcAddress`, `LoadLibrary*`, and
# `SetCursorPos`. The window toolkit and the C runtime import the first four for their own
# windows and their own DLLs, and tao imports `SetCursorPos` for `Window::set_cursor_position`,
# which moves the pointer and is neither a key nor a click. A name here is a hard failure of
# the release, so each one has to be a call no legitimate dependency makes.
#
# The source guard bans the first four in *our* code, where the question is meaningful; a
# binary-level ban would fail every Windows program. The string scan below is what watches
# `GetProcAddress` being used to reach a name on this list.
FORBIDDEN = [
    # Synthesised input.
    r"SendInput[AW]?",
    r"keybd_event",
    r"mouse_event",
    r"InjectKeyboardInput",
    r"InjectMouseInput",
    r"InjectTouchInput",
    r"InjectSyntheticPointerInput",
    r"AttachThreadInput",
    r"BlockInput",
    # Hooks.
    r"SetWindowsHookEx[AW]?",
    r"SetWinEventHook",
    # Another process's memory, threads and handles.
    r"ReadProcessMemory",
    r"WriteProcessMemory",
    r"VirtualAllocEx",
    r"VirtualProtectEx",
    r"VirtualQueryEx",
    r"CreateRemoteThread(Ex)?",
    r"OpenProcess",
    r"NtOpenProcess",
    r"NtReadVirtualMemory",
    r"NtWriteVirtualMemory",
    r"ZwReadVirtualMemory",
    r"ZwWriteVirtualMemory",
    r"NtCreateThreadEx",
    r"RtlCreateUserThread",
    r"QueueUserAPC",
    r"SetThreadContext",
]
FORBIDDEN_RE = re.compile(r"^(?:%s)$" % "|".join(FORBIDDEN), re.IGNORECASE)

# The same names as they would appear as *text* inside the image (a `GetProcAddress` operand,
# a string literal). Bounded by non-identifier bytes, so `SendInputTimeout` is not `SendInput`
# and a longer identifier that merely contains a name is not a hit.
STRING_SCAN_NAMES = FORBIDDEN
STRING_RE = re.compile(
    rb"(?<![A-Za-z0-9_])(?:%s)(?![A-Za-z0-9_])" % "|".join(STRING_SCAN_NAMES).encode(),
    re.IGNORECASE,
)


class NotAPe(Exception):
    pass


def read_pe(data):
    """Return (sections, import_dirs) from a PE image; raises NotAPe on a malformed one."""
    if len(data) < 0x40 or data[:2] != b"MZ":
        raise NotAPe("no MZ header")
    (e_lfanew,) = struct.unpack_from("<I", data, 0x3C)
    if e_lfanew + 24 > len(data) or data[e_lfanew : e_lfanew + 4] != b"PE\0\0":
        raise NotAPe("no PE signature")
    coff = e_lfanew + 4
    (_machine, n_sections, _ts, _symptr, _nsyms, opt_size, _chars) = struct.unpack_from(
        "<HHIIIHH", data, coff
    )
    opt = coff + 20
    (magic,) = struct.unpack_from("<H", data, opt)
    if magic == 0x20B:  # PE32+
        pe64, dirs_off = True, opt + 112
    elif magic == 0x10B:  # PE32
        pe64, dirs_off = False, opt + 96
    else:
        raise NotAPe("unknown optional-header magic 0x%x" % magic)
    (n_dirs,) = struct.unpack_from("<I", data, dirs_off - 4)

    def directory(index):
        if index >= n_dirs:
            return (0, 0)
        return struct.unpack_from("<II", data, dirs_off + 8 * index)

    sec_off = opt + opt_size
    sections = []
    for i in range(n_sections):
        (vsize, va, rsize, raw) = struct.unpack_from("<IIII", data, sec_off + 40 * i + 8)
        sections.append((va, max(vsize, rsize), raw, rsize))
    return pe64, sections, directory(1), directory(13)


def make_rva_reader(data, sections):
    def to_offset(rva):
        for va, size, raw, raw_size in sections:
            if va <= rva < va + size:
                off = raw + (rva - va)
                if rva - va >= raw_size or off >= len(data):
                    break
                return off
        raise NotAPe("RVA 0x%x is outside every section" % rva)

    def cstring(rva):
        off = to_offset(rva)
        end = data.index(b"\0", off)
        return data[off:end].decode("ascii", "replace")

    return to_offset, cstring


def thunk_names(data, to_offset, cstring, table_rva, pe64):
    """Names (or `#ordinal`) in one import-name table, which ends at a zero thunk."""
    size = 8 if pe64 else 4
    flag = 1 << (63 if pe64 else 31)
    out = []
    off = to_offset(table_rva)
    while True:
        (thunk,) = struct.unpack_from("<Q" if pe64 else "<I", data, off)
        if thunk == 0:
            return out
        if thunk & flag:
            out.append("#%d" % (thunk & 0xFFFF))
        else:
            out.append(cstring((thunk & 0x7FFFFFFF) + 2))  # skip the 2-byte hint
        off += size


def imports(data):
    """Every (dll, function) the image imports, ordinary and delay-loaded."""
    pe64, sections, imp, delay = read_pe(data)
    to_offset, cstring = make_rva_reader(data, sections)
    found = []

    rva, size = imp
    if rva and size:
        off = to_offset(rva)
        while True:
            (oft, _ts, _fwd, name, first) = struct.unpack_from("<IIIII", data, off)
            if oft == 0 and name == 0 and first == 0:
                break
            table = oft or first  # a bound image has no original thunks
            for fn in thunk_names(data, to_offset, cstring, table, pe64):
                found.append((cstring(name), fn))
            off += 20

    rva, size = delay
    if rva and size:
        off = to_offset(rva)
        while True:
            (attrs, name, _mod, _iat, int_rva) = struct.unpack_from("<IIIII", data, off)
            if attrs == 0 and name == 0:
                break
            if not attrs & 1:
                raise NotAPe("delay-load descriptor uses virtual addresses, not RVAs")
            for fn in thunk_names(data, to_offset, cstring, int_rva, pe64):
                found.append((cstring(name), fn))
            off += 32

    return found


def load_allowlist(path):
    names = set()
    if not path:
        return names
    with open(path, encoding="utf-8") as fh:
        for raw in fh:
            line = raw.split("#", 1)[0].strip()
            if line:
                names.add(line.lower())
    return names


def check(path, allow, show_list):
    try:
        with open(path, "rb") as fh:
            data = fh.read()
    except OSError as err:
        print("%s: cannot be read: %s" % (path, err.strerror or err), file=sys.stderr)
        return 2
    try:
        found = imports(data)
    except (NotAPe, struct.error, ValueError) as err:
        print("%s: cannot read the import table: %s" % (path, err), file=sys.stderr)
        return 2
    # A real Windows executable always imports something (the loader needs kernel32). An
    # empty table means the parser understood nothing, and that must not read as "clean".
    if not any(dll.lower() == "kernel32.dll" for dll, _ in found):
        print("%s: no kernel32 import found; refusing to call this clean" % path, file=sys.stderr)
        return 2

    if show_list:
        for dll, fn in sorted(set(found), key=lambda p: (p[0].lower(), p[1].lower())):
            print("%s!%s" % (dll, fn))

    failures = []
    for dll, fn in found:
        if FORBIDDEN_RE.match(fn) and fn.lower() not in allow:
            failures.append("imports %s!%s" % (dll, fn))
    seen = set()
    for match in STRING_RE.finditer(data):
        text = match.group(0).decode("ascii")
        if text.lower() in allow or text.lower() in seen:
            continue
        seen.add(text.lower())
        # A name that is also an import was reported above; a name that is only text is a
        # lookup by name (GetProcAddress) or a string literal, and either deserves a human.
        if not any(fn.lower() == text.lower() for _, fn in found):
            failures.append("names %s as text, at byte %d (resolved at run time?)" % (text, match.start()))

    if failures:
        print("%s: forbidden Windows API (docs/anti-cheat.md):" % path, file=sys.stderr)
        for failure in failures:
            print("  - " + failure, file=sys.stderr)
        return 1
    print("%s: %d imports, none forbidden" % (path, len(set(found))))
    return 0


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("files", nargs="+", help="PE images to check")
    parser.add_argument("--list", action="store_true", help="print every import first")
    parser.add_argument("--allow", help="allowlist file: one `Name  # reason` per line")
    args = parser.parse_args(argv)
    allow = load_allowlist(args.allow)
    status = 0
    for path in args.files:
        status = max(status, check(path, allow, args.list))
    return status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
