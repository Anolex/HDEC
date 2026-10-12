#!/bin/sh
# Builds target/release/HDEC.app. Install: cp -R target/release/HDEC.app /Applications/
set -eu
cd "$(dirname "$0")/../.."
cargo build --release -p hdec-gui
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app=target/release/HDEC.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
cp target/release/hdec "$app/Contents/MacOS/hdec"
cp packaging/macos/HDEC.icns "$app/Contents/Resources/"
# Ad-hoc signature: enough to run on this Mac (downloads would need a real one).
codesign --force --sign - "$app"
echo "Built $app ($version)"
