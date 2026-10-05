# Working in this repo

- After every change, commit it and push it to the current branch (`git push -u origin <branch>` the first time). Don't wait to be asked.
- Work on a feature branch, never directly on `main`. Merge to `main` only when asked.
- Never touch, stage or commit `reference(dont commit)/`.
- Before committing, make sure it builds and the tests pass: `cargo build --workspace`, `cargo test --workspace -- --test-threads=4`, `node --test "extension/test/*.test.mjs"`.

## Testing the GUI

- UI checks run headless inside `cargo test -p hyperfetch-gui`, through `crates/hyperfetch-gui/src/ui/harness.rs`: it drives the real `ui::render` with synthetic input (clicks on visible text, keys, typing, paste, files dragged over) and a hand-stepped clock, asserts the repaints each frame asks for, and paints frames to PNG with a CPU rasterizer. No window opens.
- To look at the animations: `cargo test -p hyperfetch-gui capture_gallery -- --ignored` writes frame strips of each one, dark and light, and every page at 100/125/150 %, to the folder in `ENDO_CAPTURE_DIR` (default `target/ui-captures`).
- Never move the mouse, send keys, focus windows or capture the screen to test the app: the user works on this PC. If a real run is needed (an idle CPU check), start the release build with `Start-Process -WindowStyle Minimized`, `ENDO_HISTORY_PATH` (its settings and queue live next to it) and `TEMP`/`TMP` pointing into a scratch folder, and close it with `CloseMainWindow()`.

## Releasing

The app updates itself from this repo's GitHub releases and installs only what `SHA256SUMS.sig` signs with the key at `%USERPROFILE%\.endo-release\update-key.pem` (its public half is `PUBLIC_KEY` in `crates/hyperfetch-core/src/updater.rs`).

1. Bump `version` in the workspace `Cargo.toml` and in `extension/manifest.json` to the same `X.Y.Z`.
2. `cargo build --release` (it updates `Cargo.lock` for the new version); commit the three files and push them to a `release/X.Y.Z` branch too: on it the macOS workflow (`.github/workflows/macos.yml`) tests on an Apple Silicon and an Intel Mac, then its `package` job builds, ad-hoc signs and checks the app for each (`packaging/macos/bundle.sh`). `gh run watch` it; it must be green.
3. Put in one empty folder: `Endos-Unified-Downloader.exe` and `Endos-Unified-Downloader-CLI.exe` from `target\release`; `Endos-Unified-Downloader-Extension-vX.Y.Z.zip` (the files of `extension/` at the zip root, without `test/` and `package.json`: from the repo, `C:\Windows\System32\tar.exe -a -cf <folder>\Endos-Unified-Downloader-Extension-vX.Y.Z.zip -C extension --exclude test --exclude package.json .`; a zip without `manifest.json` at its root makes the update fail wherever the app has an `extension` folder); `Endos-Unified-Downloader-vX.Y.Z-windows-x64.zip` (both exes, `README.md` and `extension/` without `extension/test/`); and the four macOS files: `gh run download <run id> -n macos-arm64 -D <folder>`, then the same with `-n macos-x64` (one name per call, or gh puts each in a subfolder), put `Endos-Unified-Downloader-vX.Y.Z-macos-apple-silicon.dmg`, `Endos-Unified-Downloader-vX.Y.Z-macos-intel.dmg`, `Endos-Unified-Downloader-macos-arm64.app.tar.gz` and `Endos-Unified-Downloader-macos-x64.app.tar.gz` there (the updater on a Mac installs the `.app.tar.gz` of its architecture; the names must not change). Check the folder holds those eight files and nothing else.
4. `node scripts/sign-release.mjs <folder> --version X.Y.Z` writes `SHA256SUMS` and `SHA256SUMS.sig` over every file in the folder (it refuses a key that is not the one the app trusts).
5. `gh release create vX.Y.Z <folder>/*` with all ten files. Signing stays on this PC: the key never goes to GitHub Actions.

Never commit the key or put it in the repo, and keep a backup of it offline: a lost key means users must download the next release by hand (a new key needs a new `PUBLIC_KEY`). `node scripts/sign-release.mjs --keygen` makes one only when none exists.
