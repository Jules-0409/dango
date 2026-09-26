#!/bin/bash
#
# build-app.sh —— 离线构建桌面壳（app/，antigravity-app）。
#
# 全程 --offline：这个仓库按「不依赖网络也能构建」的口径维护，哪天某个依赖
# 缓存里没有，构建必须当场红掉，而不是悄悄联网下载。
#
# 产物：target/release/antigravity-app（裸可执行文件，不是 .app bundle —— macOS 的 .app
# 由 assemble-app.sh 手工组装；tauri 自带的打包器只在出 Windows 的 NSIS 安装包时用，
# 而且要在那台 Windows 上跑，见 scripts/build-on-windows.sh 的说明）。
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo build --offline --release -p antigravity-app

BIN="$ROOT/target/release/antigravity-app"
echo
echo "构建完成：$BIN"
echo "跑起来（默认端口 8050；要和另一份实现并排对拍就 --port=8051）："
echo "  $BIN --port=8050"
echo "常驻（.app + 登录自启 + 托盘）走：scripts/appctl build && scripts/appctl install"
