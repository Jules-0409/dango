# Cursor 接入方案（`cursor/` 上游）

> 状态：设计稿，未开工。讨论于 2026-09-19。
> 目标一句话：照 `qoder/` 的样子，把 Cursor 做成 8050 这座桥里的一个前缀上游，
> Droid 挂 `--model cursor/<id>` 就能用，**带真工具轮**。不是单独再造一个反代。

## 0. 和现状的关系

- `cursor-bridge/`（独立 crate，默认 8052）已经把「聊天这条路」做扎实了：
  单发 `agent --print --output-format stream-json`、SSE 形状、思考块签名哨兵、
  正文去重、key 池、假 CLI e2e。**这些全是要搬进 core 的现零件。**
- 它刻意没做的正是 Droid 需要的：**客户端工具透传**（v1 边界里写明了，那需要 ACP + MCP）。
- 所以本方案 = Mode S（搬现有的）+ Mode A（新做 ACP 层），合进 `core/src/cursor/`，
  路由判据 `cursor/` 前缀，非前缀请求一行不碰（和 qoder 同一个隔离承诺）。
- 8052 那个独立服务：P1 落地后留作调试入口或退役，到时候再说，不强拆。
- **现状更新（2026-09-20，`c75d269`）**：「token 直连」（§8 说的 B 计划）那边已经替我们摸过
  并收了钉：原生后端（`CURSOR_BRIDGE_BACKEND=native`，`src/native/`）认证、分帧、解流全通，
  但上游是**服务端驱动的远程执行器** —— 客户端不注册 exec/kv/control、不实现
  `ClientSideToolV2Call` 那 70+ 个工具，服务端就不开始生成；且实测首字 12.09s 全在服务端，
  原生连那 0.2s 的 CLI 启动都省不出什么（上限 ~1.6%）。默认仍 `cli`。**本方案不受影响**：
  Mode A 走 ACP（官方 CLI 的公开协议面），与此路无关；且 8052 已能 `service install`
  交给 launchd 常驻，Mode S 的「先能用」比方案里写的更近了。

## 1. 总体形状

```
Droid ──POST /v1/messages (tools + 全量历史)──► 8050 core
                                                  │ model 带 cursor/ 前缀
                                                  ▼
                                        core/src/cursor/
                                  ┌───────────────┴───────────────┐
                                  ▼                               ▼
                        Mode S：agent --print              Mode A：agent acp
                        （单发、无客户端工具）             （常驻会话、ACP 客户端）
                        文首/思考/用量 → SSE               ▲            │
                                                          │            ▼
                                                  pending 注册表   内嵌 MCP server
                                                  tool_use_id ↔   （把请求里的 tools
                                                  挂起的 MCP 调用   喂给 Cursor agent）
```

请求进来先选 Mode A（请求带 `tools` 且 cursor 会话可用）；Mode A 不可用（CLI 不支持
acp、进程起不来、会话被 GC 后追不上）就降级 Mode S，**不装死**：SSE 里照常出正文，
但模型没有真工具可用，行为等同现在 8052 的 v1。

### 文件划分（照 `qoder/` 的口味）

| 文件 | 职责 | 来源 |
|---|---|---|
| `cursor/mod.rs` | `MODEL_PREFIX = "cursor/"`、`UpstreamKind::Cursor`、前缀判定/剥/加 | 新写（10 行级，照抄 qoder/mod.rs） |
| `cursor/cli.rs` | CLI 查找、`--print` 调用参数、stream-json 事件解析、`probe::LINES` 真抓包单测 | 搬 `cursor-bridge/src/cli.rs` |
| `cursor/protocol.rs` | 事件流 → Anthropic/OpenAI SSE 帧；签名哨兵 | 搬 `protocol.rs`（哨兵同值，本来就和 core 约定一致） |
| `cursor/turn.rs` | Mode S 的 prompt 拼装、24000 字符预算、正文去重 | 搬 `turn.rs` |
| `cursor/accounts.rs` | key 池（`accounts.json` 格式不变） | 搬 `accounts.rs`，路径收进 core 的 config |
| `cursor/models.rs` | `--list-models` + 10 分钟缓存，对外加 `cursor/` 前缀 | 搬 `models.rs` |
| `cursor/acp.rs` | JSON-RPC stdio 客户端：`initialize` / `session/new` / `session/prompt` / `session/cancel`，事件归一 | **新做** |
| `cursor/mcp.rs` | 每会话动态 MCP server：`tools/list` 报客户端 tools，`tools/call` 把调用挂起 | **新做** |
| `cursor/pending.rs` | `tool_use_id → {acp 会话, MCP call id, deadline}` 注册表 | **新做** |
| `cursor/session.rs` | 会话池：指纹→ACP 会话映射、增量水位线、TTL/GC、崩溃降级 | **新做** |

