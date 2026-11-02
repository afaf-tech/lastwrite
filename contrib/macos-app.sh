#!/bin/sh
# Assemble Lastwrite.app from a desktop build, without any extra tooling.
#
# Usage: contrib/macos-app.sh [output-directory]
#   PROFILE=debug contrib/macos-app.sh   # build the debug binary instead
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$root/target/macos}
profile=${PROFILE:-release}
case "$profile" in
    dev|debug) profile=dev; dir=debug ;;
    *) dir=$profile ;;
esac
app="$out/Lastwrite.app"
version=$(awk -F'"' '/^version = /{print $2; exit}' "$root/Cargo.toml")

cd "$root"
cargo build --profile "$profile" --locked --features gui --bin lastwrite-desktop

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/$dir/lastwrite-desktop" "$app/Contents/MacOS/Lastwrite"
cp "$root/icons/icon.icns" "$app/Contents/Resources/icon.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Lastwrite</string>
    <key>CFBundleDisplayName</key><string>Lastwrite</string>
    <key>CFBundleIdentifier</key><string>dev.lastwrite.Lastwrite</string>
    <key>CFBundleExecutable</key><string>Lastwrite</string>
    <key>CFBundleIconFile</key><string>icon.icns</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad-hoc signing. Without any signature a quarantined bundle reads as
# "Lastwrite is damaged and can't be opened" on Apple Silicon; an ad-hoc
# signature gives the ordinary "unidentified developer" prompt instead.
codesign --force --deep --sign - "$app" >/dev/null

echo "$app"
