#!/usr/bin/env bash
# Anti-cheat guard (docs/anti-cheat.md): the project captures with Windows Graphics Capture
# and WASAPI only. Nothing here may inject into, hook, read or write another process, or
# synthesise input.
#
# Two modes, because there are two places the policy can be broken:
#
#   scripts/check-forbidden-apis.sh
#       The SOURCE guard. Fails if such an API appears in the tree's Rust or in a script, if
#       a crate enables a Windows feature group it has no business with, or if the capture
#       backend is not the only enumerator of windows. Runs anywhere (it is grep and awk).
#
#   scripts/check-forbidden-apis.sh --binary <exe-or-dll> [...]
#       The BINARY guard. Fails if the *built* executable imports such an API, or names one
#       as text. This is the half that sees dependencies: a crate we pull in can call
#       `SendInput` without our source ever spelling the name, and the only place that is
#       true of every dependency at once is the import table of what ships. It reads the PE
#       file with scripts/check-pe-imports.py and needs only Python 3, so it runs the same on
#       a developer's machine (point it at any .exe) and in CI, which runs it on the Windows
#       job's build and again in the release workflow on the installer's own binary.
#
# The list is of *names*, and it is long on purpose. A pattern that misses a spelling is a
# hole, and the cost of a false positive is one line in an exemption with a reason; the
# cost of a false negative is an anti-cheat ban. Matching is case-insensitive: PowerShell
# resolves `sendinput` to `SendInput`, and so does a P/Invoke declaration.
#
# THE GUARD'S OWN FILES. They hold the names they search for, so they cannot be scanned.
# They are excluded by exact *path*, never by what a line says: an earlier version piped
# the matches through `grep -v 'check-forbidden-apis.sh'`, which dropped every line that
# merely contained that string, so `[Win32]::SendInput(...)  # check-forbidden-apis.sh`
# in any script passed. A file with the same name anywhere else is scanned like any other.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() {
  echo "$1" >&2
  exit 1
}

# --- the binary guard -------------------------------------------------------------------
if [ "${1:-}" = "--binary" ]; then
  shift
  [ "$#" -gt 0 ] || fail "usage: scripts/check-forbidden-apis.sh --binary <exe-or-dll> [...]"
  python=""
  for candidate in python3 python; do
    # It must *run* as Python 3: on a Windows runner a `python3` can be the Store's stub.
    if "$candidate" -c 'import sys; sys.exit(sys.version_info[0] != 3)' >/dev/null 2>&1; then
      python="$candidate"
      break
    fi
  done
  [ -n "$python" ] || fail "the binary guard needs Python 3 (python3 or python on PATH)"
  allow=()
  if [ -f scripts/pe-imports-allowlist.txt ]; then allow=(--allow scripts/pe-imports-allowlist.txt); fi
  # `-I`: isolated mode, so a stray module in the working directory cannot shadow the
  # standard library the parser imports.
  exec "$python" -I scripts/check-pe-imports.py ${allow[@]+"${allow[@]}"} "$@"
fi
[ "$#" -eq 0 ] || fail "unknown argument: $1 (the only option is --binary <exe>...)"

# --- the source guard -------------------------------------------------------------------

# Injecting, hooking, reading or writing another process, and synthesising input.
# SendKeys / AutoHotkey / WScript.Shell are how a *script* does the same thing without
# naming a Win32 call.
pattern='WriteProcessMemory|ReadProcessMemory|SetWindowsHookEx|SetWinEventHook|SendInput|CreateRemoteThread|NtWriteVirtualMemory|NtReadVirtualMemory|ZwWriteVirtualMemory|VirtualAllocEx|keybd_event|mouse_event|SetThreadContext|GetThreadContext|QueueUserAPC|LoadLibrary([AW])?(Ex)?([AW])?\(|OpenProcess|SuspendThread|ResumeThread|RtlCreateUserThread|NtCreateThreadEx|DeviceIoControl|CreateFileMapping|MapViewOfFile|PostMessage|SendMessage|GetProcAddress|GetModuleHandle[AW]|GetAsyncKeyState|GetKeyState|FindWindow|EnumDesktopWindows|GetForegroundWindow|SendKeys|AutoHotkey|WScript\.Shell|AttachThreadInput|SetCursorPos|Inject(Keyboard|Mouse|Touch)Input|RegisterRawInputDevices|BlockInput|\.[[:space:]]*set_focus[[:space:]]*\('

# `.set_focus(` is in the list although it is not a Win32 name: on Windows, tao implements the
# window's `set_focus` by sending a synthetic Alt key through `SendInput` (see
# `apps/desktop/src-tauri/src/window_focus.rs`), so the call *is* input synthesis even though
# it spells nothing from the list above. The shell raises its own window with
# `window_focus::bring_forward`, which asks `SetForegroundWindow` and flashes the taskbar
# button instead. The pattern is for our own code, where the call site is the thing to stop;
# the vendored tao is patched so the call cannot synthesise input either (apps/desktop/src-tauri/vendor/README.md).