挂载点（core 侧改动面）：

- `chat.rs`：上游分叉处加 `Cursor` 分支（现在 qoder 分叉在 `chat.rs:1131` 附近，同一个模式）。
- `server.rs`：`/healthz` 加 `cursor` 块（CLI 路径、认证模式、号数、活会话数）；
  qoder 的池子挂法照搬（`server.rs:567` 那一行的位置）。
- `config.rs`：加可选 `cursor` 对象，不写 = 不启用，`cursor/` 前缀明确报未启用。

```json
{
  "cursor": {
    "enabled": true,
    "agent_bin": null,
    "accounts_file": "~/.cursor-bridge/accounts.json",
    "workspace": "~/.antigravity-bridge/cursor-workspace",
    "mode": "ask",
    "allow_login_state": false,
    "idle_ttl_secs": 900,
    "max_sessions": 4
  }
}
```

桌面壳读不读 `cursor` 一项：P3 再定，先保证 headless 全功能。

## 2. 工具轮协议（本方案的心脏）

Anthropic 的 tool-use 循环天然是「无状态 HTTP 的两步舞」，ACP 是「一个挂起的 prompt
turn」。把两者缝起来：

1. **Droid → `POST /v1/messages`**（带 `tools`、`system`、全量历史）。
2. 桥算会话指纹（`sha256(system + 首条 user 消息)`）。命中 → 复用 ACP 会话，只投
   **增量尾巴**（见 §3）；未命中 → 起 `agent acp`，`session/new`（`mcpServers` 指向
   桥内嵌 MCP、`model = strip_prefix(id)`），system 按 qoder 惯例作首段投喂。
3. 发 `session/prompt`。Cursor agent 调 MCP 工具 → `mcp.rs` 收到 `tools/call`，
   **不执行、挂起**；桥在正在吐的 SSE 流里发 `content_block_start(tool_use)` +
   `input_json_delta`…（`tool_use_id` 由桥生成），然后 `message_delta(stop_reason="tool_use")`，
   **SSE 收尾关闭**。此刻 ACP 侧的 `session/prompt` 仍然吊着没回 —— 挂起状态由
   `pending.rs` 持有。
4. Droid 在自己那边执行真工具，**再发一次** `POST /v1/messages`，历史尾部带
   `tool_result` 块。
5. 桥用 `tool_result.tool_use_id` 查 `pending`：查到 → 把结果 resolve 回挂起的 MCP
   call，ACP 那一轮继续生成；**新开一条 SSE** 把后续思考/正文吐给 Droid，直到
   `end_turn`（或下一轮 `tool_use`，回到第 3 步）。一个会话可以这样循环多轮。

边界情形，都要实现：

- **一轮多个 `tool_use`**（Droid 会并行发一批）：注册表按集合记，结果齐了才 resolve；
  不齐且超时 → 缺的那个 MCP call 回错（「工具未执行」），不吊死会话。
- **Droid 没回来**（用户中断 / 中途换模型）：pending 超时（默认 120s）→ MCP call 回错，
  会话进 TTL 回收。
- **桥重启 / 会话已 GC**：带 `tool_result` 的请求成了孤儿 → 降级 Mode S，把工具结果
  拼回 prompt 重放，答是答了但接不回原会话 —— 日志明说，不闷声。
- **HTTP 断开**（客户端取消）：对正在生成的会话发 `session/cancel`。
- **CLI 自带工具关不掉**（spike S2 见分晓）：workspace 永远指一个隔离空目录，
  Cursor 自己的文件工具够不到真仓库，Droid 的 tools 才是唯一的手。

