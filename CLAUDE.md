# Working in this repo

- After every change, commit it and push it to the current branch (`git push -u origin <branch>` the first time). Don't wait to be asked.
- Work on a feature branch, never directly on `main`. Merge to `main` only when asked.
- Never touch, stage or commit `reference(dont commit)/`.
- Before committing, make sure it builds and the tests pass: `cargo build --workspace`, `cargo test --workspace -- --test-threads=4`, `node --test "extension/test/*.test.mjs"`.

## Testing the GUI

- UI checks run headless inside `cargo test -p hyperfetch-gui`, through `crates/hyperfetch-gui/src/ui/harness.rs`: it drives the real `ui::render` with synthetic input (clicks on visible text, keys, typing, paste, files dragged over) and a hand-stepped clock, asserts the repaints each frame asks for, and paints frames to PNG with a CPU rasterizer. No window opens.
- To look at the animations: `cargo test -p hyperfetch-gui capture_gallery -- --ignored` writes frame strips of each one, dark and light, and every page at 100/125/150 %, to the folder in `ENDO_CAPTURE_DIR` (default `target/ui-captures`).
- Never move the mouse, send keys, focus windows or capture the screen to test the app: the user works on this PC. If a real run is needed (an idle CPU check), start the release build with `Start-Process -WindowStyle Minimized`, `ENDO_HISTORY_PATH` (its settings and queue live next to it) and `TEMP`/`TMP` pointing into a scratch folder, and close it with `CloseMainWindow()`.

## Keep the PC usable: CPU, memory and disk

The user works on this PC (32 threads, 32 GB RAM) while you build and test, so builds and tests must leave it usable. A past session filled the disk with a single 200 GB file. Don't let that happen again.

- Set `CARGO_BUILD_JOBS=8` on every cargo command, and keep `--test-threads=4`.
- Run cargo, node and headless Edge at below-normal priority, so games and the user's apps come first. In Git Bash that's `nice -n 10 cargo ...`; the compiler processes inherit the priority.
- Run one cargo build or test at a time, across every agent and subagent. Parallel agents take turns on the one `target` folder and never build side by side.
- Don't use separate git worktrees (or `isolation: "worktree"`) for Rust work: each one builds its own `target` of several GB.
- Run the full suite before a commit, not after every edit. While working, run only the tests of the crate or module you touched (`cargo test -p hyperfetch-core <name>`).
- Run `cargo build --release`, `bench/` and the capture gallery only when releasing, when asked, or for a real run. Never run them in a loop.
- Run at most one headless browser, and close it when its check is done.
- Before finishing, stop everything you started: the app, test servers, headless Edge, `yt-dlp`, `ffmpeg`, cargo runs in the background. Check with `Get-Process` that none of them is left.
- No test, benchmark or real run may write more than 1 GB. Test files are KB to a few MB. A test that needs a large size fakes it in the test server's headers and never writes the body.
- Every test server sends a body of fixed length, then ends: never an endless stream. Every download, recording or capture in a test or a real run has a size or time limit (`max_filesize`, a timeout). Every test that waits has a timeout, so a hung test can't keep writing.
- Throwaway files go in the session scratchpad. That covers real-run downloads, `ENDO_CAPTURE_DIR`, `gh run download` artifacts and cloned repos. Tests write only to temp dirs that delete themselves. Never write into the user's Downloads folder or the repo.
- Keep one `target` folder: no `CARGO_TARGET_DIR` elsewhere. Delete `target/ui-captures` once you've looked at the captures.
- At the end, check the size of the scratchpad and of `target`, and the free space on C:. Delete what you made. If C: lost more than 5 GB, find out why before you stop.

## Releasing

The app updates itself from this repo's GitHub releases and installs only what `SHA256SUMS.sig` signs with the key at `%USERPROFILE%\.endo-release\update-key.pem` (its public half is `PUBLIC_KEY` in `crates/hyperfetch-core/src/updater.rs`).