# THE VENDORED CRATES. `[patch.crates-io]` points tao and muda at patched copies under
# `apps/desktop/src-tauri/vendor/` (or a top-level `vendor/`), because both call `SendInput`
# in the version we lock. Those directories are third-party code: scanned, never excluded
# (excluding them would let a re-vendor without the patch put `SendInput` back and pass),
# but with the narrower list below rather than `$pattern`. The full list is written for our
# code and a windowing toolkit trips it by design (`PostMessage`, `SendMessage`,
# `GetProcAddress`, `GetKeyState`, `FindWindow`, `LoadLibrary` are how it runs its own
# windows), and the binary guard deliberately lets those through for the same reason. This
# list is the names the binary guard refuses (scripts/check-pe-imports.py FORBIDDEN), which
# is the contract: nothing vendored may spell what the shipped binary may not import.
# `keybd_event` / `mouse_event` are bounded on the left because tao has `set_ignore_mouse_events`.
vendor_pattern='SendInput|(^|[^A-Za-z0-9_])(keybd_event|mouse_event)|Inject(Keyboard|Mouse|Touch|SyntheticPointer)Input|AttachThreadInput|BlockInput|SetWindowsHookEx|SetWinEventHook|ReadProcessMemory|WriteProcessMemory|VirtualAllocEx|VirtualProtectEx|VirtualQueryEx|CreateRemoteThread|OpenProcess|NtOpenProcess|NtReadVirtualMemory|NtWriteVirtualMemory|ZwReadVirtualMemory|ZwWriteVirtualMemory|NtCreateThreadEx|RtlCreateUserThread|QueueUserAPC|SetThreadContext|SendKeys'
# Exactly these two prefixes are "vendored". A directory merely *named* `vendor` elsewhere in
# the tree is our code and gets the full list.
vendor_roots='^(vendor|apps/desktop/src-tauri/vendor)/'

# Enumerating another process's windows. The capture backend does this to find its target;
# nothing else may.
window_pattern='EnumWindows|EnumDesktopWindows|FindWindow|GetForegroundWindow'
capture_backend='crates/capture/src/wgc.rs'

# The guard's own files, by exact path: the names they hold are data, not calls.
guard_files=(
  scripts/check-forbidden-apis.sh
  scripts/check-pe-imports.py
  scripts/test_check_pe_imports.py
  scripts/test-check-forbidden-apis.sh
  scripts/pe-imports-allowlist.txt
)
guard_filter=()
for f in "${guard_files[@]}"; do guard_filter+=(-e "$f"); done

# Every file under the given roots that has one of the given extensions, minus the guard's
# own files and build output. NUL-separated so a path with a space cannot split.
files_under() {
  local exts="$1"
  shift
  local roots=()
  local root
  for root in "$@"; do
    if [ -d "$root" ]; then roots+=("$root"); fi
  done
  [ "${#roots[@]}" -gt 0 ] || return 0
  local expr=()
  local ext
  for ext in $exts; do
    if [ "${#expr[@]}" -gt 0 ]; then expr+=(-o); fi
    expr+=(-name "*.$ext")
  done
  find "${roots[@]}" -type f \( "${expr[@]}" \) \
    -not -path '*/node_modules/*' -not -path '*/target/*' -not -path '*/dist/*' \
    | { grep -vxF "${guard_filter[@]}" || true; } | tr '\n' '\0'
}

# Split a NUL-separated file list into our code and the vendored crates.
not_vendored() { tr '\0' '\n' | { grep -vE "$vendor_roots" || true; } | tr '\n' '\0'; }
only_vendored() { tr '\0' '\n' | { grep -E "$vendor_roots" || true; } | tr '\n' '\0'; }

# Offending lines in the given files: matches of $1 that are not a whole-line comment
# ($2 is the comment-prefix regex). The comment filter lets a file that *talks* about a
# forbidden API stay clean, and drops nothing that is code: a trailing comment on a code
# line is still an offender. Works under BSD and GNU grep.
offenders() {
  local regex="$1" comment="$2"
  xargs -0 -r grep -nHEi -- "$regex" 2>/dev/null | grep -vE "^[^:]+:[0-9]+:[[:space:]]*($comment)" || true
}

# The Rust tree: our code against the full list, the vendored crates against the narrower one.
rust_hits=$(
  files_under 'rs' crates apps xtask vendor | not_vendored | offenders "$pattern" '//'
  files_under 'rs' crates apps xtask vendor | only_vendored | offenders "$vendor_pattern" '//'
)
if [ -n "$rust_hits" ]; then
  echo "$rust_hits" >&2
  fail "forbidden process-manipulation / input-synthesis API found (see docs/anti-cheat.md)"
