#!/usr/bin/env bash
#
# push-update.sh —— 把一份新的 Windows exe 投给装了桌面壳的那台机器（默认 win-host）。
#
# 那边的应用每 2 分钟看一眼 `~\.antigravity-bridge\updates\`：清单里版本比自己高、文件
# sha256 对得上，它就写个帮手脚本、退出，由帮手把自己换成新版本再拉起来（见 app/src/update.rs）。
# 所以这里的活只有两件：把 exe 传过去，最后再传清单 —— 清单最后到，读到的才是完整的一份。
#
# 用法：
#   scripts/push-update.sh --exe path.exe      # 要推的那份 exe（必给：没有 CI 产物可取了）
#   scripts/push-update.sh --host win-host --port 8045
#   BRIDGE_WINDOWS_RESTART=0 scripts/push-update.sh   # 只投递，等它自己到点换（默认投完就重启换上）
#
# 日常换版本一般不用这个脚本：`scripts/build-on-windows.sh` 直接在那台机器上编、当场换掉。
# 这条路留给「exe 在别处编好了」的时候（比如要留一份安装包、或者临时回退到某个版本）。
#
# 版本号默认从 antigravity-bridge/Cargo.toml（workspace 那份真值）抠。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" # antigravity-bridge/（工作区）
REPO="$(cd "$ROOT/.." && pwd)"                         # 仓库根（版本号的真值在它下面）
HOST="${BRIDGE_WINDOWS_HOST:?用法：BRIDGE_WINDOWS_HOST=<主机> scripts/... 或 --host 传参}"
PORT="${BRIDGE_WINDOWS_PORT:-8045}"
EXE=""
VERSION=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --exe) EXE="${2:?--exe 后面要给路径}"; shift 2 ;;
    --version) VERSION="${2:?--version 后面要给版本号}"; shift 2 ;;
    --host) HOST="${2:?--host 后面要给主机名}"; shift 2 ;;
    --port) PORT="${2:?--port 后面要给端口}"; shift 2 ;;
    *) echo "不认识的参数：$1" >&2; exit 2 ;;
  esac
done

if [[ -z "$VERSION" ]]; then
  VERSION="$(awk '
    /^\[workspace\.package\]/ { in_pkg = 1; next }
    /^\[/ { in_pkg = 0 }
    in_pkg && /^version = "/ { gsub(/[ "]/, ""); sub(/^version=/, ""); print; exit }
  ' "$REPO/antigravity-bridge/Cargo.toml")"
fi
[[ -n "$VERSION" ]] || { echo "抠不到版本号（antigravity-bridge/Cargo.toml 的 [workspace.package]）" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [[ -z "$EXE" ]]; then
  echo "没有给 --exe：现在也没有 CI 产物可取了（release.yml 2026-09-19 删了，Actions 只手动跑）。" >&2
  echo "日常换版本用 scripts/build-on-windows.sh（在那台机器上直接编）；" >&2
  echo "这条路是给「exe 在别处编好了」用的：--exe path\\to\\antigravity-app.exe" >&2
  exit 2
fi
[[ -f "$EXE" ]] || { echo "找不到这个 exe：$EXE" >&2; exit 1; }

# 传过去的那份名字里带版本：那边 updates 目录一眼能看出堆的是哪版
STAGED="antigravity-app-${VERSION}.exe"
cp "$EXE" "$TMP/$STAGED"
SHA="$(shasum -a 256 "$TMP/$STAGED" | awk '{print $1}')"
printf '{"version":"%s","file":"%s","sha256":"%s"}\n' "$VERSION" "$STAGED" "$SHA" > "$TMP/pending.json"

# updates 目录：远程那边拿 USERPROFILE 拼（别在脚本里写死用户名）
ssh "$HOST" 'if not exist "%USERPROFILE%\.antigravity-bridge\updates" mkdir "%USERPROFILE%\.antigravity-bridge\updates"'
REMOTE_DIR="$(ssh "$HOST" 'echo %USERPROFILE%' | tr -d '\r' | sed 's|\\|/|g')/.antigravity-bridge/updates"

echo "推到 ${HOST}:${REMOTE_DIR}（版本 ${VERSION}，sha256 ${SHA:0:12}…）"
scp -q "$TMP/$STAGED" "$HOST:$REMOTE_DIR/"
scp -q "$TMP/pending.json" "$HOST:$REMOTE_DIR/"

# 投完就换掉，别等它那两分钟的轮询：桌面壳的更新线程是「启动 20 秒后看一眼、之后每 2 分钟
# 一眼」（app/src/main.rs），所以重启一次就等于把等待压成半分钟。杀掉重起只在用户的交互会话里
# 做（ssh 自己的进程落在 session 0，托盘不会出现），借一次性计划任务 + /IT 回去。
if [[ "${BRIDGE_WINDOWS_RESTART:-1}" != "0" ]]; then
  cat > "$TMP/bridge-restart.cmd" <<'CMD'
@echo off
rem restart the desktop shell so it picks up updates\pending.json within ~20s.
rem ssh sessions land in session 0 (no tray icon), so borrow the user session via a one-shot task.
rem wait with ping, not timeout: timeout dies in a non-interactive session. keep this file ASCII:
rem a chcp switch mid-script makes cmd mis-read the following lines.
setlocal
set "APP=%LOCALAPPDATA%\Antigravity Bridge\antigravity-app.exe"
taskkill /IM antigravity-app.exe /F >nul 2>nul
ping -n 3 127.0.0.1 >nul
schtasks /Create /F /TN BridgeAppRunOnce /TR "\"%APP%\" --port=8045 --autostart" /SC ONCE /ST 23:59 /IT /RU "%USERNAME%"
schtasks /Run /TN BridgeAppRunOnce
ping -n 7 127.0.0.1 >nul
schtasks /Delete /F /TN BridgeAppRunOnce
CMD
  # Windows 的 cmd 读不了纯 LF 的 .cmd（会认错行），落盘时换成 CRLF
  awk '{ printf "%s\r\n", $0 }' "$TMP/bridge-restart.cmd" > "$TMP/bridge-restart.crlf" &&
    mv "$TMP/bridge-restart.crlf" "$TMP/bridge-restart.cmd"
  echo "重启一次，让它立刻换上（约 30 秒）…"
  scp -q "$TMP/bridge-restart.cmd" "$HOST:$REMOTE_DIR/bridge-restart.cmd"
  ssh "$HOST" "\"%USERPROFILE%\\.antigravity-bridge\\updates\\bridge-restart.cmd\"" >/dev/null
  sleep 30
fi

echo -n "那边当前版本："
ssh "$HOST" "curl -s --max-time 5 http://127.0.0.1:$PORT/version" | head -c 240
echo
if [[ "${BRIDGE_WINDOWS_RESTART:-1}" == "0" ]]; then
  echo "（没重启：那边最迟两分钟自己换；想立刻换，就在那台机器托盘上点一下「检查更新」）"
fi
