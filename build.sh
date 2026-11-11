#!/bin/bash
# Build Draft Desk.
#   ./build.sh            release build of the command line `draftdesk`
#   ./build.sh --app      release build of the app, wrapped and signed into "target/Draft Desk.app" (nothing installed)
#   ./build.sh --install  release build of app and command line, both signed with DRAFTDESK_SIGN_ID, installed as
#                         ~/Applications/Draft Desk.app and ~/.local/bin/draftdesk
#   ./build.sh --tests    format check, lints, the feature guard and the tests of every package
#
# Each package is built and tested in its own cargo call: cargo unifies features per call, and built together with the
# app the command line would get the core's `send` feature.
#
# Installing (until the README has its own section): create or pick a code signing identity in Keychain Access
# (a self-signed "Code Signing" certificate is enough), list it with `security find-identity -v -p codesigning`, then
#   DRAFTDESK_SIGN_ID="<name of the identity>" ./build.sh --install
# The built copy in target/ is removed and unregistered: macOS opens apps by bundle id, and with two copies it may
# open the wrong one. Quit Draft Desk first: --install replaces its bundle and refuses while the app runs.
#
# Environment (optional, required for --install):
#   DRAFTDESK_SIGN_ID  code signing identity from your keychain (default: ad hoc). Keychain approvals belong to the
#                      signature: with a named identity they survive rebuilds; ad hoc, macOS asks again after a build.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR"
LSREG=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

if ! command -v cargo >/dev/null; then
  echo "cargo not found. Install Rust (https://rustup.rs, or brew install rust) and the Xcode Command Line Tools" >&2
  echo "(xcode-select --install)." >&2
  exit 1
fi

# Wrapping and installing read the builds from exactly where they land: CARGO_TARGET_DIR and an explicit target (the
# host) beat what the environment or a Cargo config (build.target-dir, build.target) would choose, which could leave an
# older build in the place read from. The target is pinned right before a build, after the checks that need no Rust.
export CARGO_TARGET_DIR="$DIR/target"
pin_target() {
  CARGO_BUILD_TARGET="$(cargo -vV | sed -n 's/^host: //p')"
  export CARGO_BUILD_TARGET
  REL="$CARGO_TARGET_DIR/$CARGO_BUILD_TARGET/release"
}

# The bundle id lives once, in the core (the command line wakes the app with it).
bundle_id() {
  local bid
  bid="$(sed -n 's/^pub const APP_BUNDLE_ID: &str = "\(.*\)";$/\1/p' core/src/lib.rs)"
  if [ -z "$bid" ]; then
    echo "APP_BUNDLE_ID not found in core/src/lib.rs" >&2
    exit 1
  fi
  echo "$bid"
}

# app_bundle: wrap the release build of draftdesk-app into a signed "target/Draft Desk.app" (as Contents/MacOS/DraftDesk).
app_bundle() {
  local version bid app
  version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
  bid="$(bundle_id)"
  app="$DIR/target/Draft Desk.app"
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/en.lproj" "$app/Contents/Resources/de.lproj"
  cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key><string>en</string>
	<key>CFBundleDisplayName</key><string>Draft Desk</string>
	<key>CFBundleExecutable</key><string>DraftDesk</string>
	<key>CFBundleIconFile</key><string>AppIcon</string>
	<key>CFBundleIdentifier</key><string>$bid</string>
	<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
	<key>CFBundleLocalizations</key><array><string>en</string><string>de</string></array>
	<key>CFBundleName</key><string>Draft Desk</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$version</string>
	<key>CFBundleVersion</key><string>1</string>
	<key>LSMinimumSystemVersion</key><string>26.0</string>
	<key>NSHighResolutionCapable</key><true/>
	<key>NSSupportsSuddenTermination</key><false/>
</dict>
</plist>
PLIST
  for lang in en de; do
    printf '/* Draft Desk */\n' > "$app/Contents/Resources/$lang.lproj/InfoPlist.strings"
  done
  printf 'APPL????' > "$app/Contents/PkgInfo"
  cp "$DIR/macos/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
  cp "$REL/draftdesk-app" "$app/Contents/MacOS/DraftDesk"
  # Clear extended attributes that codesign rejects (macOS 27's xattr has no -r); codesign below is the real check.
  find "$app" -exec xattr -c {} + 2>/dev/null || true
  codesign --force --identifier "$bid" -s "${DRAFTDESK_SIGN_ID:--}" "$app"
  codesign --verify --strict "$app"
  echo "$app"
}

