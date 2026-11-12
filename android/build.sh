#!/bin/bash
# Builds the phone app without Gradle: aapt2, javac, d8, zipalign, apksigner.
# Needs ANDROID_HOME with build-tools 34 and platform 34, plus a JDK 17+.
# The phone page comes from the Rust source, copied into assets first.
set -e
cd "$(dirname "$0")"
SDK=${ANDROID_HOME:-/opt/android-sdk}
BT=$SDK/build-tools/34.0.0
PLATFORM=$SDK/platforms/android-34/android.jar

mkdir -p assets
cp ../src/mobile/app.html assets/app.html
rm -rf build
mkdir -p build/gen build/classes build/dex

$BT/aapt2 compile --dir res -o build/res.zip
$BT/aapt2 link -o build/unsigned.apk -I $PLATFORM --manifest AndroidManifest.xml -A assets \
  --java build/gen --min-sdk-version 26 --target-sdk-version 34 \
  --version-code 1 --version-name 0.7.6 build/res.zip

javac -nowarn -source 17 -target 17 -classpath $PLATFORM -d build/classes \
  $(find src -name '*.java') $(find build/gen -name 'R.java')

# d8 wants a jar, not a directory of class files
jar cf build/classes.jar -C build/classes .
$BT/d8 --lib $PLATFORM --min-api 26 --output build/dex build/classes.jar

cd build
cp unsigned.apk unaligned.apk
zip -q -j unaligned.apk dex/classes.dex
$BT/zipalign -f 4 unaligned.apk aligned.apk

# A local debug key: the same one every build, so `adb install -r` upgrades
# in place instead of demanding an uninstall.
[ -f ../app.keystore ] || keytool -genkeypair -keystore ../app.keystore -alias abacus \
  -storepass android -keypass android -dname "CN=Abacus Mobile" \
  -keyalg RSA -keysize 2048 -validity 10000
$BT/apksigner sign --ks ../app.keystore --ks-pass pass:android --key-pass pass:android \
  --out abacus.apk aligned.apk
$BT/apksigner verify abacus.apk && echo "signed: $(pwd)/abacus.apk"
