#!/bin/bash
# 标题字体子集：只取 index.html 里 <h2> 和 .cn 用到的字，从 Google Fonts 切出小 woff2 自托管
# （国内访问不依赖 Google）。改了标题文案就重跑：bash site/fonts/subset.sh
# ZCOOL KuaiLe、Nunito 都是 SIL OFL 1.1。
set -euo pipefail
cd "$(dirname "$0")"
UA='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36'
chars=$(python3 - <<'PY'
import re, urllib.parse
s = open('../index.html').read(); body = s[s.index('<body'):]
t = ''.join(re.sub(r'<[^>]+>', '', m.group(0)) for m in re.finditer(r'<h2[^>]*>(.*?)</h2>|<span class="cn"[^>]*>(.*?)</span>', body, re.S))
t += '额度团子开心还行垮脸哭了Dango'
print(urllib.parse.quote(''.join(sorted(set(c for c in t if not c.isspace())))))
PY
)
get() { curl -sS -m 30 -A "$UA" "$1" | grep -o 'https://fonts.gstatic.com[^)]*' | head -1; }
curl -sS -m 30 -o kuaile-sub.woff2 "$(get "https://fonts.googleapis.com/css2?family=ZCOOL+KuaiLe&text=$chars")"
# Nunito：英文版标题要用，切整套可打印 ASCII
ascii=$(python3 -c "import urllib.parse; print(urllib.parse.quote(''.join(chr(c) for c in range(32,127))))")
curl -sS -m 30 -o nunito-dango.woff2 "$(get "https://fonts.googleapis.com/css2?family=Nunito:wght@800&text=$ascii")"
ls -l *.woff2
