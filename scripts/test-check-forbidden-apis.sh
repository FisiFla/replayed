#!/usr/bin/env bash
# Tests for scripts/check-forbidden-apis.sh. Each case builds a throwaway tree holding a copy
# of the guard plus one offending (or innocent) file, runs the guard in it, and checks the
# exit status. The guard `cd`s to the directory above itself, so a copy under a temp
# `scripts/` is scanned in isolation and the real tree is never touched.
#
# The case that matters most is `self_name_bypass`: an earlier guard filtered its matches with
# `grep -v 'check-forbidden-apis.sh'`, so any line mentioning the guard's name was dropped,
# including a line that called SendInput.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
failures=0

new_tree() {
  tree="$(mktemp -d)"
  mkdir -p "$tree/scripts" "$tree/crates/events/src" "$tree/crates/capture/src" "$tree/xtask"
  cp "$here/check-forbidden-apis.sh" "$tree/scripts/"
  # The positive half of the process-watcher claim looks for this call; give it one.
  echo 'fn snapshot() { CreateToolhelp32Snapshot(); }' > "$tree/crates/events/src/process.rs"
  printf '[package]\nname = "x"\n' > "$tree/crates/events/Cargo.toml"
  printf '[package]\nname = "x"\n' > "$tree/crates/capture/Cargo.toml"
  printf '[package]\nname = "x"\n' > "$tree/xtask/Cargo.toml"
}

# expect <name> <pass|fail>: run the guard in $tree.
expect() {
  local name="$1" want="$2" out status
  out="$(bash "$tree/scripts/check-forbidden-apis.sh" 2>&1)"
  status=$?
  if { [ "$want" = pass ] && [ "$status" -eq 0 ]; } || { [ "$want" = fail ] && [ "$status" -ne 0 ]; }; then
    echo "ok   $name"
  else
    echo "FAIL $name (wanted $want, exit $status)"
    echo "$out" | sed 's/^/     /'
    failures=$((failures + 1))
  fi
  rm -rf "$tree"
}

new_tree
expect "an innocent tree passes" pass

new_tree
echo '[Win32]::SendInput(1, $x, 28)  # check-forbidden-apis.sh' > "$tree/scripts/helper.ps1"
expect "self_name_bypass: a call that names the guard in a comment is still caught" fail

new_tree
echo '[Win32]::SendInput(1, $x, 28)' > "$tree/scripts/check-forbidden-apis.sh.ps1"
expect "a file whose name merely contains the guard's name is still scanned" fail

new_tree
mkdir -p "$tree/xtask/scripts"
echo 'sendinput(1, x, 28)' > "$tree/xtask/scripts/check-forbidden-apis.sh"
expect "a same-named file outside scripts/ is scanned like any other" fail

new_tree
echo '[Win32]::sendinput(1, $x, 28)' > "$tree/scripts/lower.ps1"
expect "matching is case-insensitive" fail

new_tree
echo '[void][System.Windows.Forms.SendKeys]::SendWait("a")' > "$tree/scripts/keys.ps1"
expect "SendKeys in a script is caught" fail

new_tree
echo 'unsafe { SetWindowsHookExW(0, None, None, 0) };' > "$tree/crates/capture/src/lib.rs"
expect "a hook in Rust is caught" fail

new_tree
echo '    unsafe { SendInput(&[], 0) }; // not a comment line' > "$tree/crates/capture/src/lib.rs"
expect "a trailing comment does not hide a call" fail

new_tree
echo '// SendInput is what this crate must never call' > "$tree/crates/capture/src/lib.rs"
expect "a whole-line comment about a forbidden API is allowed" pass

new_tree
printf '[dependencies]\nwindows = { version = "0.62", features = ["Win32_System_Memory"] }\n' > "$tree/crates/capture/Cargo.toml"
expect "a forbidden Windows feature group in any crate is caught" fail

new_tree
printf '[dependencies]\nwindows = { version = "0.62", features = ["Win32_UI_Input_KeyboardAndMouse"] }\n' > "$tree/crates/capture/Cargo.toml"
expect "a feature group owned by another crate is caught" fail