# registered <bundle id>: every bundle Launch Services has registered with this id, one path per line.
registered() {
  "$LSREG" -dump 2>/dev/null | awk -v id="$1" '
    /^-+$/ { path = "" }
    /^path:/ { p = $0; sub(/^path:[ \t]+/, "", p); sub(/ \(0x[0-9a-f]+\)$/, "", p); path = p }
    /^identifier:/ { if ($2 == id && path != "") print path }
  ' | sort -u
}

# refuse_if_running: stops while the installed app runs. Its bundle is replaced: a running app would lose what it still
# loads from there (and a send may be under way). A command line that starts with the app's program, compared as plain
# text: a process that only mentions the path (a shell command, an editor) is not the app, and a home path is no
# regular expression.
refuse_if_running() {
  if ps -Ao command= | awk -v p="$HOME/Applications/Draft Desk.app/Contents/MacOS/" \
    'index($0, p) == 1 { found = 1 } END { exit !found }'; then
    echo "Draft Desk is running: quit Draft Desk, then run --install again." >&2
    exit 2
  fi
}

# install_built: the command line into ~/.local/bin, the app from target/ into ~/Applications, both by rename (a
# running copy keeps its old file), then exactly one registered bundle with the app's id.
install_built() {
  local bid built dest bins cli found
  bid="$(bundle_id)"
  built="$DIR/target/Draft Desk.app"
  dest="$HOME/Applications/Draft Desk.app"
  bins="$HOME/.local/bin"
  mkdir -p "$HOME/Applications" "$bins"
  cli="$bins/.draftdesk.new"
  cp "$REL/draftdesk" "$cli"
  codesign --force --identifier "$bid.cli" -s "$DRAFTDESK_SIGN_ID" "$cli"
  codesign --verify --strict "$cli"
  mv -f "$cli" "$bins/draftdesk"
  # Again right before the bundle moves: the builds take minutes, and the app may have been started meanwhile.
  # ponytail: a start in the moment between this check and the renames still loses its bundle; only a lock the app
  # takes at its start would close that.
  refuse_if_running
  "$LSREG" -u "$built" 2>/dev/null || true
  rm -rf "$dest.new" "$dest.old"
  mv "$built" "$dest.new"
  # The old bundle steps aside before the new one moves in, so a stop in between never leaves no app at all.
  if [ -e "$dest" ]; then mv "$dest" "$dest.old"; fi
  mv "$dest.new" "$dest"
  rm -rf "$dest.old"
  codesign --verify --strict "$dest"
  "$LSREG" -f "$dest"
  echo "$dest"
  echo "$bins/draftdesk"
  found="$(registered "$bid")"
  if [ "$found" != "$dest" ]; then
    echo "warning: Launch Services knows these bundles with the id $bid; macOS may open the wrong one:" >&2
    printf '  %s\n' "$found" >&2
    echo "unregister every other one: $LSREG -u <path>" >&2
  fi
}

case "${1:-}" in
  "")
    pin_target
    cargo build --release -p draftdesk-cli
    ;;
  --app)
    pin_target
    cargo build --release -p draftdesk-macos
    app_bundle
    ;;
  --install)
    # A named identity, never ad hoc: with ad hoc signatures macOS asks for keychain access again after every build,
    # and calls from scripts block on that question.
    if [ -z "${DRAFTDESK_SIGN_ID:-}" ] || [ "$DRAFTDESK_SIGN_ID" = "-" ]; then
      echo "--install needs DRAFTDESK_SIGN_ID: a code signing identity from your keychain" >&2
      echo "(security find-identity -v -p codesigning). Ad hoc, the keychain asks again after every build." >&2
      exit 2
    fi
    # Before the builds too: they take minutes, wasted if the app runs.
    refuse_if_running
    pin_target
    cargo build --release -p draftdesk-cli
    cargo build --release -p draftdesk-macos
    app_bundle >/dev/null
    install_built
    ;;
  --tests)
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy -p draftdesk-core --all-targets -- -D warnings
    cargo clippy -p draftdesk-core --all-targets --features send -- -D warnings
    # The command line must never be able to send: its feature tree may not contain the core's `send` feature.
    # Read the whole tree first (set -e stops on a cargo error); piped into `grep -q`, an early match could end
    # in SIGPIPE, which pipefail reports as failure, and the guard would pass silently.
    tree="$(cargo tree -p draftdesk-cli -e features --prefix none)"
    if [[ "$tree" == *'draftdesk-core feature "send"'* ]]; then
      echo "draftdesk-cli enables the core's send feature" >&2
      exit 1
    fi
    cargo test -p draftdesk-core
    cargo test -p draftdesk-core --features send
    cargo test -p draftdesk-cli
    cargo test -p draftdesk-macos
    ;;
  *) echo "Unknown option: $1 (allowed: --app, --install, --tests)" >&2; exit 2 ;;
esac