fi

# The scripts, which are not Rust and are therefore not covered above: a script that
# synthesises input on the machine the game runs on is the same breach. Every language a
# helper in this repository could be written in, for the same reason. The comment prefix is
# per language, so that `//` is not treated as a comment in a PowerShell file, where it is
# not one.
script_hits=$(
  files_under 'ps1 psm1 py sh' scripts apps/desktop/scripts xtask | offenders "$pattern" '#'
  files_under 'bat cmd' scripts apps/desktop/scripts xtask | offenders "$pattern" 'REM[[:space:]]|::'
  files_under 'js mjs cjs ts' scripts apps/desktop/scripts xtask | offenders "$pattern" '//'
)
if [ -n "$script_hits" ]; then
  echo "$script_hits" >&2
  fail "forbidden process-manipulation / input-synthesis API found in a script (see docs/anti-cheat.md)"
fi

# The window enumeration belongs to the capture backend and nowhere else.
window_hits=$(
  files_under 'rs' crates apps xtask | not_vendored | offenders "$window_pattern" '//'
  files_under 'ps1 py' scripts apps/desktop/scripts | offenders "$window_pattern" '#'
  files_under 'bat cmd' scripts apps/desktop/scripts | offenders "$window_pattern" 'REM[[:space:]]|::'
)
window_hits=$(printf '%s\n' "$window_hits" | { grep -v "^$capture_backend:" || true; })
if [ -n "$window_hits" ]; then
  echo "$window_hits" >&2
  fail "a window is enumerated outside the capture backend (see docs/anti-cheat.md)"
fi

# The Windows feature groups. A `windows` / `windows-sys` feature is a *group* of APIs:
# enabling one makes every function in it reachable, whether or not the code calls it. So
# the manifests are the capability list, and every manifest in the repository is checked,
# not one crate's. (crates/events/tests/anti_cheat.rs pins the events crate's exact list;
# this covers all of them, and the binary guard proves what actually got linked.)
#
# Two kinds of group:
#   - forbidden everywhere: another process's memory, a device driver, accessibility hooks
#     and raw/injected input. Nothing in this project needs one.
#   - owned: a group that is legitimate in exactly the crate(s) named, and a violation in
#     any other. Adding a crate to an owner list is a reviewed change to this file.
#
# `Win32_System_Threading` in capture is the argued entry rather than the obvious one: the
# group also contains the call that opens another process, and capture needs it for event
# handles and thread priority only. The binary guard is what shows the call is not linked.
feature_owners() {
  case "$1" in
    Win32_System_Memory | Win32_System_Diagnostics_Debug | Win32_System_Ioctl | \
      Win32_UI_Accessibility | Win32_UI_Input | Win32_UI_Input_Pointer | \
      Win32_UI_Input_Touch) echo "-" ;;
    Win32_UI_Input_KeyboardAndMouse | Win32_System_Diagnostics_ToolHelp) echo "crates/events/Cargo.toml" ;;
    Win32_System_Threading) echo "crates/capture/Cargo.toml xtask/Cargo.toml" ;;
    Win32_Security) echo "crates/capture/Cargo.toml" ;;
    Win32_System_ProcessStatus) echo "xtask/Cargo.toml" ;;
    *) echo "*" ;;
  esac
}
feature_failures=""
for manifest in Cargo.toml crates/*/Cargo.toml xtask/Cargo.toml apps/desktop/src-tauri/Cargo.toml; do
  [ -f "$manifest" ] || continue
  for feature in $(grep -oE '"(Win32|Wdk)_[A-Za-z0-9_]+"' "$manifest" | tr -d '"' | sort -u); do
    owners=$(feature_owners "$feature")
    [ "$owners" = "*" ] && continue
    case " $owners " in
      *" $manifest "*) ;;
      *) feature_failures="$feature_failures
  $manifest enables $feature (allowed in: $owners)" ;;
    esac
  done
done
if [ -n "$feature_failures" ]; then
  echo "$feature_failures" >&2
  fail "a crate enables a Windows feature group it is not allowed (see docs/anti-cheat.md and feature_owners in this script)"
fi

# The positive half of the process-watcher claim: it enumerates the process list, which is
# what Task Manager does, and it never opens a process — no access right is requested over
# any game, and no memory is read.
if ! grep -qE 'CreateToolhelp32Snapshot' crates/events/src/process.rs; then
  fail "the process watcher no longer enumerates the process list (see docs/anti-cheat.md)"
fi
if grep -niE 'OpenProcess' crates/events/src/process.rs | grep -vE '^[0-9]+:\s*//'; then
  fail "the process watcher opens a process (see docs/anti-cheat.md)"
fi

echo "no forbidden APIs"
