#!/bin/bash
# Builds the Abacus iOS app archive and IPA using xcodebuild.
# Requires macOS with Xcode 15+ installed.
set -e
cd "$(dirname "$0")"

mkdir -p build
cp ../src/mobile/app.html Abacus/app.html

echo "=== Building Abacus for iOS ==="

xcodebuild -project Abacus.xcodeproj \
  -scheme Abacus \
  -configuration Release \
  -sdk iphoneos \
  -archivePath build/Abacus.xcarchive \
  archive \
  CODE_SIGN_IDENTITY="" \
  CODE_SIGNING_REQUIRED=NO \
  CODE_SIGNING_ALLOWED=NO

echo "=== Packaging unsigned IPA ==="
rm -rf build/Payload build/Abacus.ipa
mkdir -p build/Payload
cp -R build/Abacus.xcarchive/Products/Applications/Abacus.app build/Payload/
cd build
zip -qr Abacus.ipa Payload
rm -rf Payload
echo "Build complete: $(pwd)/Abacus.ipa"
