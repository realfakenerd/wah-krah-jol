#!/bin/sh
# Mudcrab Installer for Linux & macOS
# Usage: curl -fsSL https://raw.githubusercontent.com/Mudcrab-Team/mudcrab/main/scripts/install.sh | sh
#
# "Hey you, you're finally awake."

set -eu

REPO="Mudcrab-Team/mudcrab"
INSTALL_DIR="${MUDCRAB_INSTALL_DIR:-$HOME/.local/share/mudcrab}"
BIN_DIR="${MUDCRAB_BIN_DIR:-$HOME/.local/bin}"

# Colors
if [ -t 1 ]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    BLUE='\033[0;34m'
    YELLOW='\033[1;33m'
    BOLD='\033[1m'
    RESET='\033[0m'
else
    RED=''
    GREEN=''
    BLUE=''
    YELLOW=''
    BOLD=''
    RESET=''
fi

# Print the message in $1 to stdout with the installer progress prefix and colors.
info() {
    printf "${BLUE}${BOLD}==>${RESET} ${BOLD}%s${RESET}\n" "$1"
}

# Print the message in $1 to stdout with a success marker and colors.
success() {
    printf "${GREEN}${BOLD}✓${RESET} %s\n" "$1"
}

# Print the message in $1 to stdout with a warning marker and colors.
warn() {
    printf "${YELLOW}${BOLD}!${RESET} %s\n" "$1"
}

# Print the message in $1 to stderr with an error prefix and colors, then exit 1.
error() {
    printf "${RED}${BOLD}error:${RESET} %s\n" "$1" >&2
    exit 1
}

main() {
    # 1. Detect OS & Architecture
    OS="$(uname -s)"
    ARCH="$(uname -m)"

    case "$OS" in
        Linux)
            OS_TYPE="unknown-linux-gnu"
            ;;
        Darwin)
            OS_TYPE="apple-darwin"
            ;;
        *)
            error "Unsupported operating system: $OS. Mudcrab supports Linux and macOS (Windows uses installer wizard)."
            ;;
    esac

    case "$ARCH" in
        x86_64|amd64)
            ARCH_TYPE="x86_64"
            ;;
        aarch64|arm64)
            if [ "$OS" = "Linux" ]; then
                error "Linux ARM64 is not yet officially distributed in pre-built releases. Please compile from source."
            fi
            ARCH_TYPE="aarch64"
            ;;
        *)
            error "Unsupported architecture: $ARCH"
            ;;
    esac

    TARGET="${ARCH_TYPE}-${OS_TYPE}"

    info "Detected platform: ${BOLD}${TARGET}${RESET}"

    # 2. Resolve Version
    VERSION="${MUDCRAB_VERSION:-latest}"

    if [ "$VERSION" = "latest" ]; then
        info "Resolving latest Mudcrab release..."
        # /releases/latest redirects to /releases/tag/<tag>; with no published release it
        # redirects to /releases instead, which must not be mistaken for a tag.
        LATEST_URL="$(curl -sIL -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest" 2>/dev/null || true)"
        case "$LATEST_URL" in
            */releases/tag/*) TAG="${LATEST_URL##*/}" ;;
            *) error "No published release found for ${REPO}. Set MUDCRAB_VERSION to install a specific tag." ;;
        esac
    else
        # Ensure tag starts with 'v'
        case "$VERSION" in
            v*) TAG="$VERSION" ;;
            *)  TAG="v$VERSION" ;;
        esac
    fi

    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${TAG}/mudcrab-${TAG}-${TARGET}.tar.gz"
    CHECKSUM_URL="https://github.com/${REPO}/releases/download/${TAG}/SHA256SUMS.txt"
    ARCHIVE_FILENAME="mudcrab-${TAG}-${TARGET}.tar.gz"

    info "Downloading Mudcrab ${BOLD}${TAG}${RESET} (${TARGET})..."

    # 3. Create temporary directory for download
    TMP_DIR="$(mktemp -d)"
    # Remove the download directory in TMP_DIR when the exit or signal trap runs.
    cleanup() {
        rm -rf "$TMP_DIR"
    }
    trap cleanup EXIT INT TERM

    ARCHIVE="${TMP_DIR}/${ARCHIVE_FILENAME}"
    CHECKSUMS="${TMP_DIR}/SHA256SUMS.txt"

    if ! curl -fL --progress-bar "$DOWNLOAD_URL" -o "$ARCHIVE"; then
        error "Failed to download Mudcrab from: $DOWNLOAD_URL
