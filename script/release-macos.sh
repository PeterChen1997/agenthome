#!/usr/bin/env bash
# macOS 发布包：release 构建 → .app（含图标）→ Developer ID 签名（hardened runtime）→ zip
# → 可选公证与装订 → 输出到 dist/。
#
# 用法：script/release-macos.sh [版本号]
#   环境变量：
#     CODESIGN_IDENTITY   Developer ID Application 证书名（必填）
#     ASC_KEY_ID / ASC_ISSUER / ASC_KEY_PATH
#                         App Store Connect API 密钥；密钥文件存在即公证
#   以上可写进 script/release.env（已被 .gitignore 忽略，不进仓库），脚本启动时自动读取。
#     SENTRY_AUTH_TOKEN   dSYM 上传凭据，也可用 sentry-cli 的本地登录配置
#     SENTRY_DSN_AGENTHOME
#                         构建时注入，也可从 ~/.config/sentry-dsns.env 读取
#     NOTARY_PROFILE      备选：notarytool 钥匙串 profile 名（API 密钥不可用时）
#     SKIP_NOTARIZE=1     跳过公证（包能跑，但别的 Mac 首次打开需右键“打开”）
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:-$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')}"
# 个人签名身份不进仓库：放在本地 script/release.env
if [ -f script/release.env ]; then
  # shellcheck disable=SC1091
  . script/release.env
fi
IDENTITY="${CODESIGN_IDENTITY:?请设置 CODESIGN_IDENTITY（或写进 script/release.env）}"
ASC_KEY_ID="${ASC_KEY_ID:-}"
ASC_ISSUER="${ASC_ISSUER:-}"
ASC_KEY_PATH="${ASC_KEY_PATH:-$HOME/.appstoreconnect/private_keys/AuthKey_${ASC_KEY_ID}.p8}"
if [ -n "${SKIP_NOTARIZE:-}" ]; then
  NOTARY=()
elif [ -n "$ASC_KEY_ID" ] && [ -n "$ASC_ISSUER" ] && [ -f "$ASC_KEY_PATH" ]; then
  NOTARY=(--key "$ASC_KEY_PATH" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER")
elif [ -n "${NOTARY_PROFILE:-}" ]; then
  NOTARY=(--keychain-profile "$NOTARY_PROFILE")
else
  NOTARY=()
fi
APP=dist/AgentHome.app
ZIP="dist/AgentHome-${VERSION}-macos-arm64.zip"
APP_BIN="$APP/Contents/MacOS/agenthome-app"
DWARF_DIR=target/release/agenthome-app.dSYM/Contents/Resources/DWARF
ICON_SRC=docs/design/logo/app-icon.png   # 1024，已按 macOS 图标网格留边、圆角外透明（logo 选定 D，2026-11-09）

# 发版包必须同时能上报事件和上传 dSYM，凭据问题在构建前报错。
if ! command -v sentry-cli >/dev/null; then
  echo "错误：请安装 sentry-cli 以校验并上传 dSYM" >&2
  exit 1
fi
if ! sentry-cli info --no-defaults --quiet; then
  echo "错误：Sentry 鉴权失败，请设置 SENTRY_AUTH_TOKEN 或配置 sentry-cli 本地登录" >&2
  exit 1
fi
if [ -z "${SENTRY_DSN_AGENTHOME:-}" ] && [ -f "$HOME/.config/sentry-dsns.env" ]; then
  SENTRY_DSN_AGENTHOME="$(awk '/^SENTRY_DSN_AGENTHOME=/ {sub(/^[^=]*=/, ""); print; exit}' "$HOME/.config/sentry-dsns.env")"
fi
: "${SENTRY_DSN_AGENTHOME:?请设置 SENTRY_DSN_AGENTHOME（或写进 ~/.config/sentry-dsns.env）}"
export SENTRY_DSN_AGENTHOME

echo "== 1/5 release 构建"
cargo build --release -p agenthome-app -p agenthome-cli

# packed release profile 会在 target 下生成 dSYM。仅有符号表时 check 也可能
# 成功，因此额外要求 DWARF 调试信息，确保 Sentry 能解析到源码行号。
DWARF_FILES=("$DWARF_DIR"/*)
DSYM="${DWARF_FILES[0]}"
if [ "${#DWARF_FILES[@]}" -ne 1 ] || [ ! -f "$DSYM" ]; then
  echo "错误：release dSYM 应包含一个 DWARF 文件：$DWARF_DIR" >&2
  exit 1
fi
if ! sentry-cli debug-files check --type dsym --json "$DSYM" | /usr/bin/python3 -c \
  'import json, sys
if "debug" not in json.load(sys.stdin)["features"].split(", "):
    sys.exit("dSYM 缺少 DWARF 调试信息")'; then
  echo "错误：release dSYM 校验失败：$DSYM" >&2
  exit 1
fi

echo "== 2/5 组装 .app"
rm -rf dist && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/agenthome-app "$APP_BIN"
cp target/release/agenthome "$APP/Contents/MacOS/agenthome"   # CLI 随包附带
# 仅 strip 包内副本，在签名前移除调试符号并保留运行时回溯用的函数符号。
xcrun strip -S "$APP_BIN" "$APP/Contents/MacOS/agenthome"
# 许可与第三方声明随包分发（Apache-2.0 第 4 条、字体 OFL、依赖署名）
cp LICENSE NOTICE THIRD_PARTY_LICENSES.md "$APP/Contents/Resources/"
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

# 签名后的 Mach-O 必须与 dSYM UUID 一致。等 Sentry 处理完成才继续打包。
APP_UUID="$(xcrun dwarfdump --uuid "$APP_BIN" | awk '/^UUID:/ && $3 == "(arm64)" {print $2}')"
DSYM_UUID="$(xcrun dwarfdump --uuid "$DSYM" | awk '/^UUID:/ && $3 == "(arm64)" {print $2}')"
if [ -z "$APP_UUID" ] || [ "$APP_UUID" != "$DSYM_UUID" ]; then
  echo "错误：arm64 UUID 不匹配（签名二进制：${APP_UUID:-缺失}，dSYM：${DSYM_UUID:-缺失}）" >&2
  exit 1
fi
echo "== 上传 Sentry dSYM（${DSYM_UUID}）"
if ! sentry-cli debug-files upload --org own-tay --project agenthome --type dsym --include-sources \
  --id "$DSYM_UUID" --require-all --wait "$DSYM"; then
  echo "错误：Sentry dSYM 上传或处理失败，中止打包" >&2
  exit 1
fi

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
