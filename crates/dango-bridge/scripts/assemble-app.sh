#!/usr/bin/env bash
# 手工组装 macOS .app：这台 Mac 的日常路径，离线、不依赖 tauri-cli（tauri 自己的打包器
# 只在 Windows 发版时用，产物是 NSIS 安装包）。产物在 target/（已 gitignore）：
# AntigravityBridge.app。
#
# 用法：scripts/assemble-app.sh（等价 scripts/appctl build）
# 产物：双击 / `open target/AntigravityBridge.app` 即可常驻托盘；退出走托盘菜单。
# 常驻（登录自启 + 挂了自动拉起）用 scripts/appctl install；默认端口 8050（桥的常驻端口），
# 要和另一份实现并排对拍就 BRIDGE_PORT=8051 scripts/appctl install。
# 注意：端口被占时 app 会在任何窗口出来之前如实退出（端口就是单实例锁），
#       报错在 stderr（Finder 拉起时进统一日志，Console.app 按 app 名过滤可看）。

set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# --offline 是硬约束：这台机器按离线环境维护（依赖全在本地 cargo 缓存里）
cargo build --offline --release -p antigravity-app

APP="target/AntigravityBridge.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp target/release/antigravity-app "$APP/Contents/MacOS/antigravity-app"
cp app/icons/icon.icns "$APP/Contents/Resources/app.icns"

# 版本号只写一次：workspace 的 Cargo.toml（app/core 的 Cargo.toml 都是 version.workspace = true，
# 真值在根文件的 [workspace.package] 里）。Info.plist 里留 __VERSION__ / __BUILD__ 占位符，
# 这里替换 —— 发版改 Cargo.toml 一处，plist 不用再手写。全程本地，不碰网络。
VERSION="$(awk '
  /^\[workspace\.package\]/ { in_pkg = 1; next }
  /^\[/ { in_pkg = 0 }
  in_pkg && /^version = "/ { gsub(/[ "]/, ""); sub(/^version=/, ""); print; exit }
' Cargo.toml)"
if [[ -z "$VERSION" ]]; then
  echo "从 Cargo.toml 抠不到版本号（[workspace.package] version）" >&2
  exit 1
fi
# CFBundleVersion 按 Apple 的规矩得是纯数字：把 0.1.0 折成随版本单调涨的 100
BUILD="$(awk -F. '{ printf "%d", $1 * 10000 + $2 * 100 + $3 }' <<< "$VERSION")"

sed -e "s/__VERSION__/$VERSION/g" -e "s/__BUILD__/$BUILD/g" \
  app/Info.plist > "$APP/Contents/Info.plist"

# ad-hoc 签名：不签名也能跑，但签了才是自洽的 bundle（TCC 提示、图标缓存都更稳）
codesign --force --sign - "$APP"

plutil -lint "$APP/Contents/Info.plist"

echo "组装完成：$APP"
echo "  打开：open $APP      （托盘常驻；退出走托盘菜单）"
echo "  常驻：scripts/appctl install   （登录自启 + 挂了自动拉起 + 面板重启按钮可用）"
