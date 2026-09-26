#!/usr/bin/env bash
# ==============================================================================
# Endo's Unified Downloader - Linux installer
# Debian/Ubuntu, Fedora/RHEL, Arch, openSUSE and Alpine.
#
#   ./install.sh              build and install the CLI (plus the GUI when a display is present)
#   ./install.sh --gui        also build and install the GUI
#   ./install.sh --no-gui     CLI only
#   ./install.sh --service    also install the systemd batch-queue service
#   ./install.sh --uninstall  remove what the installer added (downloads and history are kept)
#
# Run it as your normal user; it uses sudo only for the system-wide install step.
# ==============================================================================

set -euo pipefail

PREFIX=/usr/local
BIN_DIR="$PREFIX/bin"
CLI_DEST="$BIN_DIR/endos-downloader-cli"
GUI_DEST="$BIN_DIR/endos-downloader-gui"
DESKTOP_FILE=/usr/share/applications/endos-downloader.desktop
ICON_FILE="$PREFIX/share/icons/hicolor/scalable/apps/endos-downloader.svg"
SERVICE_FILE=/etc/systemd/system/endos-downloader.service
SERVICE_USER=endos-downloader
DOWNLOAD_DIR=/var/downloads
MIN_RUST_MINOR=89 # Rust 1.89 or newer
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

CYAN='\033[0;36m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
RESET='\033[0m'

step() { echo -e "\n${YELLOW}$*${RESET}"; }
info() { echo -e "${CYAN}$*${RESET}"; }
warn() { echo -e "${YELLOW}[WARN] $*${RESET}" >&2; }
die() {
    echo -e "${RED}[ERROR] $*${RESET}" >&2
    exit 1
}

usage() {
    sed -n '3,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

# --- Options ------------------------------------------------------------------
INSTALL_GUI=auto
INSTALL_SERVICE=false
UNINSTALL=false
for arg in "$@"; do
    case "$arg" in
        --gui) INSTALL_GUI=true ;;
        --no-gui) INSTALL_GUI=false ;;
        --service) INSTALL_SERVICE=true ;;
        --uninstall) UNINSTALL=true ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "Unknown option: $arg (see --help)" ;;
    esac
