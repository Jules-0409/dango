#!/bin/bash
# 打包成能直接下载双击的 Dango.app（Apple Silicon）。
# 用法：bash scripts/bundle-macos.sh        → dist/Dango.app + dist/Dango-<版本>-macos-arm64.zip
# 没有 Apple 开发者证书，只做 ad-hoc 签名；第一次打开要右键「打开」（README 里写了）。
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP=dist/Dango.app
ZIP=dist/Dango-$VERSION-macos-arm64.zip

cargo build --release -p dango-widget
rm -rf "$APP" "$ZIP" && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/dango "$APP/Contents/MacOS/dango"
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Dango</string>
  <key>CFBundleDisplayName</key><string>Dango</string>
  <key>CFBundleIdentifier</key><string>io.github.jules-0409.dango</string>
  <key>CFBundleExecutable</key><string>dango</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>MIT · github.com/Jules-0409/dango</string>
</dict>
</plist>
PLIST
codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict "$APP"
ditto -c -k --keepParent "$APP" "$ZIP"
echo "==> $APP"
echo "==> $ZIP ($(du -h "$ZIP" | cut -f1))"
shasum -a 256 "$ZIP"
