"""Tests for scripts/check-pe-imports.py (run: python3 -m unittest discover -s scripts).

The parser is exercised on PE images built here, byte for byte, so the test needs no Windows
toolchain and no binary in the repository. It was also compared against `dumpbin /imports`
on the real release build of the desktop shell (497 imports, the same set) and on two system
executables with delay-load tables, which is how the byte layout below was checked.
"""

import contextlib
import importlib.util
import io
import os
import struct
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("check_pe_imports", os.path.join(HERE, "check-pe-imports.py"))
pe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pe)

SECTION_VA = 0x1000
RAW = 0x400


def build_pe(imports, delay=None, pe64=True, text=b""):
    """A minimal PE image: one section holding the import table (and optionally a delay-load one)."""
    buf = bytearray()

    def add(blob, align=8):
        while len(buf) % align:
            buf.append(0)
        rva = SECTION_VA + len(buf)
        buf.extend(blob)
        return rva

    thunk_fmt = "<Q" if pe64 else "<I"

    def thunks(names):
        """(ILT rva) for a list of function names; an `#n` name is an ordinal."""
        entries = []
        for name in names:
            if name.startswith("#"):
                entries.append((1 << (63 if pe64 else 31)) | int(name[1:]))
            else:
                entries.append(add(struct.pack("<H", 0) + name.encode() + b"\0", 2))
        table = b"".join(struct.pack(thunk_fmt, e) for e in entries) + struct.pack(thunk_fmt, 0)
        return add(table)

    descriptors = bytearray()
    for dll, names in imports.items():
        name_rva = add(dll.encode() + b"\0", 1)
        ilt = thunks(names)
        iat = add(bytes(buf[ilt - SECTION_VA : ilt - SECTION_VA + (len(names) + 1) * (8 if pe64 else 4)]))
        descriptors += struct.pack("<IIIII", ilt, 0, 0, name_rva, iat)
    descriptors += bytes(20)
    import_rva = add(bytes(descriptors), 4)

    delay_rva = delay_size = 0
    if delay:
        d = bytearray()
        for dll, names in delay.items():
            name_rva = add(dll.encode() + b"\0", 1)
            int_rva = thunks(names)
            d += struct.pack("<IIIIIIII", 1, name_rva, 0, int_rva, int_rva, 0, 0, 0)
        d += bytes(32)
        delay_rva, delay_size = add(bytes(d), 4), len(d)

    if text:
        add(text, 1)

    opt_size = 240 if pe64 else 224
    dos = bytearray(0x40)
    dos[:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3C, 0x40)
    coff = struct.pack("<HHIIIHH", 0x8664 if pe64 else 0x14C, 1, 0, 0, 0, opt_size, 0x22)
    opt = bytearray(opt_size)
    struct.pack_into("<H", opt, 0, 0x20B if pe64 else 0x10B)
    dirs_off = 112 if pe64 else 96
    struct.pack_into("<I", opt, dirs_off - 4, 16)
    struct.pack_into("<II", opt, dirs_off + 8 * 1, import_rva, len(descriptors))
    struct.pack_into("<II", opt, dirs_off + 8 * 13, delay_rva, delay_size)
    size = len(buf)
    section = struct.pack("<8sIIIIIIHHI", b".idata", size, SECTION_VA, size, RAW, 0, 0, 0, 0, 0x40000040)
    header = bytes(dos) + b"PE\0\0" + coff + bytes(opt) + section
    return header.ljust(RAW, b"\0") + bytes(buf)


