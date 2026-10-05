# Working in this repo

- After every change, commit it and push it to the current branch (`git push -u origin <branch>` the first time). Don't wait to be asked.
- Work on a feature branch, never directly on `main`. Merge to `main` only when asked.
- Never touch, stage or commit `reference(dont commit)/`.
- Before committing, make sure it builds and the tests pass: `cargo build --workspace`, `cargo test --workspace -- --test-threads=4`, `node --test "extension/test/*.test.mjs"`.

## Releasing

The app updates itself from this repo's GitHub releases and installs only what `SHA256SUMS.sig` signs with the key at `%USERPROFILE%\.endo-release\update-key.pem` (its public half is `PUBLIC_KEY` in `crates/hyperfetch-core/src/updater.rs`).

1. Bump `version` in the workspace `Cargo.toml` and in `extension/manifest.json` to the same `X.Y.Z`.
2. `cargo build --release` (it updates `Cargo.lock` for the new version); commit the three files and push.
3. Put in one empty folder: `Endos-Unified-Downloader.exe` and `Endos-Unified-Downloader-CLI.exe` from `target\release`; `Endos-Unified-Downloader-Extension-vX.Y.Z.zip` (the files of `extension/` at the zip root, without `test/` and `package.json`: from the repo, `C:\Windows\System32\tar.exe -a -cf <folder>\Endos-Unified-Downloader-Extension-vX.Y.Z.zip -C extension --exclude test --exclude package.json .`; a zip without `manifest.json` at its root makes the update fail wherever the app has an `extension` folder); `Endos-Unified-Downloader-vX.Y.Z-windows-x64.zip` (both exes, `README.md` and `extension/` without `extension/test/`).
4. `node scripts/sign-release.mjs <folder> --version X.Y.Z` writes `SHA256SUMS` and `SHA256SUMS.sig` (it refuses a key that is not the one the app trusts).
5. `gh release create vX.Y.Z <folder>/*` with all six files.

Never commit the key or put it in the repo, and keep a backup of it offline: a lost key means users must download the next release by hand (a new key needs a new `PUBLIC_KEY`). `node scripts/sign-release.mjs --keygen` makes one only when none exists.