## 3. 会话与上下文对齐

- ACP 那侧服务端记上下文，Anthropic 协议每轮重发全量历史 → 桥记**水位线**：
  每个会话记已投喂到 messages 数组的哪个块，新请求只投差集；被 MCP 吃掉的
  `tool_result` 块在差集计算时跳过（它已经进过会话了）。
- Droid 会压缩历史：指纹命中但进来的历史**比水位线短** → 视为对话变了，
  开新 ACP 会话、把当前全量历史一次性喂进去（当轮不转工具也行，工具照常下轮再转）。
- `system`：ACP 没有 system 字段，作首段投喂。**注意与 qoder 的差异**：qoder 承诺
  「桥零注入、上游无 harness」；Cursor CLI 自带 agent 人格和系统提示，这是上游特性，
  文档里如实写，不当 bug 修。

## 4. 认证（红线不变）

- 默认**只用 key 池**（`accounts.json`，格式照 cursor-bridge 现版）。ACP 是常驻进程，
  一个会话钉一个 key（轮换按会话，不按请求）——具体走 `--api-key` 还是环境变量，spike S1 定。
- `allow_login_state` 默认 `false`：主号登录态只有显式打开才允许用。
- 不读、不写、不覆盖钥匙串（v1 README 里那条 CURSOR_CONFIG_DIR 隔离失效的实测结论直接沿用）。
- 验收口径学 qoder：新建的小号，Cursor 后台用量对账，桥跑一轮真 Droid 前后**额度不动**
  （限免/免费额度的情形另说，反正是拿数据说话）。

## 5. 图片

- Mode S：CLI 无图片入参（`--help` 实测），维持 `[图片已省略]` 降级 + 日志。
- Mode A：ACP 的内容块带 resource/blob，图能不能真进模型是 spike S4。能进就做；
  不能进就不做 —— Qoder 那条图床路是**网关**能力，Cursor 不认桥侧塞的外部图 URL，
  别硬凑。

## 6. 测试

- 搬运即带测试：`probe::LINES` 真抓包单测、假 CLI e2e 整套进 core（Linux CI 可跑，
  假 CLI 不烧真额度）。
- 假 CLI 加 **ACP 模式**（stdio 上的 JSON-RPC 脚本），e2e 覆盖：握手 → MCP 注入 →
  `tool_use` SSE 形状 → `tool_result` 续流 → pending 超时 → 并行工具集 →
  会话复用/水位线/压缩历史重开 → cancel。
- 真机验收清单（照 qoder 那套）：
  1. Droid 形状压测：流式 + `tools` + `system` + `max_tokens: 32768` → 200，帧形齐；
  2. `droid exec -m cursor/<id>` 含一轮 `Read` 真工具调用；
  3. 额度对账（小号后台）；
  4. （若 S4 过）贴图一轮；
  5. 桥重启后孤儿 `tool_result` 的降级路径走通。

## 7. 分阶段

| 阶段 | 内容 | 粗估 | 出口 |
|---|---|---|---|
| **P0 spike** | `scripts/cursor-acp-probe.mjs` 假客户端打 `agent acp`，五问（下）；JSON 存 `research/cursor-acp-*.json` | 半天-1天 | 形状落档 `docs/PROTOCOL.md` 新章；S1-S3 全绿才进 P2 设计微调 |
| **P1 Mode S 进 core** | 搬 cli/protocol/turn/accounts/models + 路由 + `/v1/models` 出 `cursor/*` + `/healthz` | 1天 | Droid 挂上能聊能读（无工具），真实机验收 1、2 的无工具版 |
| **P2 ACP 工具透传** | `acp.rs` `mcp.rs` `pending.rs` `session.rs` + 假 CLI e2e | 2-3天（主体） | 验收 1-3 全过 |
| **P3 收尾** | 图片（按 S4）、面板项、TTL/GC 打磨、README/桌面壳、8052 去留决定 | 0.5-1天 | 验收 4、5；文档齐 |

### P0 五问

- **S1 认证**：`agent acp` 怎么吃 API key（全局 flag 前置？env？）。主号登录态还在的
  机器上，带 key 起 acp，确认不走钥匙串、不落新凭据。