Check that the release tag '${TAG}' and target '${TARGET}' exist."
    fi

    info "Verifying SHA256 checksum..."
    curl -fsSL "$CHECKSUM_URL" -o "$CHECKSUMS" 2>/dev/null \
        || error "SHA256SUMS.txt could not be fetched for release ${TAG}; aborting installation."

    EXPECTED_HASH="$(awk -v f="$ARCHIVE_FILENAME" '$2 == f || $2 == "*" f { print $1; exit }' "$CHECKSUMS")"
    [ -n "$EXPECTED_HASH" ] || error "No entry for ${ARCHIVE_FILENAME} in SHA256SUMS.txt; aborting installation."

    if command -v sha256sum >/dev/null 2>&1; then
        ACTUAL_HASH="$(sha256sum "$ARCHIVE" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        ACTUAL_HASH="$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')"
    else
        error "Neither sha256sum nor shasum found on system; cannot verify checksum."
    fi

    if [ "$EXPECTED_HASH" != "$ACTUAL_HASH" ]; then
        error "Checksum verification failed!
Expected: ${EXPECTED_HASH}
Got:      ${ACTUAL_HASH}"
    fi
    success "Checksum verified ($EXPECTED_HASH)"

    # 4. Extract into installation directory
    info "Installing to ${BOLD}${INSTALL_DIR}${RESET}..."

    mkdir -p "${INSTALL_DIR}/bin" "${BIN_DIR}"
    # Extract aside, then rename into place: replacing a running binary in place
    # fails with "text file busy", renaming over it does not.
    STAGE_DIR="${TMP_DIR}/stage"
    mkdir -p "$STAGE_DIR"
    tar -xzf "$ARCHIVE" -C "$STAGE_DIR"
    for staged in "$STAGE_DIR"/*; do
        mv -f "$staged" "${INSTALL_DIR}/bin/"
    done

    # 5. Symlink binaries to BIN_DIR as mudcrab-<name>. The real files keep their plain
    # names because the launcher finds the engine next to its own (resolved) path.
    BINARIES="launcher engine converter world-inspect"
    for bin in $BINARIES; do
        if [ -f "${INSTALL_DIR}/bin/${bin}" ]; then
            chmod +x "${INSTALL_DIR}/bin/${bin}"
            LINK="${BIN_DIR}/mudcrab-${bin}"
            if [ -e "$LINK" ] || [ -L "$LINK" ]; then
                # Only replace a symlink that already points into this installation.
                if [ ! -L "$LINK" ] || [ "$(readlink "$LINK")" != "${INSTALL_DIR}/bin/${bin}" ]; then
                    error "Refusing to overwrite existing '${LINK}'. Remove it or set MUDCRAB_BIN_DIR to another directory."
                fi
            fi
            ln -sf "${INSTALL_DIR}/bin/${bin}" "$LINK"
        fi
    done

    # 6. Linux Desktop integration
    if [ "$OS" = "Linux" ]; then
        APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
        mkdir -p "$APPS_DIR"
        # Desktop Entry Exec: quote the path and escape \ " ` $ inside the quotes.
        DESKTOP_EXEC="$(printf '%s' "${BIN_DIR}/mudcrab-launcher" | sed -e 's/[\\"`$]/\\&/g')"
        # Start in the install dir: the launcher writes its converted output to
        # ./modern_assets, and a menu launch would otherwise use the home directory.
        DESKTOP_PATH="$(printf '%s' "${INSTALL_DIR}" | sed -e 's/\\/&&/g')"
        cat <<EOF > "${APPS_DIR}/mudcrab.desktop"
[Desktop Entry]
Name=Mudcrab
Comment=Modern Skyrim Engine and Converter
Exec="${DESKTOP_EXEC}"
Path=${DESKTOP_PATH}
Terminal=false
Type=Application
Categories=Game;
StartupNotify=true
EOF
        success "Created desktop entry: ${APPS_DIR}/mudcrab.desktop"
    fi

    success "Installed Mudcrab binaries to ${BOLD}${BIN_DIR}${RESET}"

    # 7. Check PATH
    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *)
            printf "\n"
            warn "${BIN_DIR} is not in your PATH!"
            printf "To run 'mudcrab-launcher', 'mudcrab-engine', or 'mudcrab-converter' from anywhere, add this to your shell profile:\n\n"
            printf "  ${BOLD}export PATH=\"%s:\$PATH\"${RESET}\n\n" "$BIN_DIR"
            ;;
    esac

    printf "\n"
    printf "${GREEN}${BOLD}🦀 Mudcrab ${TAG} is ready!${RESET}\n"
    printf "Run ${BOLD}mudcrab-launcher${RESET} to configure your Skyrim data and launch the engine.\n"
}

main "$@"
