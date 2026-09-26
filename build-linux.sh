#!/usr/bin/env bash
# ==============================================================================
# Endo's Unified Downloader - Linux build script (no install; see install.sh)
#   ./build-linux.sh          CLI, plus the GUI when a display is present
#   ./build-linux.sh --gui    CLI and GUI
# ==============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "=== Building Endo's Unified Downloader for Linux ($(rustc --version)) ==="

cargo build --release --locked -p hyperfetch-cli
echo "CLI: target/release/Endos-Unified-Downloader-CLI"

if [ "${1:-}" = "--gui" ] || [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; then
    if cargo build --release --locked -p hyperfetch-gui; then
        echo "GUI: target/release/Endos-Unified-Downloader"
    else
        echo "GUI build failed; the CLI binary is ready." >&2
    fi
fi