- **S2 形状**：`initialize`/`session/new`/`session/prompt` 事件流抓真包 ——
  `agent_message_chunk`、`tool_call`/`tool_call_update`、plan、usage；`modes` 里能不能
  选一个「本地工具最少」的模式。
- **S3 MCP 注入**：`session/new` 的 `mcpServers` 指一个玩具 stdio MCP server，
  agent 调不调 `tools/list`；工具命名空间（`mcp__<server>__<tool>` 之类）怎么映射，
  保证 Droid 拿回的 `tool_use.name` 是它自己发的**原名**。
- **S4 图片**：投 image resource，看模型真不真当图看（回显内容特征）。
- **S5 模型**：会话内能不能按 `--list-models` 的 id 任意点单（`set_model`/configOptions）；
  顺带记录 CLI 注入了哪些自家话术。

### P0 战果（2026-09-20 实测；`scripts/cursor-acp-probe.mjs`，抓包在 `research/cursor-acp-*.json`）

- **S1 认证 ✓**：`agent --api-key <日抛 key> acp` 照收（全局 flag 放子命令前），会话正常建立。
  本机**没有** CLI 登录态：不带 key 的 `session/new` 直接 `-32000 Authentication required`
  （authMethod 只列了 `cursor_login` = 设备码浏览器流，不走）。这条桥从此只有日抛 key 一条路，
  反倒干净：钥匙串碰不到，也没有回退通道。
- **S2 形状 ✓**：`session/new` 返回 `sessionId/modes/models/configOptions`；modes 三档
  `agent/plan/ask` —— **钉 `ask` 正合方案**（它不编辑不执行，本地工具这条路被上游自己关死，
  只剩我们注入的 MCP 工具）。prompt 轮事件序：`session_info_update` →
  `available_commands_update`（CLI 会塞自家命令面，§3 那条「Cursor 自带 harness」的注记成立）
  → `agent_thought_chunk`（思考）→ `agent_message_chunk`（正文）→ `stopReason:"end_turn"`。
  另：`loadSession: true` —— 崩溃/重启后能捞回旧会话，§2 的降级路径可以升格成「真恢复」。
- **S3 MCP ◐**：`mcpCapabilities:{http:true,sse:true}`、`session/new` 吃 `mcpServers` ——
  通道声明在；真 tool-call 往返（玩具 MCP + 一轮 prompt）还没跑，要烧一点日抛额度，下一步。
- **S4 图片 ◐**：`promptCapabilities.image: true` —— 协议层说行；「模型真看得见」等实轮验证。
- **S5 模型 ✓（有彩蛋）**：`models.availableModels` 直接给 37 个（日抛号能点到
  grok-4.6 / claude-opus-5 / gpt-5.6 全家），且**思考等级编在模型 id 里**：
  `grok-4.6[effort=high,fast=true]`、`claude-opus-4-7[…effort=xhigh]`、`kimi-k3[reasoning=max]`、
  `gemini-3.8-flash[reasoning_effort=high]`。`cursor/` 上游可以把带括号变体原样透传，
  /v1/models 直接列全 —— Droid 想要的那个等级旋钮，Cursor 自己就有（和 StepFun 的
  `reasoning_effort` 同一件事，两种包装）。

## 8. 风险与开放项

- **CLI 版本漂移**：ACP 形状跟着 `agent` 版本走。测一版钉一版（`agent --version` 进
  抓包文件名），升级 bump 时测试先响 —— 和 `probe::LINES` 同一哲学。
- **进程成本**：每活会话一个 `agent acp` 进程（内存未知，spike 里量）。`max_sessions` +
  空闲 TTL 兜底；本机单人 Droid，压力主要是「别泄漏」。
- **工具名映射**：Anthropic 工具名规则 × MCP 命名空间，映射要可逆，注册表存原名。
- **参数不可达**：`max_tokens`/`temperature`/`stop_sequences` 依然不生效（v1 原文照抄进文档）。
- **count_tokens**：维持 `/4` 估算，别对账。
- **额度模型**：小号 API key 的限流/计费口径未知，等你建好号实测说话。
