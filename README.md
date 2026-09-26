# Dango（额度团子）

A native macOS quota widget + unified AI-usage hub. One slim capsule lives on
your screen edge — each animated ball is one provider's remaining quota.
Hover a ball and a detail card slides out: every quota bucket, reset
countdowns, and the local proxy endpoint (if any) for that provider.

常驻屏幕边缘的原生 macOS 小胶囊：每颗表情球是一家 AI 套餐的剩余额度，球越开心
额度越足。悬停展开详情卡；顺带把各家 CLI/App 真实烧掉的 token 记成一本
本地账（按天、按模型、按走的哪个出口），还有一座自带账号池的 Gemini 桥。

![capsule](site/img/capsule.png)

## 有什么

| Crate | 干什么 |
|---|---|
| `dango-widget` | 原生小件进程（winit + Core Animation，无 WebView）：胶囊、详情卡、菜单栏项、设置窗口；内置 `127.0.0.1:8049` 控制接口（设置页 / 快照 / SSE） |
| `dango-bridge` | Gemini/Antigravity 账号池桥（`127.0.0.1:8050`）：轮换、熔断、OpenAI + Anthropic 双协议翻译；设置页一键加账号（OAuth 回环，落库即进池） |
| `cursor-bridge` | 把请求交给本机 Cursor CLI Agent 跑的包装（`127.0.0.1:8052`） |
| `dango-lib` | 共用层：模型、macOS 钥匙串读写、各家 provider 探针、token 账本 |
| `grok-ball` | 表情球渲染库，[tycoding/grok-ball](https://github.com/tycoding/grok-ball) 的 Rust 移植（MIT） |

## 支持的额度源

- **Claude**（Claude Code 采样文件）、**Cursor**（官方用量明细）、
  **Devin**（每日额度 % + 本机 `sessions.db` 的每轮真 token 记账）、
  **Factory**、**Gemini/Antigravity**（走 8050 桥的账号池），
  以及自建小球：`custom-*` 模板（DeepSeek / Moonshot / 阶跃 / OpenRouter /
  SiliconFlow 余额）。
- 读不到凭据就是读不到：球如实哭，永不拿缓存数冒充新鲜值。

## 跑起来

```bash
cargo build --release --workspace
./target/release/dango            # 小件 + 8049 设置页
./target/release/dango-bridge     # 可选：Gemini 账号池桥（8050）
./target/release/cursor-bridge    # 可选：Cursor 桥（8052）
```

设置页在 `http://127.0.0.1:8049/ui/settings.html`，或菜单栏图标 →「设置…」。
要常驻就装 launchd（模板见 `crates/dango-bridge/scripts/` 和 MANUAL.md）。

## 凭据纪律

只读各家应用自己留在本机的登录态（CLI 凭据文件 / LocalStorage / 钥匙串），
**永不 refresh token**；手动粘贴的凭据进 `dango` 自己的 Keychain service，
进程内拿不到明文第二次。接口都是逆向来的非公开端点，解析失败如实报错。

## 致谢

表情球引擎来自 **[tycoding/grok-ball](https://github.com/tycoding/grok-ball)**（MIT，Copyright (c) 2026 tycoding）。
`ui/grok-ball.js` 是原版，`crates/grok-ball` 是逐帧对照原版移植的 Rust 版，许可证原文在 `crates/grok-ball/LICENSE`。

## License

MIT。`grok-ball` 保留它自己的 MIT 许可证和版权声明。
