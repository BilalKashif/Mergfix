#!/bin/sh
# Install mergefix on macOS or Linux: installs the build tools if needed,
# builds mergefix and installs it as an app plus a `mergefix` command.
#
#   ./setup.sh                  install or update
#   ./setup.sh --git-mergetool  also make mergefix your `git mergetool`
#   ./setup.sh --uninstall      remove everything this script installed
set -eu
cd "$(dirname "$0")"

MIN_RUST=85 # Rust 1.85 is the first release with edition 2024
CARGO_DIR="${CARGO_HOME:-$HOME/.cargo}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"

step() { printf '\n==> %s\n' "$1"; }
has() { command -v "$1" >/dev/null 2>&1; }

GIT=0
UNINSTALL=0
for arg in "$@"; do
  case "$arg" in
    --git-mergetool) GIT=1 ;;
    --uninstall) UNINSTALL=1 ;;
    -h|--help) sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "Unknown option: $arg (see ./setup.sh --help)"; exit 1 ;;
  esac
done

case "$(uname -s)" in
  Darwin)
    OS=mac
    APP=/Applications/mergefix.app
    BIN="$APP/Contents/MacOS/mergefix"
    LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
    ;;
  Linux)
    OS=linux
    BIN="$HOME/.local/bin/mergefix"
    DESKTOP="$DATA_DIR/applications/mergefix.desktop"
    ICON="$DATA_DIR/icons/hicolor/256x256/apps/mergefix.png"
    ;;
  *)
    echo "setup.sh supports macOS and Linux. Elsewhere, run: cargo install --path ."
    exit 1
    ;;
esac

remove_git_config() {
  has git || return 0
  if git config --global --get mergetool.mergefix.cmd >/dev/null 2>&1; then
    git config --global --remove-section mergetool.mergefix
  fi
  if [ "$(git config --global --get merge.tool || true)" = "mergefix" ]; then
    git config --global --unset merge.tool
  fi
}

if [ "$UNINSTALL" = 1 ]; then
  step "Removing mergefix"
  if [ "$OS" = mac ]; then
    if [ -d "$APP" ]; then
      "$LSREGISTER" -u "$APP" 2>/dev/null || true
      rm -rf "$APP"
    fi
    if [ -L "$CARGO_DIR/bin/mergefix" ]; then rm -f "$CARGO_DIR/bin/mergefix"; fi
  else
    rm -f "$BIN" "$DESKTOP" "$ICON"
    if has update-desktop-database; then update-desktop-database "$DATA_DIR/applications" 2>/dev/null || true; fi
  fi
  remove_git_config
  echo "Done. The Rust toolchain was left installed (remove it with: rustup self uninstall)."
  exit 0
fi

# --- System build tools ------------------------------------------------------

# Run a command as root (directly when already root, e.g. in a container).
as_root() {
  if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi
}

# Install distribution packages: build tools (C linker, curl) and the
# graphics/keyboard libraries the window needs at run time.
linux_packages() {
  if has apt-get; then
    as_root apt-get update -qq
    as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential curl ca-certificates \
      libxkbcommon0 libxkbcommon-x11-0 libegl1 libgl1 libwayland-client0 libwayland-egl1 \
      libx11-xcb1 libxcursor1 libxrandr2 libxi6
  elif has dnf; then
    as_root dnf install -y gcc curl \
      libxkbcommon libxkbcommon-x11 mesa-libEGL mesa-libGL libwayland-client libwayland-egl \
      libXcursor libXrandr libXi
  elif has pacman; then
    as_root pacman -S --needed --noconfirm base-devel curl \
      libxkbcommon libxkbcommon-x11 mesa wayland libxcursor libxrandr libxi
  elif has zypper; then
    as_root zypper --non-interactive install gcc curl \
      libxkbcommon0 libxkbcommon-x11-0 Mesa-libEGL1 Mesa-libGL1 libwayland-client0 libwayland-egl1 \
      libXcursor1 libXrandr2 libXi6
  else
    echo "Unknown package manager. Install a C compiler (gcc), curl, libxkbcommon,"
    echo "libxkbcommon-x11, Mesa EGL/GL and the Wayland client library, then run ./setup.sh again."
    exit 1
  fi
}

