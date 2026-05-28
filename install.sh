#!/usr/bin/env bash
# install.sh — build cb, install it into a user-owned bin dir on your PATH
# (no sudo), then register hooks for whichever agents are installed.
#
# By default this installs to ~/.cb/bin (its own dir, like bun's ~/.bun/bin)
# and, if that dir isn't already on your PATH, appends an `export PATH=...` line
# to your shell rc (the same trick rustup/bun use). Restart your shell (or
# source the rc) afterwards.
#
# It then installs hooks for Claude Code and Codex, but only for the CLIs you
# actually have: each is skipped (with a notice) if its command isn't found.
#
# Usage:
#   ./install.sh                 # build + install to ~/.cb/bin + PATH + hooks
#   BINDIR=~/bin ./install.sh    # install to a different user dir
#   ./install.sh --no-hooks      # skip all hook installation
set -euo pipefail

cd "$(dirname "$0")"

ORIG_PATH="$PATH"
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

# Install into our own user-owned dir (like bun's ~/.bun/bin): $BINDIR or ~/.cb/bin.
BINDIR="${BINDIR:-$HOME/.cb/bin}"
# Expand a leading ~ and make absolute so PATH comparisons are reliable.
BINDIR="${BINDIR/#\~/$HOME}"
# Install root (parent of bin), used for the bun-style CB_INSTALL rc block.
CB_INSTALL="${BINDIR%/bin}"

echo ">> building cb"
go build -o dist/cb .

echo ">> installing to $BINDIR/cb"
mkdir -p "$BINDIR" 2>/dev/null || true
if [ -w "$BINDIR" ]; then
  install -m 0755 dist/cb "$BINDIR/cb"
else
  echo "   ($BINDIR is not writable — using sudo)"
  sudo install -m 0755 dist/cb "$BINDIR/cb"
fi

# Is BINDIR already on the PATH we started with?
on_path=0
case ":$ORIG_PATH:" in
  *":$BINDIR:"*) on_path=1 ;;
esac

# rc_for_shell prints the shell rc file to edit for the user's login shell.
rc_for_shell() {
  case "$(basename "${SHELL:-}")" in
    zsh)  echo "$HOME/.zshrc" ;;
    bash) [ -f "$HOME/.bashrc" ] && echo "$HOME/.bashrc" || echo "$HOME/.bash_profile" ;;
    fish) echo "$HOME/.config/fish/config.fish" ;;
    *)    echo "" ;;
  esac
}

if [ "$on_path" -eq 0 ]; then
  RC="$(rc_for_shell)"
  MARKER="# command-center (cb)"
  is_fish=0; [ "$(basename "${SHELL:-}")" = "fish" ] && is_fish=1
  # Emit a bun-style block (CB_INSTALL + $CB_INSTALL/bin on PATH) when the bin
  # dir is the conventional <root>/bin; otherwise just add BINDIR directly.
  if [ -n "$RC" ] && ! grep -qsF "$MARKER" "$RC"; then
    mkdir -p "$(dirname "$RC")"
    {
      echo ""
      echo "$MARKER"
      if [ "$is_fish" -eq 1 ]; then
        if [ "$BINDIR" = "$CB_INSTALL/bin" ]; then
          echo "set -gx CB_INSTALL \"$CB_INSTALL\""
          echo "set -gx PATH \"\$CB_INSTALL/bin\" \$PATH"
        else
          echo "set -gx PATH \"$BINDIR\" \$PATH"
        fi
      else
        if [ "$BINDIR" = "$CB_INSTALL/bin" ]; then
          echo "export CB_INSTALL=\"$CB_INSTALL\""
          echo "export PATH=\"\$CB_INSTALL/bin:\$PATH\""
        else
          echo "export PATH=\"$BINDIR:\$PATH\""
        fi
      fi
    } >> "$RC"
    echo ">> added $BINDIR to your PATH in $RC"
    echo "   run:  source \"$RC\"   (or open a new terminal) to pick it up"
  else
    echo "!! $BINDIR is not on your PATH. Add it manually:"
    echo "     export PATH=\"$BINDIR:\$PATH\""
  fi
  # Make cb resolvable for the rest of THIS script run too.
  export PATH="$BINDIR:$PATH"
fi

# Sanity: confirm the cb we'll use is the one we just installed.
RESOLVED="$(command -v cb || true)"
if [ "$RESOLVED" != "$BINDIR/cb" ]; then
  echo "!! note: 'cb' currently resolves to '${RESOLVED:-<none>}', not $BINDIR/cb"
  echo "   ensure $BINDIR comes first on your PATH."
fi

if [ "$INSTALL_HOOKS" -eq 1 ]; then
  if command -v claude >/dev/null 2>&1; then
    echo ">> installing Claude Code hooks"
    "$BINDIR/cb" install-hooks
  else
    echo "claude code does not exist, skipping initialization for claude code"
  fi
  if command -v codex >/dev/null 2>&1; then
    echo ">> installing Codex hooks"
    "$BINDIR/cb" install-codex
  else
    echo "codex does not exist, skipping initialization for codex"
  fi
fi

echo ">> done. run: cb"
