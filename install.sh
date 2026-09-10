#!/bin/sh
# Install a downloaded release or build this source checkout. Never requires sudo.
set -eu
builder_source=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
builder_destination=${BUILDER_INSTALL_DIR:-"$HOME/.local/bin"}
if [ -f "$builder_source/builder" ]; then
    mkdir -p "$builder_destination"
    install -m 755 "$builder_source/builder" "$builder_destination/builder"
    printf 'Installed Builder to %s/builder\n' "$builder_destination"
    case ":$PATH:" in
        *":$builder_destination:"*) ;;
        *) printf 'Add this directory to PATH: %s\n' "$builder_destination" ;;
    esac
elif command -v cargo >/dev/null 2>&1; then
    cargo install --path "$builder_source" --locked
else
    printf '%s\n' 'This is a source checkout. Install stable Rust from https://rustup.rs, then run ./install.sh again.' >&2
    exit 1
fi
