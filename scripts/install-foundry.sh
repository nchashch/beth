#!/usr/bin/env bash
# Installs the Foundry toolchain (forge, cast, anvil, chisel) via foundryup, for driving beth's
# JSON-RPC (see scripts/regtest-up.sh) -- signing and sending transactions, deploying and
# testing contracts, from the command line.
#
# Safe to re-run: installs foundryup itself only if missing, then always runs it to install (or
# update) the actual binaries.

set -euo pipefail

command -v curl >/dev/null || { echo "error: curl is required" >&2; exit 1; }

# foundryup's own installer respects XDG_CONFIG_HOME when set, installing to
# $XDG_CONFIG_HOME/.foundry rather than the "traditional" $HOME/.foundry -- check both, plus
# whatever's already on PATH, rather than assuming one location.
find_foundry_bin() {
    if command -v foundryup >/dev/null; then
        dirname "$(command -v foundryup)"
        return 0
    fi
    local candidates=()
    [ -n "${XDG_CONFIG_HOME:-}" ] && candidates+=("$XDG_CONFIG_HOME/.foundry/bin")
    candidates+=("$HOME/.config/.foundry/bin" "$HOME/.foundry/bin")
    local dir
    for dir in "${candidates[@]}"; do
        if [ -x "$dir/foundryup" ]; then
            printf '%s' "$dir"
            return 0
        fi
    done
    return 1
}

FOUNDRY_BIN="$(find_foundry_bin || true)"
if [ -z "$FOUNDRY_BIN" ]; then
    echo "Installing foundryup..."
    curl -L https://foundry.paradigm.xyz | bash
    FOUNDRY_BIN="$(find_foundry_bin || true)"
fi

if [ -z "$FOUNDRY_BIN" ]; then
    echo "error: could not find foundryup after installing it" >&2
    exit 1
fi

echo "Running foundryup ($FOUNDRY_BIN/foundryup) to install/update forge, cast, anvil, and chisel..."
"$FOUNDRY_BIN/foundryup"

echo
echo "Installed:"
for bin in forge cast anvil chisel; do
    if [ -x "$FOUNDRY_BIN/$bin" ]; then
        echo "  $bin: $("$FOUNDRY_BIN/$bin" --version 2>&1 | head -1)"
    else
        echo "  $bin: NOT FOUND" >&2
    fi
done

if ! command -v cast >/dev/null; then
    echo
    echo "Note: $FOUNDRY_BIN isn't on PATH in this shell yet. foundryup's installer adds it to"
    echo "your shell profile (~/.bashrc, ~/.zshrc, ...) for new shells; for this one, run:"
    echo "  export PATH=\"$FOUNDRY_BIN:\$PATH\""
fi
