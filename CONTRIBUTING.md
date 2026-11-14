# Contributing

## Build and test
```
cargo fmt --check
cargo test --workspace --features replayed-encoder/test-encoders,replayed-cli/test-encoders   # needs ffmpeg on PATH
cargo clippy --workspace --all-targets --features replayed-encoder/test-encoders,replayed-cli/test-encoders -- -A clippy::unwrap_used -A clippy::chunks_exact_to_as_chunks -D warnings
cd apps/desktop && npm ci && npm run check && npm test && npm run build
cd apps/desktop/src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test   # separate workspace
scripts/check-forbidden-apis.sh                                      # anti-cheat guard (source)
python3 -I -m unittest discover -s scripts -p 'test_*.py'            # tests of the guards and release scripts
bash scripts/test-check-forbidden-apis.sh
```
On macOS the capture path is a synthetic stub: green tests prove nothing about Windows capture.
Read `docs/verification-status.md` for what has actually run on Windows hardware.

CI (`.github/workflows/ci.yml`) is the reference for the exact commands, and every cargo command in it
is `--locked`. Details that have caught people out:

- **The Windows cross-check needs the test feature.** From macOS, lint the Windows-only code with
  `cargo clippy --locked -p replayed-capture -p replayed-encoder -p replayed-events -p replayed-replay -p replayed-media -p xtask --features replayed-encoder/test-encoders --all-targets --target x86_64-pc-windows-msvc -- -D warnings`.
  Without `--features replayed-encoder/test-encoders` the integration tests do not compile. The
  store, recorder, CLI and desktop shell cannot be cross-checked from macOS (bundled SQLite needs
  MSVC); the `windows` CI job lints and tests them.
- **`apps/desktop/src-tauri` needs `apps/desktop/dist` to exist** before `cargo test` (Tauri embeds
  it). `npm run build` creates it; an empty directory is enough for the Rust tests.
- **Frontend tests** sit beside their component as `*.test.ts`. A test that has to hold its props
  in Svelte `$state` (to replay a parent updating them) must be named `*.svelte.test.ts` so the
  runes compile; see `EncoderPanel.shared.svelte.test.ts`.
- **`tao` and `muda` are vendored** in `apps/desktop/src-tauri/vendor/` with the code that calls
  `SendInput` removed. A Tauri upgrade that needs a different `tao` means re-vendoring: follow
  `apps/desktop/src-tauri/vendor/README.md`. The guard scans that directory too.
- **Cutting a release** is a tag; read `docs/packaging.md` §5.10 first. Before tagging, run
  `python3 -I scripts/check-release-version.py --tag vX.Y.Z`: the version is written in six places and
  `CHANGELOG.md` needs a `## [X.Y.Z]` section.

## Rules
- The five principles in the README are constraints, not preferences: no cloud, no telemetry, no
  Overwolf, minimal resources, lossless trims.
- Never add APIs that touch another process or synthesise input (`docs/anti-cheat.md`). The guard
  checks our source and the vendored crates, and in CI it also reads the import table of the built
  executable, so a dependency that pulls in `SendInput` fails the build. Do not call `set_focus` on
  a window; use `apps/desktop/src-tauri/src/window_focus.rs`.
- Windows-only code must at least pass the cross-check above (CI does this).
- Security issues: see `SECURITY.md`; do not file them publicly.
- PowerShell eats `--`; call the Tauri CLI binary directly when bundling (`docs/packaging.md`).
- When behaviour or a default changes, change `config.example.toml` and the docs in the same
  commit: a test loads the example file, so a comment that contradicts it is usually also a bug.
