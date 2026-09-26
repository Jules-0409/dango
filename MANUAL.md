# Dango 使用手册

> 一条住在屏幕边上的原生 macOS 小胶囊：每颗表情球是一家 AI 套餐的额度，
> 球越开心额度越足。悬停出详情卡，点底部小条收成一颗小胶囊。

## 它长什么样

- **常态**：一条竖直细胶囊（62pt 宽），上下排一排表情球，一颗 = 一家套餐。
  球外一圈细环是当前最紧张那个额度窗口的剩余百分比。
- **悬停**：鼠标压上某颗球，其余球变暗聚焦，旁边展开一张详情卡
  （mini 球 + 大字百分比 + 每个额度 bucket 的进度条 + 重置倒计时 +
  该套餐名下的反代端点 + 数据新鲜度）。
- **折叠**：胶囊底部有一条小灰条，点一下整条胶囊收成一颗小胶囊，
  只剩那条白条；再点一下展开。折叠状态会记住，重启后保持。
- **拖拽**：按住胶囊任意位置拖动，松手自动落盘记住位置。
  点击和拖拽按位移区分（<4pt 才算点击），拖动不会误触别的操作。

## 球和环怎么读

| 视觉 | 含义 |
|---|---|
| 开心眼 | 剩余 ≥ 60% |
| 普通眼 | 15% – 60% |
| 闭眼失落 | < 15%，环末端会有红色警示点 |
| 红色虚线环 | 这家抓数失败了（环上的虚线在走，错误如实报） |
| 环颜色 | 套餐色 = 健康；橙 = 偏低；红 = 见底 |
| 无环弧 | 该家没返回百分比数据 |

悬停进场时五颗环会从上往下逐颗"彗星绕一圈"描进来；切球单颗重描。

## 详情卡

- **钉住**：点一下球卡片钉住，鼠标走开也不消失；再点取消。
  钉住的卡会跟着胶囊拖动。
- **反代行**：只有这家套餐名下有本地反代端点时才出现，
  显示可用账号/延迟，点击行打开设置页对应反代页签。
- **新鲜度**：底栏显示"N 秒前更新"；超过 10 分钟没刷成会降淡提醒。

## 设置窗口

托盘图标 → 「设置…」，或点详情卡的反代行。

### 小球页签

- 拖拽手柄排序（顺序同步到胶囊）。
- 每颗球可换形态（Blob / Gem / Wedge / Cloud / Drop 等 8 种）和颜色。
- **凭据区**（页签底部）：小球一般自己读应用登录态；读不到时可以给
  Devin / Factory 手动粘一个 token——存进 macOS 钥匙串的
  `dango` 专区（跟应用自己的凭据完全隔离），手动槽优先、
  清除后回到自动读取。保存即触发刷新，不用等一分钟。
- 挂了的小店行内带「怎么修」指引 + 跳转到凭据区的快捷链接。

### Token 记录页签

各工具在自己地盘留下的真实 token 账，只读本机，不联网：今天/区间/日均、
每日堆叠柱、按模型表、「钱花在哪」按付费方汇总、缓存命中率。

- Claude Code：`~/.claude/projects/**/*.jsonl`
- Factory：会话 `settings.json` 的累计 `tokenUsage` 增长
- Devin：本机会话库 `~/.local/share/devin/cli/sessions.db` 里
  每轮推理自带的 `metadata.metrics`（Devin 的接口只给额度 %，
  token 是它自己记账——桌面端和 CLI 的会话都进这个库）
- Cursor：官方用量明细（5 分钟同步）
- Gemini 桥：本地反代转发的请求日志

### 偏好

- **外观主题**：跟随系统 / 深色 / 浅色。胶囊玻璃、环轨道、
  详情卡、设置页全部跟着换（跟随系统时一个刷新周期内生效）。
- **性能模式**：流畅 ≈ 60fps / 均衡 ≈ 30fps / 省电 = 平时静止悬停才动。

### 反代页签（Gemini）

- 顶部状态条：监听状态、上游最近 HTTP 码、今日/总请求数、在途、
  错误数、内存占用。
- 接入配置：Base URL + `.env` 片段一键复制。
- 模型列表（可搜索）、账号池健康、最近 50 条请求。
- **「+ 添加账号」（仅 Gemini）**：点一下弹浏览器走 Google 授权，
  回来账号自动进池子，不用重启反代、不用手碰 JSON。授权窗口 120 秒，
  超时可以再点一次。账号文件落 `~/.antigravity-bridge/accounts/`（0600）。
  命令行等价物：`dango-bridge login`。

## 接账号指引

| 套餐 | 自动读取来源 | 读不到时 |
|---|---|---|
| Gemini | 反代桥账号池 | 「Gemini 反代」页 → **+ 添加账号**，浏览器点一下授权就进池 |
| Devin | Devin CLI `credentials.toml` / Desktop `state.vscdb` | 重登 Devin CLI；或手动粘 CLI token |
| Cursor | Cursor `state.vscdb` | 在 Cursor App 里重新登录 |
| Factory | macOS 钥匙串 / Windows 凭据库 | 在 Factory App 登录；或手动粘 access token |

**凭据红线**：任何 token 只进 macOS 钥匙串，不落盘、不进日志、不进 commit；
vendor 的钥匙串项永远只读，永不自动 refresh（有的 vendor 会轮换整对凭据，
refresh 会把官方 App 挤下线）。

## 排障

| 现象 | 原因 / 动作 |
|---|---|
| 某球红虚线环 + `http 401` | 登录态过期 → 照上表重新登录或粘凭据 |
| `keychain: xxx` | 那家凭据没在本机找到 → 确认装过并登录过对应 App |
| `超时` / `timeout` | 网络问题 → 检查代理 / 网络后点「立即刷新」 |
| 球不更新 | 数据如实报，不拿缓存冒充；看详情卡底栏的"N 秒前更新" |
| 胶囊不见了 | 拖到屏幕边缘去了？设置页恢复默认，或删 `~/Library/Application Support/dango/window.json` 后重启 |

## 常用命令

```bash
# 构建 / 测试
cargo build --workspace --release
cargo test --workspace

# 重启常驻服务
launchctl kickstart -k gui/$(id -u)/<你的 launchd 标签>

# 控制 API（localhost:8049）
curl http://127.0.0.1:8049/snapshot          # 当前快照
curl -X POST http://127.0.0.1:8049/refresh   # 立即刷新
curl http://127.0.0.1:8049/settings          # 设置
curl http://127.0.0.1:8049/credentials       # 手动凭据占用情况

# 开发调试
cargo run -p dango-widget -- --debug-card=cursor        # 启动即钉住详情卡（截图用）
cargo run -p dango-widget -- --open-settings=balls    # 启动即开设置窗
cargo run -p dango-widget --example card_preview --release -- /tmp/card_preview
                                                     # 离屏渲染详情卡 PNG
```

## 架构一句话

单进程：winit + Core Animation 画胶囊/卡片（`dango-widget`），
控制 API（:8049，设置窗口/Web 页面/SSE）同进程内嵌，
`dango-lib` 管设置/钥匙串/探针，`dango-bridge` 管 Gemini 账号池（:8050），
`grok-ball` 是表情球的 Rust 移植。
