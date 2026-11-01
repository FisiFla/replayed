# Vendored crates

Two third-party crates live here as patched copies, wired in with `[patch.crates-io]` in
`../Cargo.toml`. Nothing else is vendored.

| Directory | Upstream | Patch (applied with `patch -p1`) |
| --- | --- | --- |
| `tao/`  | `tao 0.37.1` from crates.io  | `scripts/patches/tao-0.37.1-no-sendinput.patch`   |
| `muda/` | `muda 0.20.0` from crates.io | `scripts/patches/muda-0.20.0-no-sendinput.patch`  |

## Why

`docs/anti-cheat.md` promises that the shipped binary synthesises no input: no `SendInput`, no
`keybd_event`. The shell never calls such a thing, but two of its dependencies contain one, and
the Windows linker keeps the import whether or not anything reaches it. `scripts/check-pe-imports.py`
found `SendInput` in the import table of `replayed-desktop.exe`, through:

- **tao** `Window::set_focus` -> `force_window_active`. When `SetForegroundWindow` is refused,
  tao presses and releases a synthetic Alt key to steal the foreground permission. The patch
  removes that fallback: `set_focus` (and the one internal caller, creating a fullscreen window)
  asks `SetForegroundWindow` and stops there. A window the system will not raise stays where it
  is. The shell does not call `set_focus` at all; it raises its window through
  `src/window_focus.rs` (`SetForegroundWindow`, then `FlashWindowEx`).
- **muda** `EditCommand::run`. The predefined Copy / Cut / Paste / Select All / Undo / Redo menu
  items work by sending Ctrl+C / X / V / A / Z / Y. The patch makes `run` a no-op. The shell's
  menus (the tray menu in `src/lib.rs`) use none of those items, so no behaviour changes; a
  future predefined edit item would do nothing rather than type.

Both patches degrade to a no-op or a plain `SetForegroundWindow`; neither replaces the input
with another way of injecting it.

Newer releases do not help (checked when this was vendored): `tao 0.37.2` and `muda 0.21.2`
still contain the same `SendInput` calls.

## What stops it regressing

- `scripts/check-forbidden-apis.sh` scans this directory (it is *not* excluded) for the names the
  binary guard forbids, `SendInput` first among them, so re-vendoring without the patch fails
  CI. It also bans the `.set_focus(` call in our own code.
- The binary guard (`scripts/check-forbidden-apis.sh --binary <exe>`) checks the import table of
  what is actually built, with an empty allowlist. This is the check that notices a `[patch]`
  that silently stopped applying (cargo only warns "patch was not used" when the locked version
  no longer matches the patched one).
- `.github/dependabot.yml` ignores `tao` and `muda`. Nothing bumps them automatically.

## Layout of the copies

Each directory is the crates.io tarball as published (normalised `Cargo.toml`, `Cargo.toml.orig`,
`LICENSE*`, `README.md`, `src/`, `examples/` for tao), with exactly these differences:

- the patch above applied (the only edited files are `tao/src/platform_impl/windows/window.rs`
  and `muda/src/platform_impl/windows/mod.rs`);
- the crate's own `Cargo.lock` and the `.cargo-ok` marker removed: neither is used when the crate
  is a dependency.

The licences are kept: `tao/LICENSE` (Apache-2.0), `muda/LICENSE-APACHE` and `muda/LICENSE-MIT`
(Apache-2.0 OR MIT), plus the `LICENSE.spdx` files.

## Re-vendoring on an upgrade

Do this when `tauri` / `tauri-runtime-wry` needs a newer `tao`, or `muda` has to move, or an
advisory lands on either crate (Dependabot will not tell you).

1. Bump the dependency that pulls the new version in, with the patches out of the way:
   comment out `[patch.crates-io]` in `apps/desktop/src-tauri/Cargo.toml`, then
   `cargo update -p tauri` (or whichever crate) and note the new `tao` / `muda` versions in
   `Cargo.lock`.
2. Replace the copies with the pristine sources of exactly those versions, from
   `~/.cargo/registry/src/*/tao-<ver>` (or `cargo download` / `cargo vendor` into a scratch
   directory):
   ```sh
   cd apps/desktop/src-tauri
   rm -rf vendor/tao vendor/muda
   cp -R ~/.cargo/registry/src/*/tao-<ver>  vendor/tao
   cp -R ~/.cargo/registry/src/*/muda-<ver> vendor/muda
   rm -f vendor/*/Cargo.lock vendor/*/.cargo-ok
   ```
3. Re-apply the patches. If a hunk no longer applies, port it by hand and **regenerate the patch
   file** (`diff -u` between the pristine tree and the vendored one, paths `a/` and `b/`), with the
   new version in its name, so `scripts/patches/` stays the record of what differs from upstream:
   ```sh
   (cd vendor/tao  && patch -p1 < ../../../../../scripts/patches/tao-<ver>-no-sendinput.patch)
   (cd vendor/muda && patch -p1 < ../../../../../scripts/patches/muda-<ver>-no-sendinput.patch)
   ```
   Then `grep -rni sendinput vendor/` must show only the `// replayed:` comments. Also look for
   *new* synthesis in the release notes and diff (`keybd_event`, `mouse_event`, `SendInput`
   under another name): the patches remove the two known call sites, not the category.
4. Restore the `[patch.crates-io]` section, update the version numbers in this file, and check:
   - `cargo metadata --locked` (the lockfile still resolves; the only `Cargo.lock` change
     for these two packages should be that `source` and `checksum` are gone, as they are for
     any path package, and nothing else drifts);
   - `bash scripts/check-forbidden-apis.sh && bash scripts/test-check-forbidden-apis.sh`;
   - on Windows: `cargo build --release --locked`, then
     `python scripts/check-pe-imports.py target/release/replayed-desktop.exe` must pass with an
     empty `scripts/pe-imports-allowlist.txt`.
5. Delete the old patch files from `scripts/patches/` once the new ones are in.

When upstream finally removes `SendInput` (or makes it opt-out), delete the vendored copy, the
`[patch.crates-io]` section, the Dependabot `ignore` entries and this directory, and let the
binary guard confirm it.
