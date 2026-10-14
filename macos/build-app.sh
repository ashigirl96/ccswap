#!/bin/sh
# Usage: macos/build-app.sh <ccswap binary> <version> <output dir>
# Wraps the binary and the widget extension into <output dir>/ccswap.app.
set -eu

binary=$1
version=$2
out=$3
here=$(cd "$(dirname "$0")" && pwd)

(cd "$here" && xcodegen generate --quiet)
xcodebuild -project "$here/CcswapWidget.xcodeproj" -target CcswapWidget \
    -configuration Release SYMROOT="$here/build" CCSWAP_VERSION="$version" \
    build >"$here/build.log" 2>&1 || { tail -40 "$here/build.log"; exit 1; }

app="$out/ccswap.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/PlugIns"
cp "$binary" "$app/Contents/MacOS/ccswap"
cp -R "$here/build/Release/CcswapWidget.appex" "$app/Contents/PlugIns/"
sed "s/@VERSION@/$version/g" "$here/Info.plist.in" >"$app/Contents/Info.plist"

# The extension keeps the sandbox entitlements xcodebuild signed it with;
# signing the bundle after it seals the extension in.
codesign --force --sign - "$app"
codesign --verify --strict "$app"
