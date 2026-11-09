#!/usr/bin/env bash
# 把 debug 二进制包成最小 .app，供本机开发时以真实 macOS 应用身份启动（有 bundle id、能被系统识别窗口、
# Dock 里有图标）。正式发版走 script/release-macos.sh。
set -euo pipefail
cd "$(dirname "$0")/.."
PROFILE="${1:-debug}"
APP="target/AgentHome.app"
ICON_SRC=docs/design/logo/app-icon.png
ICNS=target/AppIcon.icns
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$PROFILE/agenthome-app" "$APP/Contents/MacOS/agenthome-app"

# 图标只在源图更新后重新生成（iconutil 要几秒）
if [ ! -f "$ICNS" ] || [ "$ICON_SRC" -nt "$ICNS" ]; then
  ICONSET=target/AppIcon.iconset
  rm -rf "$ICONSET" && mkdir -p "$ICONSET"
  for s in 16 32 128 256 512; do
    sips -z $s $s "$ICON_SRC" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
    d=$((s*2)); sips -z $d $d "$ICON_SRC" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$ICNS"
  rm -rf "$ICONSET"
fi
cp "$ICNS" "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>cn.peterchen.agenthome.dev</string>
  <key>CFBundleName</key><string>AgentHome Dev</string>
  <key>CFBundleDisplayName</key><string>AgentHome</string>
  <key>CFBundleExecutable</key><string>agenthome-app</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}-dev</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
# Finder / Dock 会缓存图标；更新修改时间让 LaunchServices 重新读取
touch "$APP"
echo "已生成 ${APP}（${VERSION}-dev）"