1. Bump `version` in the workspace `Cargo.toml` and in `extension/manifest.json` to the same `X.Y.Z`.
2. `cargo update --workspace` (it updates `Cargo.lock` for the new version); commit the three files and push them to a `release/X.Y.Z` branch too. On it the macOS workflow (`.github/workflows/macos.yml`) tests on an Apple Silicon and an Intel Mac, then its `package` job builds, ad-hoc signs and checks the app for each (`packaging/macos/bundle.sh`); the Packages workflow (`.github/workflows/packages.yml`) builds and checks the Windows programs, zips and installer (`packaging/windows/installer.iss`; code-signed when the signing secrets are set, see below), fills in the winget manifests (`packaging/winget/`), and builds the Linux x64 and arm64 tar.gz, .deb and .rpm (`packaging/linux/package.sh`) and the static x64 CLI. `gh run watch` both; they must be green. (CI, `.github/workflows/ci.yml`, runs the three test suites on Windows and Linux on every push.)
3. Put in one empty folder, with `gh run download <run id> -n <artifact> -D <folder>` (one name per call, or gh puts each in a subfolder):
   - from the Packages run, `-n windows-x64`: `Endos-Unified-Downloader.exe` and `Endos-Unified-Downloader-CLI.exe` (what the updater installs on Windows), `Endos-Unified-Downloader-vX.Y.Z-windows-x64.zip` (both exes, `README.md` and `extension/`), `Endos-Unified-Downloader-vX.Y.Z-windows-x64-setup.exe` and `Endos-Unified-Downloader-Extension-vX.Y.Z.zip` (what the updater unpacks next to the programs; CI checks `manifest.json` is at its root). The Windows files come only from CI, never from `target\release`: they are the signed ones and the ones in the installer.
   - from the Packages run, `-n linux-x64`: `Endos-Unified-Downloader-vX.Y.Z-linux-x64.tar.gz`, `endos-unified-downloader_X.Y.Z_amd64.deb` and `endos-unified-downloader-X.Y.Z-1.x86_64.rpm`; `-n linux-arm64`: `Endos-Unified-Downloader-vX.Y.Z-linux-arm64.tar.gz`, `endos-unified-downloader_X.Y.Z_arm64.deb` and `endos-unified-downloader-X.Y.Z-1.aarch64.rpm` (`install.sh` installs the tar.gz of the machine's architecture); `-n linux-x64-static`: `Endos-Unified-Downloader-CLI-vX.Y.Z-linux-x64-static.tar.gz`.
   - from the macOS run, `-n macos-arm64` and `-n macos-x64`: `Endos-Unified-Downloader-vX.Y.Z-macos-apple-silicon.dmg`, `Endos-Unified-Downloader-vX.Y.Z-macos-intel.dmg`, `Endos-Unified-Downloader-macos-arm64.app.tar.gz` and `Endos-Unified-Downloader-macos-x64.app.tar.gz` (the updater on a Mac installs the `.app.tar.gz` of its architecture).
   - `node scripts/sign-firefox.mjs <folder> --version X.Y.Z`, with Mozilla's add-on API key in `AMO_JWT_ISSUER` and `AMO_JWT_SECRET` (never commit or print them), writes `Endos-Unified-Downloader-Firefox-vX.Y.Z.xpi` and `firefox-updates.json` (Firefox installs the add-on from the release and updates it through the latest release's `firefox-updates.json`).

   Check the folder holds those eighteen files and nothing else. None of the names may change: the updater, `install.sh` and Firefox look for them.
4. `node scripts/sign-release.mjs <folder> --version X.Y.Z` writes `SHA256SUMS` and `SHA256SUMS.sig` over every file in the folder (it refuses a key that is not the one the app trusts).
5. `gh release create vX.Y.Z <folder>/*` with all twenty files. Signing stays on this PC: the key never goes to GitHub Actions.
6. winget, once the release is public: `gh run download <run id> -n winget -D <another folder>` gives this version's three manifests (`Adityasharma0101911.EndosUnifiedDownloader`, pointing at the release's setup.exe). Submitting them to winget (`wingetcreate submit <that folder>`, or a pull request to microsoft/winget-pkgs under `manifests/a/Adityasharma0101911/EndosUnifiedDownloader/X.Y.Z/`) is the maintainer's own step: an agent never submits anything to winget.

Never commit the key or put it in the repo, and keep a backup of it offline: a lost key means users must download the next release by hand (a new key needs a new `PUBLIC_KEY`). `node scripts/sign-release.mjs --keygen` makes one only when none exists.

Windows code signing (optional): the Packages workflow signs both programs with Azure Trusted Signing before it packs them, then the setup.exe, when the repository has all six secrets `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` (an app registration with the Trusted Signing Certificate Profile Signer role on the signing account), `SIGNING_ENDPOINT` (the account's regional endpoint, such as `https://eus.codesigning.azure.net/`), `SIGNING_ACCOUNT` and `SIGNING_PROFILE` (the certificate profile). Without them the step is skipped and the files are unsigned. This signature is only for Windows (SmartScreen); it is not the updater's, whose key stays on this PC.
