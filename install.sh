#!/usr/bin/env bash
# install.sh — build cb and install it onto your PATH, then register the
# Claude Code hooks against the installed binary.
#
# Usage:
#   ./install.sh                 # build + install to a standard bin dir + hooks
#   BINDIR=~/bin ./install.sh    # install somewhere else (must be on your PATH)
#   ./install.sh --no-hooks      # skip the install-hooks step
set -euo pipefail

cd "$(dirname "$0")"

INSTALL_HOOKS=1
for arg in "$@"; do
  case "$arg" in
    --no-hooks) INSTALL_HOOKS=0 ;;
    -h|--help)
      grep '^#' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

# Pick a bin directory. Convention for hand-installed (non-package-manager)
# binaries is /usr/local/bin; on Apple Silicon Homebrew owns /opt/homebrew/bin.
# Override with BINDIR=... if you keep a personal ~/bin or ~/.local/bin on PATH.
if [ -z "${BINDIR:-}" ]; then
  if [ -d /opt/homebrew/bin ] && [ -w /opt/homebrew/bin ]; then
    BINDIR=/opt/homebrew/bin
  else
    BINDIR=/usr/local/bin
  fi
fi

echo ">> building cb"
go build -o cb .

echo ">> installing to $BINDIR/cb"
if [ -w "$BINDIR" ]; then
  install -m 0755 cb "$BINDIR/cb"
else
  echo "   (need elevated permissions to write $BINDIR)"
  sudo install -m 0755 cb "$BINDIR/cb"
fi

# Sanity: confirm the thing on PATH is the one we just installed.
RESOLVED="$(command -v cb || true)"
if [ "$RESOLVED" != "$BINDIR/cb" ]; then
  echo "!! warning: 'cb' on PATH resolves to '${RESOLVED:-<none>}', not $BINDIR/cb"
  echo "   make sure $BINDIR comes first on your PATH."
fi

if [ "$INSTALL_HOOKS" -eq 1 ]; then
  echo ">> installing Claude Code hooks (using the on-PATH 'cb')"
  "$BINDIR/cb" install-hooks
fi

echo ">> done. run: cb"
