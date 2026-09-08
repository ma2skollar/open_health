# Shipping OuraApp to TestFlight

The simulator dev harness (`OuraApp/build_run.sh`) is for quick local runs. TestFlight
needs a **signed device archive**. Everything below the signing step is scaffolded;
signing requires *your* Apple Developer account.

## One-time
- **Apple Developer Program** membership ($99/yr).
- Register the App ID **`md.thomas.openoura`** and create the app in App Store Connect.
- Install xcodegen: `brew install xcodegen`.

## Build & upload
Current upload version is configured as **0.1.1 (22)** in
`OuraApp/project.yml` and `OuraApp/project-ci.yml`.

```bash
# 1. shared Rust core → both device + simulator slices
./apps/ios/build-xcframework.sh

# 2. rebuild the activity model's mobile export (requires Python with torch 2.9)
#    fixes sparse-day peak detection; .pt/.ptl files are local, gitignored inputs
python tools/export_mobile.py automatic_activity_detection_3_1_11

# 3. generate the Xcode project from project.yml
cd apps/ios/OuraApp && xcodegen generate

# 4. open it, set your Team under Signing & Capabilities (or DEVELOPMENT_TEAM in project.yml)
open OuraApp.xcodeproj
#    then: Product → Archive → Distribute App → TestFlight & App Store
```
Or headless once a Team is set:
```bash
xcodebuild -project OuraApp.xcodeproj -scheme OuraApp -sdk iphoneos \
  -configuration Release archive -archivePath build/OuraApp.xcarchive
xcodebuild -exportArchive -archivePath build/OuraApp.xcarchive \
  -exportOptionsPlist ExportOptions.plist -exportPath build/export   # then upload with `xcrun altool`/Transporter
```

## Activity model regression checks

After changing the activity export, run `python -m unittest discover -s tools -p
test_mobile_activity.py` from the repository root. This checks sparse-day behavior
and exact full-model output parity through an export/reload. The TORCH-enabled
`StabilityTests` also exercise sparse days and a known workout in the iOS lite
runtime. Re-export before archiving: updating Swift alone does not replace an
older local `.ptl` file.

## Already handled
- App icon (`Assets.xcassets/AppIcon.appiconset`, 1024²).
- `Info.plist`: Bluetooth usage strings; `ITSAppUsesNonExemptEncryption=false` (AES ring
  auth is exempt); simulator platform pin removed so a device archive is valid.
- Device (`ios-arm64`) **and** simulator slices in `OuraCore.xcframework`.
- Both sim and device Release builds verified to compile + link.

## Still on you
- **Signing**: Team ID + a distribution provisioning profile (only you can do this).
- **Version bumps**: `MARKETING_VERSION` / `CURRENT_PROJECT_VERSION` in `project.yml`.
- **Data**: the local `project.yml` build bundles `oura.db` when that gitignored file is
  present, which is useful for a personal TestFlight. The Xcode Cloud `project-ci.yml`
  build does not bundle `oura.db`, `.ptl` models, or LibTorch.
