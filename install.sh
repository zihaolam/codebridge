#!/usr/bin/env bash
# install.sh — install cb (codebridge) into a user-owned bin dir on your
# PATH (no sudo), then register hooks for whichever agents are installed.
#
# By default this downloads a prebuilt binary from GitHub Releases — no Go
# toolchain required. Set BUILD_FROM_SOURCE=1 to compile locally instead
# (needs Go 1.25+).
#
# Installs to ~/.cb/bin (its own dir, like bun's ~/.bun/bin) and, if that dir
# isn't already on your PATH, appends an `export PATH=...` line to your shell
# rc (the same trick rustup/bun use). Restart your shell (or source the rc)
# afterwards.
#
# It then installs hooks for Claude Code and Codex, but only for the CLIs you
# actually have: each is skipped (with a notice) if its command isn't found.
#
# Usage:
#   ./install.sh                       # download latest release + install + hooks
#   curl -fsSL https://raw.githubusercontent.com/zihaolam/codebridge/main/install.sh | bash
#   VERSION=v0.2.0 ./install.sh        # pin to a specific release
#   BINDIR=~/bin ./install.sh          # install to a different user dir
#   BUILD_FROM_SOURCE=1 ./install.sh   # compile locally (needs Go 1.25+)
#   ./install.sh --no-hooks            # skip all hook installation
set -euo pipefail

REPO="zihaolam/codebridge"

# Tempdir used by download_release(); declared at script scope so the EXIT
# trap below can reference it safely under `set -u`.
tmpdir=""
trap '[ -n "$tmpdir" ] && rm -rf "$tmpdir"' EXIT

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
BINDIR="${BINDIR/#\~/$HOME}"
CB_INSTALL="${BINDIR%/bin}"

# detect_platform sets OS / ARCH to the names goreleaser uses in archive names.
detect_platform() {
  local uname_s uname_m
  uname_s="$(uname -s)"
  uname_m="$(uname -m)"
  case "$uname_s" in
    Darwin) OS="darwin" ;;
    Linux)  OS="linux"  ;;
    *) echo "unsupported OS: $uname_s" >&2; exit 1 ;;
  esac
  case "$uname_m" in
    x86_64|amd64) ARCH="amd64" ;;
    arm64|aarch64) ARCH="arm64" ;;
    *) echo "unsupported arch: $uname_m" >&2; exit 1 ;;
  esac
}

build_from_source() {
  if ! command -v go >/dev/null 2>&1; then
    echo "!! BUILD_FROM_SOURCE=1 but 'go' is not on PATH. Install Go 1.25+ or unset BUILD_FROM_SOURCE." >&2
    exit 1
  fi
  echo ">> building cb from source"
  # Resolve script dir for source builds (download path has no source tree).
  cd "$(cd "$(dirname "$0")" && pwd)"
  mkdir -p dist
  go build -o dist/cb .
  BINARY="dist/cb"
}

download_release() {
  detect_platform
  local version asset url
  version="${VERSION:-}"
  if [ -z "$version" ]; then
    echo ">> resolving latest release"
    version="$(
      curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -m1 '"tag_name"' \
        | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/'
    )"
    if [ -z "$version" ]; then
      echo "!! could not resolve latest release tag. Set VERSION=vX.Y.Z explicitly." >&2
      exit 1
    fi
  fi
  asset="cb_${OS}_${ARCH}.tar.gz"
  url="https://github.com/$REPO/releases/download/${version}/${asset}"
  tmpdir="$(mktemp -d)"
  echo ">> downloading $asset ($version)"
  if ! curl -fsSL "$url" -o "$tmpdir/$asset"; then
    echo "!! download failed: $url" >&2
    echo "   if this platform isn't published yet, try BUILD_FROM_SOURCE=1 ./install.sh" >&2
    exit 1
  fi
  tar -xzf "$tmpdir/$asset" -C "$tmpdir"
  if [ ! -x "$tmpdir/cb" ]; then
    echo "!! archive did not contain a 'cb' binary" >&2
    exit 1
  fi
  BINARY="$tmpdir/cb"
}

if [ "${BUILD_FROM_SOURCE:-0}" = "1" ]; then
  build_from_source
else
  download_release
fi

echo ">> installing to $BINDIR/cb"
mkdir -p "$BINDIR" 2>/dev/null || true
if [ -w "$BINDIR" ]; then
  install -m 0755 "$BINARY" "$BINDIR/cb"
else
  echo "   ($BINDIR is not writable — using sudo)"
  sudo install -m 0755 "$BINARY" "$BINDIR/cb"
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
  MARKER="# codebridge (cb)"
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
