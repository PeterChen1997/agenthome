#!/usr/bin/env bash
# 把 debug 二进制包成最小 .app，供本机开发时以真实 macOS 应用身份启动（有 bundle id、能被系统识别窗口）。
# 正式打包走 cargo-packager，见 docs/01-产品与架构方案.md §6。
set -euo pipefail
cd "$(dirname "$0")/.."
PROFILE="${1:-debug}"
APP="target/AgentHome.app"
mkdir -p "$APP/Contents/MacOS"
cp "target/$PROFILE/agenthome-app" "$APP/Contents/MacOS/agenthome-app"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>cn.peterchen.agenthome</string>
  <key>CFBundleName</key><string>AgentHome</string>
  <key>CFBundleExecutable</key><string>agenthome-app</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.0.1</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
echo "已生成 $APP"
