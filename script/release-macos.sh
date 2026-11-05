#!/usr/bin/env bash
# macOS 发布包：release 构建 → .app（含图标）→ Developer ID 签名（hardened runtime）→ zip
# → 可选公证与装订 → 输出到 dist/。
#
# 用法：script/release-macos.sh [版本号]
#   环境变量：
#     CODESIGN_IDENTITY   默认 "Developer ID Application: YANG CHEN (R85345VS2J)"
#     NOTARY_PROFILE      notarytool 的钥匙串 profile 名；不设则跳过公证（包能跑，但首次打开需右键“打开”）
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:-$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')}"
IDENTITY="${CODESIGN_IDENTITY:-Developer ID Application: YANG CHEN (R85345VS2J)}"
APP=dist/AgentHome.app
ZIP="dist/AgentHome-${VERSION}-macos-arm64.zip"
ICON_SRC=docs/design/assets/processed/app-icon.png

echo "== 1/5 release 构建"
cargo build --release -p agenthome-app -p agenthome-cli

echo "== 2/5 组装 .app"
rm -rf dist && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/agenthome-app "$APP/Contents/MacOS/agenthome-app"
cp target/release/agenthome "$APP/Contents/MacOS/agenthome"   # CLI 随包附带
# 图标：png → icns
ICONSET=dist/AppIcon.iconset && mkdir -p "$ICONSET"
for s in 16 32 64 128 256 512; do
  sips -z $s $s "$ICON_SRC" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  d=$((s*2)); [ $d -le 1024 ] && sips -z $d $d "$ICON_SRC" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>cn.peterchen.agenthome</string>
  <key>CFBundleName</key><string>AgentHome</string>
  <key>CFBundleDisplayName</key><string>AgentHome</string>
  <key>CFBundleExecutable</key><string>agenthome-app</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST

echo "== 3/5 签名（${IDENTITY}）"
cat > dist/entitlements.plist <<'ENT'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.security.cs.disable-library-validation</key><false/>
</dict></plist>
ENT
codesign --force --options runtime --timestamp --entitlements dist/entitlements.plist \
  --sign "$IDENTITY" "$APP/Contents/MacOS/agenthome"
codesign --force --options runtime --timestamp --entitlements dist/entitlements.plist \
  --sign "$IDENTITY" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
spctl --assess --type execute --verbose=2 "$APP" || echo "（未公证时 spctl 会拒绝，属预期）"

echo "== 4/5 打包 zip"
ditto -c -k --keepParent "$APP" "$ZIP"

if [ -n "${NOTARY_PROFILE:-}" ]; then
  echo "== 5/5 公证（profile: ${NOTARY_PROFILE}）"
  xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$APP"
  rm -f "$ZIP" && ditto -c -k --keepParent "$APP" "$ZIP"
else
  echo "== 5/5 跳过公证（未设置 NOTARY_PROFILE）"
fi

shasum -a 256 "$ZIP" | tee "$ZIP.sha256"
echo "完成：$ZIP"
