#!/usr/bin/env bash
# macOS 发布包：release 构建 → .app（含图标）→ Developer ID 签名（hardened runtime）→ zip
# → 可选公证与装订 → 输出到 dist/。
#
# 用法：script/release-macos.sh [版本号]
#   环境变量：
#     CODESIGN_IDENTITY   默认 "Developer ID Application: YANG CHEN (R85345VS2J)"
#     ASC_KEY_ID / ASC_ISSUER / ASC_KEY_PATH
#                         App Store Connect API 密钥（与 Twnkle 桌面端共用同一把）；密钥文件存在即公证
#     NOTARY_PROFILE      备选：notarytool 钥匙串 profile 名（API 密钥不可用时）
#     SKIP_NOTARIZE=1     跳过公证（包能跑，但别的 Mac 首次打开需右键“打开”）
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:-$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')}"
IDENTITY="${CODESIGN_IDENTITY:-Developer ID Application: YANG CHEN (R85345VS2J)}"
# 密钥 ID 与 Issuer ID 不是机密（机密的是 .p8 文件本身，只按路径引用）；取值同 tick-me/desktop/scripts/release.sh
ASC_KEY_ID="${ASC_KEY_ID:-68W68734N8}"
ASC_ISSUER="${ASC_ISSUER:-20201e41-a5f0-43d7-a531-827f80cc98ee}"
ASC_KEY_PATH="${ASC_KEY_PATH:-$HOME/.appstoreconnect/private_keys/AuthKey_${ASC_KEY_ID}.p8}"
if [ -n "${SKIP_NOTARIZE:-}" ]; then
  NOTARY=()
elif [ -f "$ASC_KEY_PATH" ]; then
  NOTARY=(--key "$ASC_KEY_PATH" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER")
elif [ -n "${NOTARY_PROFILE:-}" ]; then
  NOTARY=(--keychain-profile "$NOTARY_PROFILE")
else
  NOTARY=()
fi
APP=dist/AgentHome.app
ZIP="dist/AgentHome-${VERSION}-macos-arm64.zip"
ICON_SRC=docs/design/logo/app-icon.png   # 1024，已按 macOS 图标网格留边、圆角外透明（logo 选定 D，2026-11-09）

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

if [ ${#NOTARY[@]} -gt 0 ]; then
  echo "== 5/5 公证"
  # notarytool --wait 在 Invalid 时退出码也可能是 0，必须看 status；不通过就拉日志并失败
  OUT="$(xcrun notarytool submit "$ZIP" "${NOTARY[@]}" --wait --output-format json)"
  STATUS="$(printf '%s' "$OUT" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))')"
  SUBMISSION="$(printf '%s' "$OUT" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')"
  echo "公证结果：${STATUS}（${SUBMISSION}）"
  if [ "$STATUS" != "Accepted" ]; then
    xcrun notarytool log "$SUBMISSION" "${NOTARY[@]}" >&2 || true
    echo "公证未通过" >&2
    exit 1
  fi
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
  spctl --assess --type execute --verbose=2 "$APP"
  rm -f "$ZIP" && ditto -c -k --keepParent "$APP" "$ZIP"
else
  echo "== 5/5 跳过公证（未找到 API 密钥 / NOTARY_PROFILE，或设置了 SKIP_NOTARIZE）"
fi

# 校验和文件只写文件名，下载者在同一目录下可直接 `shasum -a 256 -c`
(cd "$(dirname "$ZIP")" && shasum -a 256 "$(basename "$ZIP")") | tee "$ZIP.sha256"
echo "完成：$ZIP"