class CheckPeImports(unittest.TestCase):
    KERNEL = {"KERNEL32.dll": ["ExitProcess", "GetLastError"]}

    def run_check(self, image, allow=None):
        with tempfile.TemporaryDirectory() as tmp:
            exe = os.path.join(tmp, "app.exe")
            with open(exe, "wb") as fh:
                fh.write(image)
            argv = [exe]
            if allow is not None:
                allow_file = os.path.join(tmp, "allow.txt")
                with open(allow_file, "w") as fh:
                    fh.write(allow)
                argv = ["--allow", allow_file] + argv
            err = io.StringIO()
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
                status = pe.main(argv)
            return status, err.getvalue()

    def test_the_parser_reads_both_pe32_and_pe32_plus(self):
        for pe64 in (True, False):
            imports = pe.imports(build_pe({**self.KERNEL, "USER32.dll": ["RegisterHotKey", "#42"]}, pe64=pe64))
            self.assertIn(("KERNEL32.dll", "ExitProcess"), imports)
            self.assertIn(("USER32.dll", "RegisterHotKey"), imports)
            self.assertIn(("USER32.dll", "#42"), imports)

    def test_a_clean_binary_passes(self):
        status, _ = self.run_check(build_pe({**self.KERNEL, "USER32.dll": ["RegisterHotKey", "GetMessageW"]}))
        self.assertEqual(status, 0)

    def test_every_forbidden_import_fails(self):
        for name in (
            "SendInput", "keybd_event", "mouse_event", "SetWindowsHookExW", "SetWindowsHookExA",
            "ReadProcessMemory", "WriteProcessMemory", "CreateRemoteThread", "CreateRemoteThreadEx",
            "OpenProcess", "VirtualAllocEx", "SetWinEventHook", "QueueUserAPC",
        ):
            status, err = self.run_check(build_pe({**self.KERNEL, "USER32.dll": [name]}))
            self.assertEqual(status, 1, name)
            self.assertIn(name, err)

    def test_matching_is_case_insensitive(self):
        status, _ = self.run_check(build_pe({**self.KERNEL, "user32.dll": ["sendinput"]}))
        self.assertEqual(status, 1)

    def test_a_delay_loaded_import_is_an_import(self):
        status, err = self.run_check(build_pe(self.KERNEL, delay={"USER32.dll": ["SendInput"]}))
        self.assertEqual(status, 1)
        self.assertIn("SendInput", err)

    def test_names_that_only_look_similar_pass(self):
        # A process asking about itself, and a longer unrelated name.
        status, _ = self.run_check(
            build_pe({**self.KERNEL, "ADVAPI32.dll": ["OpenProcessToken", "SendInputTimeoutX"]})
        )
        self.assertEqual(status, 0)

    def test_a_name_resolved_at_run_time_is_caught_as_text(self):
        status, err = self.run_check(build_pe(self.KERNEL, text=b"\0SendInput\0"))
        self.assertEqual(status, 1)
        self.assertIn("as text", err)

    def test_the_allowlist_is_per_name_and_explicit(self):
        image = build_pe({**self.KERNEL, "USER32.dll": ["SendInput", "SetWindowsHookExW"]})
        status, err = self.run_check(image, allow="SendInput  # tao, tracked\n")
        self.assertEqual(status, 1)
        self.assertIn("SetWindowsHookExW", err)
        self.assertNotIn("imports user32.dll!SendInput", err.lower().replace("USER32", "user32"))
        status, _ = self.run_check(image, allow="SendInput # a reason\nSetWindowsHookExW # another\n")
        self.assertEqual(status, 0)

    def test_an_unreadable_file_is_a_failure_never_a_pass(self):
        self.assertEqual(self.run_check(b"not a pe file at all" * 10)[0], 2)
        self.assertEqual(self.run_check(b"MZ" + bytes(0x100))[0], 2)
        # A well-formed image that imports nothing from kernel32 means the parser understood
        # nothing, and must not read as clean.
        self.assertEqual(self.run_check(build_pe({"USER32.dll": ["RegisterHotKey"]}))[0], 2)

    def test_a_missing_file_is_a_failure_with_a_message_not_a_traceback(self):
        err = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
            status = pe.main(["/no/such/dir/app.exe"])
        self.assertEqual(status, 2)
        self.assertIn("/no/such/dir/app.exe", err.getvalue())


if __name__ == "__main__":
    unittest.main()
