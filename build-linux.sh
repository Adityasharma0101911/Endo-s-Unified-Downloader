#!/usr/bin/env bash
# ==============================================================================
# Endo's Unified Downloader - Linux Build Script
# ==============================================================================

set -e

echo "=== Building Endo's Unified Downloader for Linux ==="
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Build headless CLI binary (works on any Linux server)
echo "Building CLI binary..."
cargo build --release -p hyperfetch-cli

echo "CLI binary built at: target/release/Endos-Unified-Downloader-CLI"

# Check if GUI dependencies are available before attempting GUI build
if [ "$1" = "--gui" ] || [ -n "$DISPLAY" ] || [ -n "$WAYLAND_DISPLAY" ]; then
    echo "Attempting to build Native GUI binary..."
    if cargo build --release -p hyperfetch-gui; then
        echo "GUI binary built at: target/release/Endos-Unified-Downloader"
    else
        echo "GUI build failed or dependencies missing. CLI binary remains ready for server use."
    fi
fi

echo "=== Build finished successfully ==="
