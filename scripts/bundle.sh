#!/bin/sh
# Assemble target/companion.app — the primary artifact.
#
# The companion is a menu-bar agent: LSUIElement keeps it out of the Dock
# and the app switcher, so the only UI is the status-bar icon. The plain
# `target/release/companion` binary still runs from a terminal for
# debugging, but it has nowhere to hang an icon.
#
# SIGNING: macOS keys Input Monitoring and Accessibility grants to code
# identity, so the signature decides whether permissions survive a
# rebuild. With a real certificate the designated requirement is based
# on the team and bundle id, and grants persist. Ad-hoc (`--sign -`)
# embeds the binary's cdhash instead, so every rebuild looks like a new
# app and re-prompts for both permissions.
#
# Identity is picked automatically: Developer ID Application, else Apple
# Development, else ad-hoc. Override with SIGN_IDENTITY.
#
# Hardened runtime is opt-in (HARDENED=1). It's required for
# notarization, but it also strips get-task-allow, which blocks lldb
# from attaching — not what you want day to day. TCC stability does not
# depend on it.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
APP="$ROOT/target/companion.app"
BUNDLE_ID=${BUNDLE_ID:-net.guemez.trackpad-companion}

if [ -z "${SIGN_IDENTITY:-}" ]; then
	SIGN_IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
		| awk -F'"' '/Developer ID Application/ { print $2; exit }')
fi
if [ -z "$SIGN_IDENTITY" ]; then
	SIGN_IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
		| awk -F'"' '/Apple Development/ { print $2; exit }')
fi
SIGN_IDENTITY=${SIGN_IDENTITY:--}
VERSION=$(awk -F'"' '/^version *=/ { print $2; exit }' "$ROOT/Cargo.toml")

cargo build --release --manifest-path "$ROOT/Cargo.toml"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/target/release/companion" "$APP/Contents/MacOS/companion"
cp "$ROOT/assets/icons/bridge.icns" "$APP/Contents/Resources/companion.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>            <string>Trackpad Companion</string>
	<key>CFBundleDisplayName</key>     <string>Trackpad Companion</string>
	<key>CFBundleIdentifier</key>      <string>${BUNDLE_ID}</string>
	<key>CFBundleExecutable</key>      <string>companion</string>
	<key>CFBundleIconFile</key>        <string>companion</string>
	<key>CFBundlePackageType</key>     <string>APPL</string>
	<key>CFBundleShortVersionString</key> <string>${VERSION}</string>
	<key>CFBundleVersion</key>         <string>${VERSION}</string>
	<key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
	<key>LSMinimumSystemVersion</key>  <string>13.0</string>
	<key>LSUIElement</key>             <true/>
	<key>NSHighResolutionCapable</key> <true/>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist" >/dev/null

# --force so a rebuild replaces the previous signature rather than failing.
if [ "${HARDENED:-0}" = "1" ] && [ "$SIGN_IDENTITY" != "-" ]; then
	codesign --force --sign "$SIGN_IDENTITY" --identifier "$BUNDLE_ID" \
		--options runtime "$APP"
else
	codesign --force --sign "$SIGN_IDENTITY" --identifier "$BUNDLE_ID" "$APP"
fi
codesign --verify --deep --strict "$APP"

echo "built $APP"
echo "  bundle id : $BUNDLE_ID"
echo "  version   : $VERSION"
echo "  signed by : $SIGN_IDENTITY"
# The designated requirement is what TCC matches on across rebuilds.
codesign -d -r- "$APP" 2>&1 | sed -n 's/^designated => /  requirement: /p'

# Optional install. The bundle in target/ is rebuilt (and rm -rf'd) on
# every run, so a copy under ~/Applications is what you actually launch
# day to day — and TCC grants follow the bundle id, not the path, so
# both copies share one set of permissions.
if [ "${INSTALL:-0}" = "1" ]; then
	INSTALL_DIR=${INSTALL_DIR:-$HOME/Applications}
	DEST="$INSTALL_DIR/Trackpad Companion.app"
	mkdir -p "$INSTALL_DIR"
	rm -rf "$DEST"
	cp -R "$APP" "$DEST"
	codesign --verify --strict "$DEST"
	echo "  installed : $DEST"
fi

echo
echo "run it:  open $APP"
echo "or CLI:  $ROOT/target/release/companion -v"
echo "install: INSTALL=1 $0"
