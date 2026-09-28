# AGENTS.md — Dango

**档位**：正式（桌面常驻件）。开源项目，凭据纪律按开源标准执行：
token/keychain 值不落盘、不进日志、不进 commit。

## 是什么

Cargo workspace，一次 `cargo build --workspace` 全编。原生（winit + Core
Animation）桌面小件：常态是一条细胶囊，上面一排 Grok-ball 表情球 = 各套餐
额度；悬停某颗球向下展开详情卡（额度 bucket + 该套餐名下的反代端点）。
同进程 8049 控制接口（设置页 / 快照 / SSE）。

## 红线（本项目特有）

- **不 refresh 任何 token**。有些 vendor 会轮换整对凭据，refresh 会挤掉
  官方 app；一律只读。
- **错误如实报**，不拿缓存数字冒充。provider 挂了 → `ok=false`，界面哭哭。
- 接口都是逆向来的非公开端点，厂商随时会改；解析失败时显示错误而不是编数。
- 个别端点拦默认 UA（Cloudflare）；那种请求在自己的头里显式覆盖 UA，
  其余一律走 `probes::http::USER_AGENT`。
- UI 走「芭乐玻璃」设计语言：饱和度预算给小球/环/状态点，
  玻璃只有胶囊那一件（详情卡半透明 + blur）；粉色调留给用户操作和选中态。

## 结构

```
crates/
  dango-widget/  原生小件进程（winit + Core Animation）；
                 8049 控制接口 + 原生设置窗口（WKWebView 载入 8049 的设置页）
  dango-lib/     models.rs(camelCase 契约) + manual_creds + probes/(vendor 探针)
                 + providers/{claude,antigravity,custom,probes_adapter}.rs
                 + proxies.rs(反代 healthz，PLAN_PROXIES 按 plan id 挂端点)
                 + tokens.rs(本地 token 账本)
  dango-bridge/  Gemini/Antigravity 账号池桥（headless bin，8050）。
                 一键加号：POST /control/login 起 OAuth 回环（login.rs），
                 落库后 pool.rescan_accounts() 吃进，不重启
  cursor-bridge/ Cursor CLI 包装（8052）
  grok-ball/     表情球渲染库（MIT vendor 代码，Rust 移植版）
ui/              settings.html + settings.css + settings.js + common.* + bridge.js
                 （grok-ball.js 是 vendor MIT，别改）
```

反代不是独立列表：`PlanQuota.proxy` 可空，`proxies::PLAN_PROXIES` 按 plan id
把 healthz 端点挂到对应套餐上；没有反代的套餐就是 None，详情卡不显示反代行。

窗口位置写在 `~/Library/Application Support/dango/window.json`
（原子写 + 300ms 防抖，读时按屏幕裁剪），见 `crates/dango-widget/src/window_state.rs`。

## iPhone 小组件（ios/）

数据：小件 `GET 127.0.0.1:8049/feed`（`dango-widget/src/phone_feed.rs`：只有名字/颜色/形状/表情/百分比/重置时间，
错误只给一句人话，不带 token、账号、原始报错）→ `scripts/push-feed.sh` 推到用户自己的服务器（目的地只从环境变量读）
→ 手机读 `<base>/<phone-feed.secret>.json`。密钥和 `ios/Shared/FeedConfig.swift` 都不进 git。

- 工程用 xcodegen：`cd ios && DANGO_FEED_BASE=… bash gen-config.sh && xcodegen generate`。
- App 主界面 `ios/Dango/Web/capsule.html`：WKWebView 跑 `ui/grok-ball.js` + `ui/common.js`，数据由原生层
  `window.dango.setFeed(feed, meta)` 塞进去。改戳球手感时和 `site/index.html` 一起改。
- 小组件不能持续动画；点球走 `PokeIntent` 换表情。形状数据：
  `cargo run -q -p grok-ball --example frame_dump > ios/Shared/BallShapes.swift`（改了形状要重跑）。

## 端口表（固定，不许漂）

| 端口 | 服务 |
|---|---|
| 8049 | 小件控制接口 / 设置页 |
| 8050 | Gemini 账号池桥 |
| 8052 | Cursor Agent 桥（每请求交给 Cursor CLI Agent 跑，token 账本不记它） |

唯一真相在 `dango-lib/src/ports.rs`；各服务只绑自己的端口，被占就报错退出，
绝不另找端口。`ports::tests` 扫全仓 `127.0.0.1:80xx` 字面量，
表外端口直接挂测试。token 账本靠端口认付费方，表外的本机端口一律当
「已停用」，经那里的会话不入账。

token 账本 `dango-lib/src/tokens.rs`（`GET /tokens`）：Claude Code jsonl、
Factory 会话、Devin `~/.local/share/devin/cli/sessions.db` 的
`metadata.metrics`（真·每轮 token，rowid 游标增量读）、Cursor 官方明细、
Gemini 桥日志。SQLite 一律 `open_vscdb_readonly` 只读打开。

## 改动后必做

1. `cargo test --workspace` + `cargo build --workspace --release`
2. 界面改动必须过眼睛：胶囊态只有球、悬停出详情、
   反代行只在有端点的套餐上；设置窗口从托盘「设置…」或详情卡反代行打开
3. 新 provider：加 `providers/xxx.rs` + `mod.rs` 注册 + `probes_adapter` 或
   `data.rs::fetch_snapshot` 聚合 + `crates/dango-widget/src/theme.rs` 的
   `PALETTE`/`DEFAULT_SHAPE` 给个球色和形状；要挂反代就在 `PLAN_PROXIES` 加一行