new_tree
printf '[dependencies]\nwindows = { version = "0.62", features = ["Win32_UI_Input_KeyboardAndMouse"] }\n' > "$tree/crates/events/Cargo.toml"
expect "the owning crate may enable its group" pass

new_tree
echo 'OpenProcess(0, 0, pid)' >> "$tree/crates/events/src/process.rs"
expect "the process watcher opening a process is caught" fail

new_tree
echo 'EnumWindows(cb, 0)' > "$tree/crates/capture/src/lib.rs"
expect "window enumeration outside the capture backend is caught" fail

# tao's own `set_focus` sends a synthetic Alt through SendInput, so the call is input synthesis
# even though no Win32 name is spelled at the call site.
new_tree
echo 'fn raise(w: &Window) { w.set_focus(); }' > "$tree/crates/capture/src/lib.rs"
expect "a .set_focus( call is caught" fail

new_tree
echo '    window . set_focus ()' > "$tree/crates/capture/src/lib.rs"
expect "a .set_focus( call spelled with spaces is caught" fail

new_tree
echo '// never call .set_focus() here: tao synthesises an Alt key press' > "$tree/crates/capture/src/lib.rs"
expect "a whole-line comment about .set_focus( is allowed" pass

# The vendored crates (patched tao and muda) are scanned, not excluded: a re-vendor without the
# patch has to fail the build.
new_tree
mkdir -p "$tree/apps/desktop/src-tauri/vendor/tao/src/platform_impl/windows"
echo '  SendInput(&inputs, mem::size_of::<INPUT>() as _);' > "$tree/apps/desktop/src-tauri/vendor/tao/src/platform_impl/windows/window.rs"
expect "vendored_sendinput: SendInput in vendored tao is caught" fail

new_tree
mkdir -p "$tree/apps/desktop/src-tauri/vendor/muda/src"
echo '            SendInput(4, &inputs as *const _, 28);' > "$tree/apps/desktop/src-tauri/vendor/muda/src/lib.rs"
expect "SendInput in vendored muda is caught" fail

new_tree
mkdir -p "$tree/vendor/tao/src"
echo 'unsafe { keybd_event(0, 0, 0, 0) };' > "$tree/vendor/tao/src/lib.rs"
expect "a top-level vendor/ directory is scanned too" fail

new_tree
mkdir -p "$tree/apps/desktop/src-tauri/vendor/tao/src"
echo '    unsafe { SENDINPUT(&inputs) }; // trailing comment' > "$tree/apps/desktop/src-tauri/vendor/tao/src/lib.rs"
expect "vendored matching is case-insensitive and a trailing comment does not hide it" fail

new_tree
mkdir -p "$tree/apps/desktop/src-tauri/vendor/tao/src"
echo '// replayed: upstream called SendInput here; the call is gone.' > "$tree/apps/desktop/src-tauri/vendor/tao/src/lib.rs"
echo 'fn x() { PostMessageW(h, m, w, l); SendMessageW(h, m, w, l); GetKeyState(1); self.window.set_focus(); util::set_ignore_mouse_events(w, true); }' >> "$tree/apps/desktop/src-tauri/vendor/tao/src/lib.rs"
expect "vendored toolkit calls that are not on the binary guard's list pass" pass

new_tree
mkdir -p "$tree/crates/capture/src/vendor"
echo 'PostMessageW(h, m, w, l);' > "$tree/crates/capture/src/vendor/lib.rs"
expect "a directory merely named vendor elsewhere is our code and gets the full list" fail

new_tree
bash "$tree/scripts/check-forbidden-apis.sh" --nonsense >/dev/null 2>&1
if [ $? -ne 0 ]; then echo "ok   an unknown argument is refused"; else echo "FAIL an unknown argument is refused"; failures=$((failures + 1)); fi
rm -rf "$tree"

if [ "$failures" -ne 0 ]; then
  echo "$failures case(s) failed" >&2
  exit 1
fi
echo "all cases passed"
