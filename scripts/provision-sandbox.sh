#!/usr/bin/env bash
# pylens sandbox provisioning — one-time setup of the nsjail execution environment.
#
# Run this INSIDE the Linux that will execute untrusted Python:
#   * native Linux:        bash scripts/provision-sandbox.sh
#   * Windows (WSL2):      wsl -d <distro> -- bash /mnt/c/Projects/ai/pylens/scripts/provision-sandbox.sh
#
# It (a) installs build deps + builds nsjail from a pinned tag, installing it onto PATH, and
# (b) deploys worker.py + the nsjail policy into the install dir the launcher expects:
#
#     $HOME/.local/share/pylens/{worker.py,pylens.nsjail.cfg}
#
# Requires sudo for the apt + `make install` steps (provisioning a machine is privileged by
# nature). Idempotent: re-running is safe and cheap once nsjail is present.
set -euo pipefail

NSJAIL_VERSION="${NSJAIL_VERSION:-3.4}"
INSTALL_DIR="${PYLENS_INSTALL_DIR:-$HOME/.local/share/pylens}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

say() { printf '\033[1;36m[provision]\033[0m %s\n' "$*"; }

# --- sanity: this must be Linux (a real kernel) ---
if [[ "$(uname -s)" != "Linux" ]]; then
  echo "error: must run on Linux (native or inside WSL2), not $(uname -s)." >&2
  exit 1
fi

# --- user namespaces must be available (nsjail uses CLONE_NEWUSER unprivileged) ---
if ! unshare -Ur true 2>/dev/null; then
  echo "error: unprivileged user namespaces are not available in this kernel." >&2
  echo "       On some hosts: sudo sysctl -w kernel.unprivileged_userns_clone=1" >&2
  exit 1
fi

# --- 1. nsjail ---
if command -v nsjail >/dev/null 2>&1; then
  say "nsjail already installed: $(command -v nsjail) ($(nsjail --help 2>&1 | head -1 || true))"
else
  say "installing build dependencies (sudo apt-get)…"
  sudo apt-get update -y
  sudo apt-get install -y \
    autoconf bison flex gcc g++ git libprotobuf-dev libnl-route-3-dev \
    libtool make pkg-config protobuf-compiler

  BUILD_DIR="$(mktemp -d)"
  trap 'rm -rf "$BUILD_DIR"' EXIT
  say "building nsjail $NSJAIL_VERSION in $BUILD_DIR…"
  git clone --depth 1 --branch "$NSJAIL_VERSION" https://github.com/google/nsjail.git "$BUILD_DIR/nsjail"
  make -C "$BUILD_DIR/nsjail" -j"$(nproc)"
  say "installing nsjail to /usr/local/bin (sudo)…"
  sudo install -m 0755 "$BUILD_DIR/nsjail/nsjail" /usr/local/bin/nsjail
fi

command -v nsjail >/dev/null 2>&1 || { echo "error: nsjail not on PATH after install." >&2; exit 1; }

# --- 2. python ---
if ! command -v python3 >/dev/null 2>&1; then
  say "installing python3 (sudo apt-get)…"
  sudo apt-get install -y python3
fi
say "python: $(python3 --version)"

# --- 3. deploy worker + policy ---
say "deploying worker + policy into $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
install -m 0644 "$REPO_ROOT/python/worker.py"          "$INSTALL_DIR/worker.py"
install -m 0644 "$REPO_ROOT/nsjail/pylens.nsjail.cfg"  "$INSTALL_DIR/pylens.nsjail.cfg"

# --- 4. smoke test: run the worker once, fully jailed ---
say "smoke-testing the jail…"
REQ='{"source":"def f(xs):\n    xs.append(1)\n    return xs","fn":"f","args":[[0]]}'
OUT="$(printf '%s' "$REQ" | nsjail --config "$INSTALL_DIR/pylens.nsjail.cfg" \
        --bindmount_ro "$INSTALL_DIR:/pylens" -- /usr/bin/python3 /pylens/worker.py 2>/dev/null || true)"
if printf '%s' "$OUT" | grep -q '"ok": true\|"ok":true'; then
  say "OK — jailed worker responded: $OUT"
else
  echo "warning: smoke test did not return ok. Output was:" >&2
  echo "$OUT" >&2
  echo "Inspect by re-running the nsjail command without 2>/dev/null to see jail logs." >&2
  exit 1
fi

say "done. The launcher will find: $INSTALL_DIR/{worker.py,pylens.nsjail.cfg} and nsjail on PATH."
