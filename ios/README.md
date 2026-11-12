# Abacus Mobile for iOS

This directory contains the iOS version of **Abacus Mobile**, providing a native WebKit (WKWebView) experience for iPhones and iPads with full support for Abacus Sync, live session pairing, custom appearance themes, and background wallpaper customization.

---

## 📱 Installation Options for iOS

### Option 1: 1-Tap Apple Web Clip Profile (No Mac or Jailbreak Required)
The fastest and cleanest way to run Abacus as a standalone app on your iPhone:
1. Send or open `ios/Abacus.mobileconfig` in Safari on your iPhone/iPad, or host it on your local server.
2. iOS will prompt: **"Profile Downloaded"**.
3. Open **Settings** -> **Profile Downloaded** -> Tap **Install**.
4. The **Abacus** app icon will appear on your Home Screen and launch in full-screen standalone mode with no Safari navigation bars.

---

### Option 2: Sideloading IPA via Sideloadly, AltStore, TrollStore, or Scarlet
1. Locate the pre-packaged IPA at `ios/build/Abacus.ipa`.
2. Connect your iPhone to your PC / Mac.
3. Open **Sideloadly** or **AltStore**, drag and drop `Abacus.ipa`, enter your Apple ID, and click **Start**.
4. On your iPhone: Go to **Settings** -> **General** -> **VPN & Device Management** -> Trust Developer Certificate.
5. Launch Abacus!

---

### Option 3: Build & Run with Xcode (macOS)
1. Ensure you have macOS with Xcode 15+ installed.
2. Open the project:
   ```bash
   open ios/Abacus.xcodeproj
   ```
3. In **Signing & Capabilities**, select your Apple Developer Team.
4. Select your connected iPhone or an iOS Simulator (e.g. *iPhone 15 Pro*).
5. Click **Run** (`Cmd + R`) or run `bash ios/build.sh` to produce a release archive.

---

### Option 4: Automated CI via GitHub Actions
A workflow is configured at `.github/workflows/ios.yml`. Every commit pushed to GitHub automatically triggers a macOS runner that builds and packages `Abacus.ipa`, available for download under the workflow's **Artifacts** tab.
