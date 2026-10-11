#!/bin/sh
# Build Nightcap.app in target/release/bundle.
#   --install    also copy it to /Applications
#   --universal  build one binary for Apple Silicon and Intel (used for releases)
#   --dmg        also build target/release/bundle/Nightcap.dmg
set -eu

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
bundle=target/release/bundle
app=$bundle/Nightcap.app

install=false universal=false dmg=false
for arg in "$@"; do
    case "$arg" in
        --install) install=true ;;
        --universal) universal=true ;;
        --dmg) dmg=true ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

if $universal; then
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
    binary=target/release/nightcap-universal
    lipo -create -output "$binary" \
        target/aarch64-apple-darwin/release/nightcap \
        target/x86_64-apple-darwin/release/nightcap
else
    cargo build --release
    binary=target/release/nightcap
fi

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/nightcap"

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
trap 'exit 1' HUP INT TERM
iconset=$temporary/AppIcon.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z $size $size assets/AppIcon.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z $double $double assets/AppIcon.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Nightcap</string>
    <key>CFBundleDisplayName</key><string>Nightcap</string>
    <key>CFBundleIdentifier</key><string>io.github.ryankegerreis.nightcap</string>
    <key>CFBundleExecutable</key><string>nightcap</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSAppleEventsUsageDescription</key><string>Nightcap asks System Events to shut down the computer when the timer ends.</string>
</dict>
</plist>
PLIST

codesign --force --sign - "$app"
echo "Built $app"

if $dmg; then
    staging=$temporary/dmg
    mkdir -p "$staging"
    cp -R "$app" "$staging/Nightcap.app"
    ln -s /Applications "$staging/Applications"
    rm -f "$bundle/Nightcap.dmg"
    hdiutil create -volname Nightcap -srcfolder "$staging" -format UDZO -ov "$bundle/Nightcap.dmg" >/dev/null
    echo "Built $bundle/Nightcap.dmg"
fi

if $install; then
    pkill -x nightcap 2>/dev/null || true
    rm -rf /Applications/Nightcap.app
    cp -R "$app" /Applications/Nightcap.app
    echo "Installed /Applications/Nightcap.app"
fi
