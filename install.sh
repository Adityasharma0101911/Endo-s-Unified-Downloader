#!/usr/bin/env bash
# ==============================================================================
# Endo's Unified Downloader - Linux installer
# Debian/Ubuntu, Fedora/RHEL, Arch, openSUSE and Alpine.
#
#   ./install.sh                install the newest release's build (x86_64 or aarch64), checked against its signed sums
#   ./install.sh --from-source  build it here instead (any architecture; run from a clone of the repository)
#   ./install.sh --gui          also install the GUI (by default only when a display is present)
#   ./install.sh --no-gui       CLI only
#   ./install.sh --service      also install the systemd batch-queue service
#   ./install.sh --uninstall    remove what the installer added (downloads and history are kept)
#
# Run it as your normal user; it uses sudo only for the system-wide install step.
# ==============================================================================

set -euo pipefail

PREFIX=/usr/local
BIN_DIR="$PREFIX/bin"
# The programs and the browser extension, side by side as the app expects; $BIN_DIR links to them.
LIB_DIR="$PREFIX/lib/endos-unified-downloader"
CLI_DEST="$BIN_DIR/endos-downloader-cli"
GUI_DEST="$BIN_DIR/endos-downloader-gui"
DESKTOP_FILE=/usr/share/applications/endos-downloader.desktop
ICON_FILE="$PREFIX/share/icons/hicolor/scalable/apps/endos-downloader.svg"
SERVICE_FILE=/etc/systemd/system/endos-downloader.service
SERVICE_USER=endos-downloader
DOWNLOAD_DIR=/var/downloads
MIN_RUST_MINOR=89 # Rust 1.89 or newer
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RELEASES=https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader/releases
# The maintainer's Ed25519 public key (raw, base64) a release's SHA256SUMS.sig must verify with:
# PUBLIC_KEY in crates/hyperfetch-core/src/updater.rs, which CI checks this matches.
PUBLIC_KEY=T0QVGQ63fXhyznv19veDUihSZBpBRxjGVOmPVoh85ms=

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
    sed -n '3,13p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

# --- Options ------------------------------------------------------------------
INSTALL_GUI=auto
INSTALL_SERVICE=false
UNINSTALL=false
FROM_SOURCE=false
for arg in "$@"; do
    case "$arg" in
        --from-source) FROM_SOURCE=true ;;
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
if [ "$UNINSTALL" = false ]; then
    if [ "$FROM_SOURCE" = true ]; then
        [ -f "$SCRIPT_DIR/Cargo.toml" ] || die "--from-source builds the clone of the repository install.sh is in, and there is none here."
    else
        case "$(uname -m)" in
            x86_64) ARCH=x64 ;;
            aarch64 | arm64) ARCH=arm64 ;;
            *) die "Releases are built for x86_64 and aarch64 only; on $(uname -m) run ./install.sh --from-source from a clone of the repository." ;;
        esac
    fi
    if [ "$INSTALL_SERVICE" = true ] && [ ! -f "$SCRIPT_DIR/endos-downloader.service" ]; then
        die "--service installs endos-downloader.service from a clone of the repository; run install.sh from one."
    fi
fi

# --- Privileges -----------------------------------------------------------------
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || die "sudo is not installed; run this script as root instead."
    SUDO=sudo
elif [ -n "${SUDO_USER:-}" ] && [ "$UNINSTALL" = false ] && [ "$FROM_SOURCE" = true ]; then
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

# Points the symlink $2 at $1, atomically.
link_file() {
    $SUDO mkdir -p "$(dirname "$2")"
    $SUDO ln -sfn "$1" "$2.new.$$"
    $SUDO mv -f "$2.new.$$" "$2"
}

# Updates the menu's list of what opens endo-dl: links, when the desktop has that tool.
refresh_menu() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        $SUDO update-desktop-database -q "$(dirname "$DESKTOP_FILE")" || true
    fi
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
    $SUDO rm -rf "$LIB_DIR"
    refresh_menu
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
    BIN_SRC="$SCRIPT_DIR/target/release"
    EXT_SRC="$SCRIPT_DIR/extension"
    ICON_SRC="$SCRIPT_DIR/assets/logo.svg"
}