# True when a shared library is installed (checked by soname).
has_lib() {
  { ldconfig -p 2>/dev/null || /sbin/ldconfig -p 2>/dev/null; } | grep -q "$1"
}

if [ "$OS" = mac ]; then
  step "Checking the Xcode Command Line Tools"
  if ! xcode-select -p >/dev/null 2>&1; then
    xcode-select --install || true
    echo "A window asks to install the Command Line Tools. When it finishes, run ./setup.sh again."
    exit 1
  fi
  echo "Found at $(xcode-select -p)"
else
  step "Checking build tools and libraries"
  if ! has cc || ! has curl || ! has_lib libxkbcommon-x11.so.0 || ! has_lib libEGL.so.1; then
    echo "Installing missing packages (this may ask for your password)."
    linux_packages
  else
    echo "All present"
  fi
fi

# --- Rust --------------------------------------------------------------------

step "Checking Rust"
if [ -f "$CARGO_DIR/env" ]; then . "$CARGO_DIR/env"; fi
if ! has cargo; then
  echo "Rust is not installed; installing it with rustup (user-local, in $CARGO_DIR)."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  . "$CARGO_DIR/env"
fi
minor=$(rustc --version | sed -E 's/^rustc 1\.([0-9]+).*/\1/')
if [ "$minor" -lt "$MIN_RUST" ]; then
  if has rustup; then
    echo "Rust 1.$minor is too old; updating."
    rustup update stable
  else
    echo "mergefix needs Rust 1.$MIN_RUST or newer, found 1.$minor. Please update Rust."
    exit 1
  fi
fi
rustc --version

# --- Build and install -------------------------------------------------------

if [ "$OS" = mac ]; then
  step "Building and installing mergefix.app (the first build takes a minute or two)"
  ./macos/bundle.sh --install
else
  step "Building mergefix (the first build takes a minute or two)"
  cargo build --release

  step "Installing"
  mkdir -p "$(dirname "$BIN")" "$(dirname "$DESKTOP")" "$(dirname "$ICON")"
  # Replace rather than overwrite, so a running mergefix keeps working.
  cp target/release/mergefix "$BIN.new"
  chmod 755 "$BIN.new"
  mv -f "$BIN.new" "$BIN"
  cp assets/icon-256.png "$ICON"
  cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=mergefix
GenericName=Merge Conflict Resolver
Comment=Resolve Git merge conflicts, even in huge files
Exec="$BIN" %f
Icon=mergefix
Terminal=false
Categories=Development;Utility;TextEditor;
MimeType=text/plain;application/octet-stream;
StartupWMClass=mergefix
EOF
  if has update-desktop-database; then update-desktop-database "$DATA_DIR/applications" 2>/dev/null || true; fi
  if has gtk-update-icon-cache; then gtk-update-icon-cache -q -t "$DATA_DIR/icons/hicolor" 2>/dev/null || true; fi
  echo "Installed $BIN"
  echo "Installed $DESKTOP"
fi

if [ "$GIT" = 1 ]; then
  step "Setting mergefix as your git mergetool"
  if ! has git; then echo "git was not found on PATH."; exit 1; fi
  git config --global mergetool.mergefix.cmd "\"$BIN\" \"\$MERGED\""
  git config --global mergetool.mergefix.trustExitCode true
  git config --global merge.tool mergefix
  echo "Run 'git mergetool' during a merge to open each conflicted file."
fi

step "Done"
if [ "$OS" = mac ]; then
  echo "Open mergefix from Spotlight or Launchpad, or run: mergefix <file>"
  PATH_DIR="$CARGO_DIR/bin"
else
  echo "Open mergefix from your applications menu, use Open With in your file manager,"
  echo "or run: mergefix <file>"
  PATH_DIR="$(dirname "$BIN")"
fi
case ":$PATH:" in
  *":$PATH_DIR:"*) ;;
  *) echo "Note: $PATH_DIR is not on your PATH yet. Open a new terminal, or add it in your shell profile." ;;
esac