done
if [ "$INSTALL_GUI" = auto ]; then
    if [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; then INSTALL_GUI=true; else INSTALL_GUI=false; fi
fi

# --- Privileges -----------------------------------------------------------------
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || die "sudo is not installed; run this script as root instead."
    SUDO=sudo
elif [ -n "${SUDO_USER:-}" ] && [ "$UNINSTALL" = false ]; then
    # Building under sudo would put rustup in /root and leave a root-owned target/ in the checkout.
    die "Run ./install.sh as your normal user (not with sudo); it asks for sudo when it installs."
fi

# Copies a file into place atomically, so a running binary can be replaced.
install_file() {
    local mode="$1" src="$2" dest="$3" tmp
    tmp="$dest.new.$$"
    $SUDO mkdir -p "$(dirname "$dest")"
    $SUDO cp "$src" "$tmp"
    $SUDO chmod "$mode" "$tmp"
    $SUDO mv -f "$tmp" "$dest"
}

# Links $BIN_DIR/<name> to the CLI unless that name belongs to something else.
link_alias() {
    local link="$BIN_DIR/$1"
    if [ -e "$link" ] && [ "$(readlink "$link" || true)" != "$CLI_DEST" ]; then
        warn "$link already exists and is not ours; leaving it alone."
    else
        $SUDO ln -sfn "$CLI_DEST" "$link"
    fi
}

remove_alias() {
    local link="$BIN_DIR/$1"
    if [ -L "$link" ] && [ "$(readlink "$link")" = "$CLI_DEST" ]; then $SUDO rm -f "$link"; fi
}

has_systemd() { command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; }

# --- Uninstall ------------------------------------------------------------------
uninstall() {
    step "Removing Endo's Unified Downloader..."
    if [ -f "$SERVICE_FILE" ]; then
        if has_systemd; then $SUDO systemctl disable --now endos-downloader.service || true; fi
        $SUDO rm -f "$SERVICE_FILE"
        if has_systemd; then $SUDO systemctl daemon-reload; fi
    fi
    remove_alias endos-downloader
    remove_alias hyperfetch
    $SUDO rm -f "$CLI_DEST" "$GUI_DEST" "$DESKTOP_FILE" "$ICON_FILE"
    echo -e "${GREEN}Removed. Downloads, $DOWNLOAD_DIR and download history were kept.${RESET}"
}

# --- 1. System packages ---------------------------------------------------------
# The CLI needs only a C toolchain (TLS is rustls, no OpenSSL). The GUI needs no extra build
# packages; at runtime it uses the desktop's OpenGL and xkbcommon libraries.
install_system_deps() {
    step "[1/4] Installing build dependencies..."
    if command -v apt-get >/dev/null 2>&1; then
        $SUDO apt-get update -qq
        $SUDO apt-get install -y -qq build-essential pkg-config curl ca-certificates
    elif command -v dnf >/dev/null 2>&1; then
        $SUDO dnf install -y -q gcc gcc-c++ make pkgconf-pkg-config curl ca-certificates
    elif command -v pacman >/dev/null 2>&1; then
        $SUDO pacman -S --needed --noconfirm base-devel curl ca-certificates
    elif command -v zypper >/dev/null 2>&1; then
        $SUDO zypper --non-interactive install gcc gcc-c++ make pkg-config curl ca-certificates
    elif command -v apk >/dev/null 2>&1; then
        $SUDO apk add --no-cache build-base pkgconf curl ca-certificates
    else
        warn "Unknown package manager: make sure a C compiler, make and curl are installed."
    fi
}

# --- 2. Rust toolchain ----------------------------------------------------------
rust_is_recent() {
    local version major minor
    version="$(rustc --version 2>/dev/null | awk '{print $2}')" || return 1
    IFS=. read -r major minor _ <<<"$version"
    [ "${major:-0}" -gt 1 ] || { [ "${major:-0}" -eq 1 ] && [ "${minor:-0}" -ge "$MIN_RUST_MINOR" ]; }
}

install_rust() {
    step "[2/4] Checking the Rust toolchain (1.$MIN_RUST_MINOR or newer)..."
    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck source=/dev/null
        . "$HOME/.cargo/env"
    fi
    if ! command -v cargo >/dev/null 2>&1; then
        info "Rust not found; installing it with rustup (for this user only)..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
        # shellcheck source=/dev/null
        . "$HOME/.cargo/env"
    fi
    if ! rust_is_recent; then
        if command -v rustup >/dev/null 2>&1; then
            info "Updating Rust with rustup..."
            rustup update stable
            rustup default stable
        fi
        rust_is_recent || die "$(rustc --version) is too old. Install a current toolchain with rustup: https://rustup.rs"
    fi
    info "Using $(rustc --version)"
}

# --- 3. Build -------------------------------------------------------------------
build_binaries() {
    step "[3/4] Building release binaries..."
    cd "$SCRIPT_DIR"
    cargo build --release --locked -p hyperfetch-cli
    if [ "$INSTALL_GUI" = true ]; then
        cargo build --release --locked -p hyperfetch-gui || {
            warn "The GUI failed to build; installing the CLI only."
            INSTALL_GUI=false
        }
    fi
}

# --- 4. Install -----------------------------------------------------------------
install_binaries() {
    step "[4/4] Installing into $BIN_DIR..."
    install_file 755 "$SCRIPT_DIR/target/release/Endos-Unified-Downloader-CLI" "$CLI_DEST"
    link_alias endos-downloader
    link_alias hyperfetch

    if [ "$INSTALL_GUI" = true ]; then
        install_file 755 "$SCRIPT_DIR/target/release/Endos-Unified-Downloader" "$GUI_DEST"
        install_file 644 "$SCRIPT_DIR/assets/logo.svg" "$ICON_FILE"
        local desktop
        desktop="$(mktemp)"
        cat >"$desktop" <<EOF
[Desktop Entry]
Name=Endo's Unified Downloader
Comment=Multi-connection download accelerator
Exec=$GUI_DEST
Icon=endos-downloader
Terminal=false
Type=Application
Categories=Network;FileTransfer;
EOF
        install_file 644 "$desktop" "$DESKTOP_FILE"
        rm -f "$desktop"
    fi
}

install_service() {
    step "Installing the systemd service..."
    has_systemd || die "systemd is not running on this machine; the service needs systemd."
    if ! id "$SERVICE_USER" >/dev/null 2>&1; then
        $SUDO useradd --system --user-group --home-dir /var/lib/endos-downloader \
            --shell "$(command -v nologin || echo /usr/sbin/nologin)" "$SERVICE_USER"
    fi
    $SUDO install -d -m 2775 -o "$SERVICE_USER" -g "$SERVICE_USER" "$DOWNLOAD_DIR"
    if [ ! -f "$DOWNLOAD_DIR/queue.txt" ]; then
        printf '# One download per line: URL [mirror URL ...], magnet, .metalink or .torrent\n' |
            $SUDO tee "$DOWNLOAD_DIR/queue.txt" >/dev/null
        $SUDO chown "$SERVICE_USER:$SERVICE_USER" "$DOWNLOAD_DIR/queue.txt"
        $SUDO chmod 664 "$DOWNLOAD_DIR/queue.txt"
    fi
    install_file 644 "$SCRIPT_DIR/endos-downloader.service" "$SERVICE_FILE"
    $SUDO systemctl daemon-reload
    if systemctl is-active --quiet endos-downloader.service; then
        info "The service is running the previous binary; it uses the new one from its next start."
    fi
}

echo -e "${CYAN}==================================================================${RESET}"
echo -e "${CYAN}   Endo's Unified Downloader - Linux installer${RESET}"
echo -e "${CYAN}==================================================================${RESET}"

if [ "$UNINSTALL" = true ]; then
    uninstall
    exit 0
fi

install_system_deps
install_rust
build_binaries
install_binaries
if [ "$INSTALL_SERVICE" = true ]; then install_service; fi

echo -e "\n${GREEN}Installation complete.${RESET}"
echo "Run it from any terminal:"
echo -e "  ${CYAN}endos-downloader https://example.com/file.iso -d ~/Downloads${RESET}"
echo -e "  ${CYAN}endos-downloader -i links.txt -j 3 -d ~/Downloads${RESET}"
echo -e "  ${CYAN}endos-downloader --history${RESET}"
echo -e "  ${CYAN}endos-downloader --verify ~/Downloads/file.iso --repair${RESET}"
if [ "$INSTALL_GUI" = true ]; then
    echo -e "Start the GUI with ${CYAN}endos-downloader-gui${RESET} or from your application menu."
fi
if [ "$INSTALL_SERVICE" = true ]; then
    echo "Service: add URLs to $DOWNLOAD_DIR/queue.txt, then"
    echo -e "  ${CYAN}sudo systemctl start endos-downloader${RESET}   (add 'enable' to run it at every boot)"
    echo -e "  ${CYAN}journalctl -u endos-downloader -f${RESET}"
fi