# --- Or: the newest release -----------------------------------------------------
# get URL FILE [MESSAGE]: MESSAGE is the error when it cannot be downloaded.
get() { curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1" || die "${3:-Could not download $1}"; }

# Unpacks the newest release's Linux tar.gz into $WORK/app once it matches the release's
# SHA256SUMS, and that list the maintainer's signature when OpenSSL 3 can check it. The list names
# its version, so an older release cannot pass for the newest.
fetch_release() {
    step "[1/2] Downloading the newest release..."
    local tool version tarball
    for tool in curl tar sha256sum base64; do
        command -v "$tool" >/dev/null 2>&1 || die "$tool is needed; install it first."
    done
    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT
    get "$RELEASES/latest/download/SHA256SUMS" "$WORK/SHA256SUMS"
    version="$(sed -n '1s/^# version \([0-9]*\.[0-9]*\.[0-9]*\)$/\1/p' "$WORK/SHA256SUMS")"
    [ -n "$version" ] || die "The release's SHA256SUMS does not name its version; nothing was installed."
    tarball="Endos-Unified-Downloader-v$version-linux-$ARCH.tar.gz"
    info "Version $version"
    get "$RELEASES/download/v$version/$tarball" "$WORK/$tarball" \
        "Could not download $tarball (the release may have no Linux $ARCH build); run ./install.sh --from-source from a clone of the repository."
    if openssl version 2>/dev/null | grep -q '^OpenSSL [3-9]'; then
        get "$RELEASES/download/v$version/SHA256SUMS.sig" "$WORK/SHA256SUMS.sig"
        # An Ed25519 public key's DER is a fixed 12-byte header and the raw key, so in base64 the
        # header's MCowBQYDK2VwAyEA just goes in front.
        printf -- '-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA%s\n-----END PUBLIC KEY-----\n' "$PUBLIC_KEY" >"$WORK/key.pem"
        base64 -d "$WORK/SHA256SUMS.sig" >"$WORK/sig.bin" 2>/dev/null || die "The release's SHA256SUMS.sig is not base64; nothing was installed."
        openssl pkeyutl -verify -pubin -inkey "$WORK/key.pem" -rawin -in "$WORK/SHA256SUMS" -sigfile "$WORK/sig.bin" >/dev/null 2>&1 ||
            die "The release's SHA256SUMS is not signed by the maintainer's key; nothing was installed."
        info "Signature: good"
    else
        warn "OpenSSL 3 is not installed, so only the file's SHA-256 was checked, not the signature of the list it is in."
    fi
    (cd "$WORK" && awk -v name="$tarball" '$2 == name' SHA256SUMS | sha256sum -c - >/dev/null 2>&1) ||
        die "$tarball does not match the release's SHA256SUMS; nothing was installed."
    info "SHA-256: good"
    mkdir "$WORK/app"
    tar -xzf "$WORK/$tarball" -C "$WORK/app" --strip-components=1
    "$WORK/app/Endos-Unified-Downloader-CLI" --version >/dev/null 2>&1 ||
        die "The release's build does not run on this system (it needs a recent glibc); run ./install.sh --from-source from a clone of the repository."
    BIN_SRC="$WORK/app"
    EXT_SRC="$WORK/app/extension"
    ICON_SRC="$WORK/app/endos-unified-downloader.svg"
}

# --- Install ----------------------------------------------------------------------
install_binaries() {
    step "[$STEPS/$STEPS] Installing into $LIB_DIR, with the commands in $BIN_DIR..."
    install_file 755 "$BIN_SRC/Endos-Unified-Downloader-CLI" "$LIB_DIR/Endos-Unified-Downloader-CLI"
    link_file "$LIB_DIR/Endos-Unified-Downloader-CLI" "$CLI_DEST"
    link_alias endos-downloader
    link_alias hyperfetch

    if [ "$INSTALL_GUI" = true ]; then
        install_file 755 "$BIN_SRC/Endos-Unified-Downloader" "$LIB_DIR/Endos-Unified-Downloader"
        link_file "$LIB_DIR/Endos-Unified-Downloader" "$GUI_DEST"
        # The browser extension next to the GUI, where its Settings show it; without its tests.
        $SUDO rm -rf "$LIB_DIR/extension.new"
        $SUDO cp -R "$EXT_SRC" "$LIB_DIR/extension.new"
        $SUDO rm -rf "$LIB_DIR/extension.new/test" "$LIB_DIR/extension.new/package.json" "$LIB_DIR/extension"
        $SUDO mv "$LIB_DIR/extension.new" "$LIB_DIR/extension"
        install_file 644 "$ICON_SRC" "$ICON_FILE"
        local desktop
        desktop="$(mktemp)"
        cat >"$desktop" <<EOF
[Desktop Entry]
Name=Endo's Unified Downloader
Comment=Multi-connection download accelerator
Exec=$GUI_DEST %u
Icon=endos-downloader
Terminal=false
Type=Application
Categories=Network;FileTransfer;
MimeType=x-scheme-handler/endo-dl;
EOF
        install_file 644 "$desktop" "$DESKTOP_FILE"
        rm -f "$desktop"
        refresh_menu
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

if [ "$FROM_SOURCE" = true ]; then
    STEPS=4
    install_system_deps
    install_rust
    build_binaries
else
    STEPS=2
    fetch_release
fi
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
