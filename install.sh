#!/usr/bin/env bash
# ==============================================================================
# Endo's Unified Downloader - Linux Automated Installer
# Supports Ubuntu, Debian, Fedora, CentOS/RHEL, Arch Linux, and Alpine
# ==============================================================================

set -e

COLOR_CYAN='\033[0;36m'
COLOR_GREEN='\033[0;32m'
COLOR_YELLOW='\033[1;33m'
COLOR_RED='\033[0;31m'
COLOR_RESET='\033[0m'

echo -e "${COLOR_CYAN}===================================================================${COLOR_RESET}"
echo -e "${COLOR_CYAN}    Endo's Unified Downloader - Linux Server & Desktop Installer   ${COLOR_RESET}"
echo -e "${COLOR_CYAN}===================================================================${COLOR_RESET}"

# 1. Detect Package Manager and Install Prerequisites
install_system_deps() {
    echo -e "\n${COLOR_YELLOW}[1/4] Installing system build dependencies...${COLOR_RESET}"
    if command -v apt-get &> /dev/null; then
        sudo apt-get update -qq
        sudo apt-get install -y -qq build-essential pkg-config libssl-dev git curl
        if [ "$INSTALL_GUI" = "true" ]; then
            sudo apt-get install -y -qq libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libgtk-3-dev
        fi
    elif command -v dnf &> /dev/null; then
        sudo dnf install -y -q gcc gcc-c++ make pkgconf openssl-devel git curl
    elif command -v pacman &> /dev/null; then
        sudo pacman -Sy --noconfirm base-devel openssl git curl
    elif command -v zypper &> /dev/null; then
        sudo zypper install -y gcc gcc-c++ make pkg-config libopenssl-devel git curl
    else
        echo -e "${COLOR_YELLOW}[WARN] Unknown package manager. Please ensure gcc, pkg-config, and OpenSSL development headers are installed.${COLOR_RESET}"
    fi
}

# 2. Check or Install Rust Toolchain
install_rust() {
    echo -e "\n${COLOR_YELLOW}[2/4] Verifying Rust toolchain...${COLOR_RESET}"
    if ! command -v cargo &> /dev/null; then
        echo -e "${COLOR_CYAN}Rust not found. Installing Rust via rustup...${COLOR_RESET}"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        # shellcheck source=/dev/null
        source "$HOME/.cargo/env"
    else
        echo -e "${COLOR_GREEN}Rust is already installed: $(rustc --version)${COLOR_RESET}"
    fi
}

# 3. Build Release Binaries
build_binaries() {
    echo -e "\n${COLOR_YELLOW}[3/4] Compiling release binaries...${COLOR_RESET}"
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    cd "$SCRIPT_DIR"

    echo -e "${COLOR_CYAN}Building CLI accelerator (Endos-Unified-Downloader-CLI)...${COLOR_RESET}"
    cargo build --release -p hyperfetch-cli

    if [ "$INSTALL_GUI" = "true" ]; then
        echo -e "${COLOR_CYAN}Building Native GUI (Endos-Unified-Downloader)...${COLOR_RESET}"
        cargo build --release -p hyperfetch-gui || {
            echo -e "${COLOR_YELLOW}[WARN] GUI build skipped (display or X11 dependencies missing). CLI build succeeded.${COLOR_RESET}"
            INSTALL_GUI="false"
        }
    fi
}

# 4. Install Binaries to System PATH
install_binaries() {
    echo -e "\n${COLOR_YELLOW}[4/4] Installing executables to /usr/local/bin...${COLOR_RESET}"
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    
    sudo cp "$SCRIPT_DIR/target/release/Endos-Unified-Downloader-CLI" /usr/local/bin/endos-downloader-cli
    sudo chmod +x /usr/local/bin/endos-downloader-cli
    sudo ln -sf /usr/local/bin/endos-downloader-cli /usr/local/bin/endos-downloader
    sudo ln -sf /usr/local/bin/endos-downloader-cli /usr/local/bin/hyperfetch

    if [ "$INSTALL_GUI" = "true" ] && [ -f "$SCRIPT_DIR/target/release/Endos-Unified-Downloader" ]; then
        sudo cp "$SCRIPT_DIR/target/release/Endos-Unified-Downloader" /usr/local/bin/endos-downloader-gui
        sudo chmod +x /usr/local/bin/endos-downloader-gui

        # Install Desktop entry for application launchers
        sudo mkdir -p /usr/share/applications
        cat <<EOF | sudo tee /usr/share/applications/endos-downloader.desktop > /dev/null
[Desktop Entry]
Name=Endo's Unified Downloader
Comment=High-Speed Unified Download Accelerator & Ingestion Engine
Exec=/usr/local/bin/endos-downloader-gui
Terminal=false
Type=Application
Categories=Network;FileTransfer;
EOF
    fi
}

# Parse options
INSTALL_GUI="false"
for arg in "$@"; do
    case $arg in
        --gui)
            INSTALL_GUI="true"
            shift
            ;;
    esac
done

if [ -n "$DISPLAY" ] || [ -n "$WAYLAND_DISPLAY" ]; then
    INSTALL_GUI="true"
fi

install_system_deps
install_rust
build_binaries
install_binaries

echo -e "\n${COLOR_GREEN}===================================================================${COLOR_RESET}"
echo -e "${COLOR_GREEN}  Installation Complete!                                          ${COLOR_RESET}"
echo -e "${COLOR_GREEN}===================================================================${COLOR_RESET}"
echo -e "You can now run Endo's Unified Downloader from any terminal:"
echo -e "  ${COLOR_CYAN}endos-downloader <URL> [OPTIONS]${COLOR_RESET}"
echo -e "  ${COLOR_CYAN}hyperfetch <URL> -s 16 -d /path/to/downloads${COLOR_RESET}"
echo -e "  ${COLOR_CYAN}hyperfetch -i links.txt -q${COLOR_RESET}"
echo -e "  ${COLOR_CYAN}hyperfetch --history${COLOR_RESET}"
echo -e "  ${COLOR_CYAN}hyperfetch --verify <FILE> --repair${COLOR_RESET}"
if [ "$INSTALL_GUI" = "true" ]; then
    echo -e "Launch GUI with:"
    echo -e "  ${COLOR_CYAN}endos-downloader-gui${COLOR_RESET}"
fi
echo ""
