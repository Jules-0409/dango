#!/bin/bash
# 把 127.0.0.1:8049/feed（手机小组件用的精简额度数据）推到一台服务器上，
# 手机从那里读。launchd 每分钟跑一次；目的地只从环境变量来，不写进仓库：
#   DANGO_FEED_SSH   ssh 目标，例如 user@example.com
#   DANGO_FEED_DIR   服务器上放文件的目录（nginx 静态目录）
# 文件名是 ~/Library/Application Support/dango/phone-feed.secret 里的随机串。
set -euo pipefail

: "${DANGO_FEED_SSH:?DANGO_FEED_SSH 没设}"
: "${DANGO_FEED_DIR:?DANGO_FEED_DIR 没设}"
SECRET_FILE="$HOME/Library/Application Support/dango/phone-feed.secret"
SECRET=$(tr -dc 'a-f0-9' < "$SECRET_FILE")
[ ${#SECRET} -ge 32 ] || { echo "phone-feed.secret 太短或不存在" >&2; exit 1; }

FEED=$(curl -sS --max-time 10 http://127.0.0.1:8049/feed)
# 小件刚起来还没拿到数据时是空的：别拿空快照盖掉服务器上的上一份。
COUNT=$(printf '%s' "$FEED" | /usr/bin/python3 -c 'import json,sys; d=json.load(sys.stdin); print(len(d.get("balls",[])) if d.get("fetchedAt") else 0)')
[ "$COUNT" -gt 0 ] || { echo "feed 还是空的，跳过"; exit 0; }

# 先写临时文件再 mv，手机不会读到半截。
printf '%s' "$FEED" | ssh -o BatchMode=yes -o ConnectTimeout=15 "$DANGO_FEED_SSH" \
  "cat > '$DANGO_FEED_DIR/.$SECRET.tmp' && chmod 644 '$DANGO_FEED_DIR/.$SECRET.tmp' && mv '$DANGO_FEED_DIR/.$SECRET.tmp' '$DANGO_FEED_DIR/$SECRET.json'"
echo "$(date '+%F %T') pushed $COUNT balls"
