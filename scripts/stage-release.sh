#!/usr/bin/env bash
# Stages compiled release binaries and assets into a clean distribution payload.
# Usage: ./scripts/stage-release.sh --target <target-triple> [--dest <dir>]
#
# "Hey you, you're finally awake."

set -euo pipefail

TARGET=""
DEST_DIR=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target)
      TARGET="$2"
      shift 2
      ;;
    --dest)
      DEST_DIR="$2"
      shift 2
      ;;
    -h|--help)
      echo "Usage: $0 --target <target-triple> [--dest <dir>]"
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

if [[ -z "$TARGET" ]]; then
  echo "Error: --target <target-triple> is required" >&2
  exit 1
fi

BIN_EXT=""
if [[ "$TARGET" =~ windows ]]; then
  BIN_EXT=".exe"
fi

SOURCE_DIR="target/${TARGET}/release"
if [[ ! -d "$SOURCE_DIR" ]]; then
  # Fallback for local untargeted release builds
  if [[ -d "target/release" && -z "${CI:-}" ]]; then
    SOURCE_DIR="target/release"
  else
    echo "Error: Release build directory not found: $SOURCE_DIR" >&2
    echo "Did you run 'cargo build --release --target $TARGET'?" >&2
    exit 1
  fi
fi

if [[ -z "$DEST_DIR" ]]; then
  DEST_DIR="target/${TARGET}/staging"
fi

echo "==> Staging release payload for $TARGET into $DEST_DIR..."
rm -rf "$DEST_DIR"
mkdir -p "$DEST_DIR"

BINARIES=(
  "launcher"
  "engine"
  "converter"
  "world-inspect"
)

for bin in "${BINARIES[@]}"; do
  BIN_FILE="${bin}${BIN_EXT}"
  SRC_PATH="${SOURCE_DIR}/${BIN_FILE}"
  if [[ ! -f "$SRC_PATH" ]]; then
    echo "Error: Required binary not found: $SRC_PATH" >&2
    exit 1
  fi
  cp "$SRC_PATH" "$DEST_DIR/"
  if [[ -z "$BIN_EXT" ]]; then
    chmod +x "$DEST_DIR/$BIN_FILE"
  fi
  echo "  ✓ Staged $BIN_FILE"
done

# Include license in distribution payload
if [[ -f "LICENSE" ]]; then
  cp "LICENSE" "$DEST_DIR/"
  echo "  ✓ Staged LICENSE"
fi

echo "==> Staging complete: $(ls -1 "$DEST_DIR" | wc -l) files staged in $DEST_DIR."
