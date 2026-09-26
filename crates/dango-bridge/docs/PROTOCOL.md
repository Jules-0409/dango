# Antigravity 上游协议备忘

**状态：Phase 0 已完成（2026-09-17）。** Phase 0 的探针（`src/spike.mjs --run`，Node 版，
已随老实现一起删）全流程跑通：
换 token → `loadCodeAssist` → `fetchAvailableModels` → `retrieveUserQuotaSummary` →
`streamGenerateContent` 拿到真实回复。证据落在 `research/spike-*.json`。

这份文档回答一个问题：**Antigravity Tools 在跟谁说话、说什么话**，以及**我们自己的桥要怎么接**。

证据分五级，逐条标注：

- **A** = 官方二进制实测字符串 / 公开源码（Gemini CLI、Antigravity-Manager、公开的 API 规范）。可以直接信。
- **B** = Antigravity Tools 的运行日志实测。可信，但那是它的实现选择。
- **C** = Antigravity Tools 落盘的数据文件结构（只读看到的）。
- **V** = 我们自己在 2026-09-17 直连上游实测过（spike / 探针；代码已随老实现删掉，看 git 历史）。
- **D** = 还没验证的推测。

红线（全程有效）：不改系统代理 / 网络设置；不改官方 App 和 Antigravity Tools 的文件；
凭据只读、不打印、不落盘、不进仓库。

---

## 1. 上游是什么

**Google Cloud Code Assistant API**（`v1internal`），Google 的内部网关，不是 OpenAI 风格、也不是 REST 路径风格。

- 调用风格 **A**：`POST {base}:{method}`（proto custom method），例如
  `POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist`。
- 官方语言服务器（`/Applications/Antigravity.app/.../bin/language_server`，146 MB Go 二进制）里
  有 **61 个 `v1internal:` 方法** **A**。
- 三个端点，**回退顺序** **B**（桥的日志实测）**V**（我们三个都试过，sandbox 正常能用）：

  1. `https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal`
  2. `https://daily-cloudcode-pa.googleapis.com/v1internal`
  3. `https://cloudcode-pa.googleapis.com/v1internal`

  桥的日志原话：`Quota API request failed at .../v1internal:fetchAvailableModels: client error (Connect)` →
  `Quota API fallback succeeded at endpoint #2`、
  `✓ Upstream fallback succeeded | Endpoint: https://daily-cloudcode-pa.googleapis.com/v1internal | Status: 200 OK | Next endpoints available: 1`。

### 我们真正需要的方法

| 方法 | 干什么 | 状态 |
| --- | --- | --- |
| `loadCodeAssist` | 拿 tier + project | **V** 200 |
| `fetchAvailableModels` | 模型清单 + 每个模型的剩余额度 | **V** 200 |
| `retrieveUserQuotaSummary` | 分桶额度（weekly / 5h，含 Claude/GPT 的 3p 桶） | **V** 200 |
| `streamGenerateContent?alt=sse` | 主力：流式生成 | **V** 200 |
| `generateContent` | 非流式生成 | A（未单独实测） |
| `onboardUser` | 开 tier，LRO 轮询；**可能会建 GCP 项目，用前要授权** | A（未调用） |

---

## 2. 认证

### 流程

**A**：

```
POST https://oauth2.googleapis.com/token
Content-Type: application/x-www-form-urlencoded

grant_type=refresh_token&client_id=…&client_secret=…&refresh_token=…
```

授权端点 `https://accounts.google.com/o/oauth2/v2/auth`（要 refresh token 必须 `access_type=offline`）。

### refresh_token 与 client_id 绑定

- 换 token 必须用**当初签发这个 refresh_token 的 client_id**，否则 `invalid_grant` **A**。
- 官方客户端凭据硬编码在官方二进制里，两个 client_id：`1071006060591-…`（消费版）和
  `884354919052-…`（`businessaicode` / 企业版，官方二进制里配套的 proto 包名是
  `com.google.cloud.businessaicode.v1main`）**A**。
- 桥的二进制里有一张 OAuth 客户端表，条目 `antigravity_enterprise` / "Antigravity Enterprise"，
  配的就是 `1071006060591-…` **A**；账号文件里的 `oauth_client_key` 值也是 `antigravity_enterprise` **C**，
  id_token 的 `aud` 同样是这个 client **V**（解 JWT claims 得到）。
- **已解决**：能自己刷 token 了 **V**。`client_id 1071006060591-…` + 对应 secret 走
  `grant_type=refresh_token` → 200 / `expires_in=3599`。
  坑点：二进制里那串凭据是「客户端 id、secret、下一个字符串常量」紧挨着排的，
  贪婪匹配会把后面的东西一起吃进来；**正确长度是 35**（`GOCSPX-` + 28），
  按 42/47/整段去试都会得到 `invalid_client: The provided client secret is invalid`。
  这个 client 的凭据在公开仓库 `lbjlaq/Antigravity-Manager`（`src-tauri/src/modules/oauth.rs`）
  里就是明文，两个本机二进制里也各有一份 **A/V**。
- 桥还有个环境变量覆盖：`ANTIGRAVITY_OAUTH_CLIENTS`，格式
  `key|client_id|client_secret|可选标签`，默认 key 是 `antigravity_enterprise` **A**。
  账号文件里的 `oauth_client_key` 就是指向这张表；桥的注释说：
  "keep oauth_client_key unset to avoid accidental enterprise lock" **A**。
- 我们自己的做法：**运行时**从本机二进制里抽凭据（`core/src/oauth.rs`），
  仓库里不落任何 secret；抽不到就退回账号文件里现成的 access_token **V**。

### token 里确实有什么（自省）

2026-09-17 实测（`oauth2.googleapis.com/tokeninfo`）**V**：
`aud` = `1071006060591-…`；scope = `email profile openid` +
`https://www.googleapis.com/auth/{cloud-platform,cclog,experimentsandconfigs,userinfo.email,userinfo.profile}`；
`access_type=offline`。**scope 没问题**，403 不是权限范围问题。

### 凭据在本机哪

**C**：`~/.antigravity_tools/accounts/<uuid>.json`（老桥的路径，这份账号库已迁到我们自己的
`~/.antigravity-bridge/accounts/`，文件形状一样），每个账号一份：

```
token: {access_token, refresh_token, expires_in(3599), expiry_timestamp, token_type,
        project_id("aicode-consumers"), oauth_client_key("antigravity_enterprise"), id_token, is_gcp_tos}
device_profile: {machine_id, mac_machine_id, dev_device_id, sqm_id}
quota: {models[], quota_groups[], subscription_tier("Google AI Pro"), model_forwarding_rules, …}
disabled / proxy_disabled / validation_blocked / created_at / last_used
```

我们的桥将来要有自己的存储（不依赖它的目录）；Phase 0 只是只读借一个 access_token 做验证。

---

## 3. 请求格式（已实测）

### 三个必带头（缺了就是 403）

**V**（这是 Phase 0 最大的坑）：

```
Authorization: Bearer <access_token>
Content-Type: application/json
User-Agent: antigravity/1.11.5 windows/amd64
X-Goog-Api-Client: google-cloud-sdk vscode_cloudshelleditor/0.1
Client-Metadata: {"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}
流式再加：Accept: text/event-stream
```

少这三个头（UA / X-Goog-Api-Client / Client-Metadata），同一个 token、同一个请求体会得到：

- `fetchAvailableModels` → 403 `PERMISSION_DENIED`（"The caller does not have permission"）
- `retrieveUserQuotaSummary` / `streamGenerateContent` → 403 `SUBSCRIPTION_REQUIRED`
  （"You do not have a valid license of this product…" ← 这句话完全是误导，跟许可证无关）

而 `loadCodeAssist` 不带这些头也能过 **V**，所以早期很容易误判成"账号没权限"。

### 包装体（camelCase，实测可用）

**V**：

```json
{
  "project": "aicode-consumers",
  "model": "gemini-3.6-flash-high",
  "request": {
    "contents": [{"role": "user", "parts": [{"text": "..."}]}],
    "generationConfig": {"maxOutputTokens": 2048, "temperature": 0.2}
  },
  "userAgent": "antigravity",
  "requestId": "<uuid>"
}
```

`request` 里就是标准 Gemini 风格：`contents`（`role: user|model`）、`systemInstruction`、
`tools[{functionDeclarations:[…]}]`、`toolConfig`、`generationConfig`。
proto3 的 JSON 解析 snake_case / camelCase 都收 **A**，但**实测有效的是上面这份 camelCase 形状** **V**。

### 响应（SSE）

**V**：`?alt=sse`，每行 `data: {json}`，一个 chunk = 整个响应对象：

```json
{
  "response": {
    "candidates": [{"content": {"role": "model", "parts": [{"text": "…"}]}, "finishReason": "STOP"}],
    "usageMetadata": {"promptTokenCount": 6, "candidatesTokenCount": 1, "totalTokenCount": 98, "thoughtsTokenCount": 91}
  },
  "traceId": "…",
  "metadata": {…}
}
```

`parts[]` 的形态 **A**：`{text}`、`{text, thought:true}`（思考）、`{functionCall:{name,args}}`、
`{functionResponse:{name,response}}`、`{thoughtSignature:<base64>}`、`{inlineData:{mimeType,data}}`。

### 错误形态

**B**（桥日志实测）**V**：标准 Google RPC error，`error.details[]` 里有
`@type: type.googleapis.com/google.rpc.ErrorInfo`、`reason`（`QUOTA_EXHAUSTED` /
`SUBSCRIPTION_REQUIRED` / `PERMISSION_DENIED` / `INVALID_ARGUMENT`）、`domain: cloudcode-pa.googleapis.com`。
我们的桥要把 `reason` 原样往上透，别把 `SUBSCRIPTION_REQUIRED` 当成真·订阅问题。

---

## 4. 账号 / 项目 / 配额（已实测）

- `loadCodeAssist` **V**：请求体
  `{"metadata":{"ideType":"ANTIGRAVITY","platform":"DARWIN_ARM64","pluginType":"GEMINI"}}`
  → 200，返回 `currentTier.id = "free-tier"`、`allowedTiers = [free-tier, standard-tier]`、
  **`cloudaicompanionProject = "aicode-consumers"`**（项目就是这么来的，不是我们编的）。
  `mode: "HEALTH_CHECK"` 也能拿到同样的信息；不带那三个头时默认模式一个字段都不给 **V**。
  注意 `ineligibleTiers` 里有一条 `UNSUPPORTED_CLIENT`：
  "This client is no longer supported for Gemini Code Assist for individuals… migrate to Antigravity"。
- `fetchAvailableModels` **V**：`{"project":"aicode-consumers"}` → 27 个模型，形状是
  `models: { <模型 id>: {supportsImages, supportsThinking, thinkingBudget(-1 = 动态),
  minThinkingBudget, recommended, maxTokens, maxOutputTokens, quotaInfo:{remainingFraction, resetTime},
  model("MODEL_PLACEHOLDER_…"), apiProvider, modelProvider, …} }`，
  外加 `defaultAgentModelId`（当前 `gemini-3.6-flash-high`）、`agentModelSorts`、
  `commandModelIds`、`tabModelIds`、`imageGenerationModelIds`、`mqueryModelIds`、
  `webSearchModelIds`、`deprecatedModelIds`、`commitMessageModelIds`、
  `audioTranscriptionModelIds`、`experimentIds`、`tieredModelIds`。
- `retrieveUserQuotaSummary` **V**：`{"project":"aicode-consumers"}` → `groups[]`：

  ```
  Gemini Models         : gemini-weekly(weekly 83.2%) / gemini-5h(5h 99.9%)
  Claude and GPT models : 3p-weekly(weekly 100%) / 3p-5h(5h 100%)
  ```

  这就是额度面板要的数字。
- **不要写死模型名** **V**：现在请求 `gemini-3.5-flash-low`，上游会回一句纯文本
  "Gemini 3.5 Flash is no longer available. Please switch to Gemini 3.7 Flash…"（不是报错，是 200），
  桥的 `model_forwarding_rules`（`gemini-3.1-pro-high → gemini-pro-agent`）**C** 也是同一层意思。
  模型清单必须每次从 `fetchAvailableModels` 取。

---

## 5. 从桥的运行日志里学到的实战约束（直接抄结论）

1. **`maxOutputTokens` 上限 65536** **B**：`Capping maxOutputTokens from 128000 to 65536 to prevent 400 Invalid Argument`。
2. **thinking 与 maxOutputTokens 联动** **B**：`Bumping maxOutputTokens to 32768 due to thinking budget of 24576`。
3. **thoughtSignature 必须原样回传** **B**：桥维护 session 级签名缓存，
   缺失时给 `tool_use` 塞 `GEMINI_SKIP_SIGNATURE` 哨兵。
4. **断链的思考会搞坏工具循环** **B**：`[Thinking-Recovery] Broken tool loop (ToolResult without preceding Thinking). Recovery triggered.`
   —— 和我们在 Factory 侧看到的 400（`tool` 消息没有前置 `tool_calls`）是同一类问题，桥一直在救它。
5. **配额调用的 403 处理** **B**：`Quota fetch got 403 with project ID, retrying without project ID…`、
   `Warmup: 403 Forbidden - quota fetch denied` → 说明历史上 `x-goog-user-project` 这类头在配额请求上
   时灵时不灵；我们的桥要么不用它，要么带同样的重试。
6. **403 之后它会把账号标 forbidden** **B/C**：`Account unauthorized (403 Forbidden), marking as forbidden`，
   账号文件里有 `disabled` / `validation_blocked` / `validation_url`。我们的桥要有同样的熔断，
   否则一个坏账号会把整池拖慢。

---

## 6. 还没解决 / Phase 1 待办

1. **账号层已经没有拦路虎**：`refresh_token` 刷新、多账号、额度缓存、403/429 熔断都能做（§2 已验证）。
   仍要决定的是产品层面的事：用官方 client 身份继续（现状），还是哪天换成自建 OAuth 客户端重新授权。
2. `onboardUser` 没碰过（可能创建 GCP 项目，动它之前必须问）。
3. 线上行为的盲区（Phase 1a 消掉的标 ✅）：
   ✅ 函数调用（`functionCall` / `functionResponse`）往返 —— 见 §9；
   ✅ 思考签名与签名的真实报文 —— 见 §9；
   `thinkingConfig` 的合法范围（预算上限、`includeThoughts` 的副作用）还没系统试过；
   429/5xx 的重试语义、`x-cloudaicompanion-trace-id` 等可选头；`systemInstruction` 上游收不收
   （服务层已按这个形状发，但还没在真上游上验过一次带 system 的请求）。
4. 账号层的风控字段（设备指纹 `device_profile`、`validation_blocked`、`validation_url`）
   服务端到底怎么校验 —— 值得单独摸一次。
5. 模型清单是动态的（今天 27 个，含 `gpt-oss-120b-medium`、`gemini-3.8-flash-tiered` 这些），
   还有 `defaultAgentModelId` / `agentModelSorts` 之类的排序信息，服务层要按它做映射。

---

## 7. 我们自己的桥怎么分层（Phase 1 计划）

1. **账号层**：OAuth（§6 的选择）、token 刷新、多账号轮换、额度缓存、403/429 熔断。
2. **上游层**：`v1internal` 客户端 —— 三端点回退、三个必带头、camelCase 包装、
   SSE 解析、`reason` 透传。
3. **服务层**：对外 Anthropic / OpenAI 协议（Factory 今天就在用），把上游的 Gemini 形状互相翻译；
   **直接把 `byok-proxy` 里已经写好的两件修复搬进来**：泄漏的 `<call:…>` 文本转真 `tool_use`、
   孤儿 tool 消息清理（这两个错误的根因都在上游侧）。
4. **工程层**：日志、额度面板、账号 UI、健康检查。

Phase 0 期间**没有**动的东西：系统代理/网络设置、官方 App 与 Antigravity Tools 的文件与进程、
`~/.factory/settings.json`（里面 8046 那个临时垫片照旧）。

---

## 8. 验证脚本

Phase 0 的探针（`node src/spike.mjs`，干跑 / `--run --local` / `--run --only=token,loadCodeAssist`
之类）随老实现一起删了，要看就翻 git 历史。现在对应的真上游自测是 Rust 的 `smoke` 二进制：

```bash
cargo run -p antigravity-core --bin smoke                  # 干跑：只打印计划，不发任何请求
cargo run -p antigravity-core --bin smoke -- --run         # 真上游自测：自起一座临时桥，6 步全过
cargo run -p antigravity-core --bin smoke -- --run --only=tool   # 只跑工具回环那两步
```

报告落 `research/spike-<时间戳>.json`，写盘前断言不含 token 原文。
当年的探针是 Phase 0/1a 的过程记录（哪个假设怎么被证伪的，代码看 git 历史）：
01-403 头矩阵、02 entitlement/credits、03 metadata/modes、04 spec headers、
05 工具调用与签名（真实报文落 `research/tool-call-raw.json`，不进仓库）、06 哨兵与并行调用。

---

## 9. 服务层（Phase 1a）的实测结论 **V**

### 工具调用与签名（探针 05 / 06）

- 上游发出来的模型回合里，`functionCall` part 会带 `thoughtSignature`（实测 1152 / 3228 字符）。
- **回传时签名不能丢**：不带签名、也不带哨兵 → `400 INVALID_ARGUMENT
  "Function call is missing a thought_signature in functionCall parts"`（报错文案明确说
  "这是为了让工具能正常工作，缺签名可能会让模型乱猜工具调用"）。
- **哨兵可用**：把签名换成 `skip_thought_signature_validator` → 200。服务层在「签名没存到」
  时用它兜底（`core/src/signatures.rs`）。
- **思考 part 同理**（2026-09-18 实测）**V**：上游偶尔给**没有** `thoughtSignature` 的思考 part；
  下游（客户端）要拿签名才有资格留存思考块，所以桥给这种块补同一个哨兵，客户端回传时上游也接受
  （实测 200，正常返回 thinking + text）。
- **并行调用的签名只挂在第一个 part 上**（上游自己的报文就是这样）；回传时也只给第一个 part 带，
  其余原样不带 —— 实测 200。
- 有些回合结束时会单独发一个只有签名、没有正文的 part（`{text:"", thoughtSignature}`）。
  它属于整个回合，服务层按会话存（trailing signature），不硬塞给某次调用。

### 服务层的形状决定（都在 `core/` 里有对应实现）

1. **响应头最后才写**：上游可能在开流前就失败（403 无授权、400 参数、模型下线回 200 纯文本），
   所以服务层等第一个 chunk 真的到了才写 200；在此之前失败就回真正的错误状态码。
   —— 桥的「空回合」就是这么来的：它先写 200，后面出什么都只能塞进流里。
2. **模型名动态解析**：客户端给的名字在上游模型表里找不到时，按
   归一化相同 → 词元命中（≥3 字符的片段，免得单字母误命中）→ 家族兜底（claude/gpt/flash/pro，
   取版本最新、挡位最高）→ `defaultAgentModelId` 的顺序退让，
   并把替换和理由写进响应头 `x-bridge-model` 与日志（`模型替换（token_match）：…`）。
3. **usage 合并**：`candidatesTokenCount + thoughtsTokenCount` 才是 Anthropic/OpenAI 眼里的
   output tokens（思考也要计费）。
4. **结束原因映射**：有工具调用就是 `tool_use`（Anthropic）/`tool_calls`（OpenAI），
   否则 `end_turn`/`stop`；`MAX_TOKENS` → `max_tokens`/`length`；安全类 → `end_turn`/`content_filter`。
5. **maxOutputTokens 托底**：开了 thinking 且 `maxOutputTokens <= budget` 时抬到 `budget + 8192`
   （上限 65536）—— 抄桥的日志结论，免得思考把正文挤没。

### 思考：上游不回正文（探针 08，2026-09-18）**V**

- **`includeThoughts: true` 没用**：不管带不带、预算多少，上游回的 part 只有
  `{text:"<答案>"}` 和紧跟其后的 `{text:"", thoughtSignature:"…"}`；**没有任何 `thought: true` 的 part**。
  思考只体现在 `usageMetadata.thoughtsTokenCount`（实测 73 ~ 273）。
- 所以服务层不会真的产出 Anthropic 的 `thinking` 块 —— 不是我们没翻译，是上游不给。
  usage 里仍然把思考字数并进 `output_tokens`（思考也是要计费的）。
- **`thinkingLevel` 是被接受的旋钮**：同一个问题，`{thinkingBudget:2048}` 思考 79 字，
  `{thinkingLevel:"high"}` 思考 264 字，`{thinkingBudget:-1}`（模型表里 tiered 模型的默认值）思考 273 字。
  预算写 0 也照样思考（73 字）—— 在 tiered 模型上「关思考」关不掉。
- 模型表里 `thinkingBudget: -1` 表示「模型自己定」，`minThinkingBudget` 是最低下限。

### 额度（探针 07 + `/quota` 路由，两个账号实测）**V**

- `retrieveUserQuotaSummary` → `groups[].buckets[]`，只有两种窗口：`weekly` / `5h`；
  分组只有两类：Gemini Models（`gemini-weekly`/`gemini-5h`）与 Claude and GPT models（`3p-weekly`/`3p-5h`）。
  组内模型共享同一份额度，所以模型级 `quotaInfo.remainingFraction` 就是它所在组的值。
- `fetchAvailableModels` 的每个模型除了额度还带 `supportsThinking` / `thinkingBudget` /
  `minThinkingBudget` / `maxTokens` / `maxOutputTokens` —— 21 个模型支持思考。
  **服务层下一步该按模型表里的 `maxOutputTokens` 来 clamp，而不是写死 65536。**
- **两个账号都能查**：`GET /quota?all=1` 会把库里每个可用账号各查一遍（各自签各自的 token，
  第二个账号约 9 秒），返回分组窗口 + 模型级剩余 + 重置时间。
- **pro 系在免费层被拒**：`gemini-3.1-pro-high` 稳定回
  `429 Resource has been exhausted (e.g. check quota)`，而同一时刻它的额度显示还有 99.8%。
  又是一条误导性文案（和 403 那条同类）。服务层的行为：如实映射成 `rate_limit_error` 交给客户端，
  不静默换模型。

### 客户端见到的「Antigravity harness」是老桥注入的（探针 09，2026-09-18）**V**

- 现象：同一个「你是谁」问题，走老桥（8045）模型自称「我是 Antigravity，由 Google DeepMind 团队…」。
- 老桥二进制里就有这段 harness 原文并按模型名（`flash` / `pro` / `claude` / …）映射注入：
  `You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team
  working on Advanced Agentic Coding.` / `You are pair programming with a USER…` / `**Absolute paths only**`
  / `**Proactiveness**`；它的界面配置里还有一个 `global_system_prompt`（说明写着
  "Automatically injected into all systemInstructions"）。
- 走老桥调 `claude-sonnet-4-6`，模型会把上面这些短语**原文背出来** —— 实锤是桥自己加的
  `systemInstruction`，不是上游服务器加的（Claude 不可能自认 Antigravity）。
- 直连上游（我们现在这套形状）和我们的桥都不带 harness：gemini 回「由 Google 训练的大型语言模型」，
  claude 回「我是 Claude，由 Anthropic 制造」并明确说上下文里没有额外系统设定。
- 顺带两个结论：body 里的 `userAgent` 字段去不去都 200（不是它触发 harness）；
  但 **HTTP 头 `User-Agent` 是硬要求** —— 换成 `curl/8.7.1` 立刻 403 `SUBSCRIPTION_REQUIRED`
  （§3 那三个必带头里，至少这个不能动）。

### harness 是无条件注入的；老桥的「身份层」在设备指纹（2026-09-18）**V**

- 客户端**自己带了 `system`，老桥照样注入 harness**：带 system 问「你的系统提示里有没有
  `pair programming` 这个词」，回答「有」；不带的对照也「有」。（只带 system、直接问「你是谁」时，
  模型会在两套指令间摇摆 —— 一会儿 Antigravity 一会儿 Gemini，这本身就是两套 system 打架的表现。）
- 所以 harness 的实际效果是**每一次上游请求的正文都更像官方 IDE 发的**（IDE 每次都带这段），
  兼有「给不带 system 的客户端一个稳定的默认行为」的功能意义。**它是不是"降低风控概率"的设计，
  没有直接证据，只能说把请求伪装成官方形状的动作在它身上不止这一处。**
- 更明确的"像官方客户端"动作在**设备身份层**：老桥维护并发送设备档案
  （`machine_id` / `mac_machine_id` / `dev_device_id` / `sqm_id`，还有 `x-machine-id` 头、
  `device_fingerprint` 相关逻辑与持久化的 `device_profile` / `device_history`）。
  要模仿官方流量，这一层比 system prompt 更关键。
- 只读观察（2026-09-18）：库里两个账号都没有被封/需要验证的标记
  （`is_forbidden=false`、`validation_blocked=false`、`disabled=false`、`forbidden_reason=null`，
  订阅层都是 Google AI Pro），而其中一个账号自 9 月初起经老桥跑过 1741 次请求。
  「没出事」是事实；「因为 harness / 设备指纹所以没出事」不是结论。

### 协议默认值：不写 `stream` 就是整包（2026-09-18）

- Anthropic 的规矩是只有 `stream:true` 才走 SSE。服务层原来写的是「不写 = 流式」，已改成跟协议一致
  （老桥也是这个行为：不带 `stream` 的 curl 拿到的是 JSON）。`stream:false` 或不写 → JSON；
  `stream:true` → `text/event-stream`。这一条有 e2e 测试钉着。

### 账号层（多账号选号 / 熔断 / 粘性，2026-09-18）**V**

- **429 的粒度是「模型」，不是「账号」也不是「家族」**：免费层下 `gemini-3.1-pro-high` 稳定 429，
  同时 `gemini-3.8-flash-tiered` 正常 200。所以 429 只记到那个模型上；401/403 才是账号级
  （两个额度家族一起记）。按家族记会把好好的 flash 一起拖下水。
- **换号是真有用的**：实测 `gpt-oss-120b-medium` 在 en 账号 429、换到 om 账号 200；
  `gemini-3.1-pro-high` 两个账号都 429 时如实回报（聚合 429 + `Retry-After` = 最早醒的熔断）。
- **额度分按家族算，取该家族所有窗口里最紧的那个**：选号实测有效 —— 同一个桥，
  gemini 请求落在 weekly 83% 的账号、claude 请求落在 3p 99.9% 的账号（响应头 `x-bridge-account` 可验）。
- **退避沿用老桥的 [60s, 300s, 1800s, 7200s]**（它的 `circuit_breaker` 配置就是这个），
  成功一次清零；全都在冷却时也会硬着头皮试最早醒的那个（标 `breaker_forced`），不让客户端干等。
- **会话粘性的边界**：只有真会话（`metadata.session_id` / `user_id`）才粘；`default` 这种
  「没有会话标记」的流量不粘，否则所有客户端都会粘死在第一个账号上。
  粘住的账号额度掉到最高分 60% 以下时也让位。
- 换号只发生在**开流之前**：一旦给客户端吐过一个 chunk（或非流式已经开始攒 chunk）就锁死当前账号，
  失败如实报错 —— 不能让客户端看到两段拼起来的正文。

### 兼容端点与预算上限（2026-09-18）**V**

- 老桥日志里那些 404 的来历（逐个查过 `client_ip`/时间戳）：`/v1/models/{单个模型}` 64 次
  （客户端在 OpenAI 协议下探测模型是否存在）、`/props` `/v1/props` `/version` 各 28 次且时间戳
  几乎同一毫秒（一个把本地端点当 llama.cpp 那种服务探测的客户端），`/nope` 40 次是一次性自测。
- 我们的取舍：`GET /v1/models/{id}`（两种协议通用外形）+ `/models` 别名 + `/version` 都实现；
  **`/props` 继续 404** —— 语义不明，编一个形状可能比 404 更误导客户端。
- `/v1/models/{id}` 的规则：表里有的直接给；表里没有但能按解析规则匹配上的（`normalized` /
  `version_match` / `token_match` / `family_fallback`）给 200 并带 `resolved_from` + `resolve_reason`；
  `default_fallback`（谁也没匹配上，随便挑了个默认模型）必须 404 —— 否则客户端拿编错的名字探测也会得到 200。
- **预算改成按模型表 clamp**（`fetchAvailableModels` 的 `maxOutputTokens` / `minThinkingBudget`）：
  实测 `gemini-3.8-flash-tiered` = 65536 / minThinking 32，`claude-sonnet-4-6` = 64000，
  客户端要 300000 会被收到表里的值，并在请求日志的 `warnings` 里写明 `max_tokens_clamped:300000->64000`。
  拿不到模型表时才退回原来的硬编码 65536。思考预算低于 `minThinkingBudget` 时抬到下限，
  并保证 `maxOutputTokens > thinkingBudget`（小了就加 8192，仍然不够就记 `thinking_budget_exceeds_output_cap`）。

---

## 10. 第二条上游：Qoder CN（2026-09-18）**V**

加这条通道的理由只有一个：**Qwen3.8-Flash（上游 key `qfmodel`）在限免**，而那个活动只认
Qoder 客户端 / CLI 的会话（PAT 通道不算）。桥不自己登录，借官方 CLI 已经登好的会话。
实现见 `core/src/qoder/`，配置与用法见 README「Qoder 上游（CN，自用）」。

### 端点与调用风格

跟 Antigravity 完全不同的另一套：两个域名 + **自签名的 COSY 协议**，计费按**积分**。

| 用途 | 请求 |
| --- | --- |
| 换 job token | `POST {openapi}/api/v1/me/jobToken`，body `{"clientId":"732aef47-9cf2-46a2-95fe-4cebb5d0d1fa"}`，头 `Cosy-Version: 1.0.1` + `Cosy-ClientType: 5`，`Authorization: Bearer dt-…` |
| 身份 | `GET {openapi}/api/v1/userinfo` |
| 额度 | `GET {openapi}/api/v2/quota/usage` |
| 模型表 | `GET {gateway}/algo/api/v2/model/list?Encode=1`（COSY 签名） |
| 推理 | `POST {gateway}/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1`（COSY 签名） |
| 传图 | `PUT {gateway}/algo/api/v2/image/upload?request_id={32 位 hex}`，multipart 字段 `file`、文件名 `image.<ext>`（COSY 签名，见下） |

`{openapi}` = `https://openapi.qoder.com.cn`，`{gateway}` = `https://gateway.qoder.com.cn`。
COSY 版本：网关侧 `1.1.38`（旧值会让 `model/list` 返回缩减列表）、openapi 侧 `1.0.1`。

### COSY 签名（`core/src/qoder/cosy.rs`）

```text
aesKey  = uuid v4 去横线取前 16 个字节（ASCII）
info    = b64(AES-128-CBC(key = IV = aesKey)({uid, aid:"", name, email, security_oauth_token}))
cosyKey = b64(RSA_PKCS1v15(1024 位内置公钥)(aesKey))
meta    = b64({version:"v1", requestId, info, cosyVersion:"1.1.38", ideVersion:""})
sigPath = pathname 去掉前导 "/algo"
sig     = md5_hex(meta \n cosyKey \n 秒级时间戳 \n body（编码后的字节） \n sigPath)
Authorization: Bearer COSY.<meta>.<sig>
```

头还有 `Cosy-Key` / `Cosy-User` / `Cosy-Date` / `Cosy-Version` / `Cosy-Machineid` /
`Cosy-Machinetoken` / `Cosy-Machinetype: 5` / `Cosy-Machineos` / `Cosy-Clienttype: 5` /
`Cosy-Clientip` / `Cosy-Bodyhash` / `Cosy-Bodylength` / `Cosy-Sigpath` /
`Cosy-Data-Policy: disagree` / `Cosy-Organization-Id` / `Cosy-Organization-Tags` /
`Login-Version: v2` / `X-Request-Id`。
**签名串里的 body 是编码后的字节**，所以顺序永远是「先编码、再签名」。

### body 编码：私有 base64（`core/src/qoder/encoding.rs`）

标准 base64 → 按自定义字母表换表
（`_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!`）→ 3 个字符一块做旋转
→ `=` 换成 `$`。解码反向。

### 推理 body 与 SSE

`agent_chat_generation` 那套（`chat_task:"FREE_INPUT"`、`agent_id:"agent_common"`、
`model_config.key`、`system`、`messages`、`tools`、`parameters{max_tokens, enable_thinking}`…）。
三个坑（都实测过）：

- 顶层 `system` 字段**不认**，系统提示词得放成前导的 `role:"system"` 消息；
- `messages` 的 `content` 平时**必须是字符串**；只有带图那一轮换成 part 数组（见下）；
- 只有工具调用、没有正文的 assistant 消息，`content` 要塞一个空格，否则上游把整条消息丢掉，
  后面的 tool 结果就成了孤儿；`tool_calls` 与 `role:"tool"` 的 `tool_call_id` 要自己配。

响应是 SSE 信封：`data: {"statusCodeValue":200,"body":"<JSON 文本>"}`，`body` 里才是 OpenAI
风格 chunk；裸 `[DONE]` 或 `body:"[DONE]"` 是结束。**读到 DONE 必须主动断开**（上游不关连接）。

### 图片：先传图床，再换成 OpenAI 形状的 part（2026-09-19 实测）**V**

`model/list` 里 `qfmodel`（以及 `auto` / `qmodel_38max`）标着 `is_vl: true`，图确实能进；
但**不是**把 base64 塞进 `messages` 就完事 —— 得先传到 Qoder 自己的图床，再在那一轮里引用回来的 URL。

- **上传**：`PUT {gateway}/algo/api/v2/image/upload?request_id={32 位 hex}`，multipart（`file` 字段，
  文件名 `image.png` / `image.jpg` …）。返回
  `{"result":{"url":"https://qoder-cn-vl-private.oss-cn-beijing.aliyuncs.com/…?Expires=…&OSSAccessKeyId=…&Signature=…"}}`
  —— 一张约 **30 天**有效的签名 URL。**路径必须带 `/algo`**：少了就是 ALB 503（`/api/v2/image/upload`）。
- **签名与别处不同**：上传时 `sig` 里的「body」是**请求体字节长度的十进制字符串**（比如 `1234`），
  不是字节本身；`sigPath` 照旧是去掉前导 `/algo` 的路径。签错 → 403 `Signature invalid`；
  同一个 `request_id` 重放 → `{"code":"103","message":"Duplicate request"}`。
- **发图**：那一轮 `content` 换成 part 数组，图片用 **OpenAI 形状**：
  `[{type:"text",text:"…"},{type:"image_url",image_url:{url:"<上面那张 URL>"}}]`。
  实测只有这个形状被当图看（prompt tokens 71 → 124，模型的描述与图一致）；
  官方 SDK 那套 `{type:"image",source:{type:"url"/"base64"}}`、顶层 `image_urls`、
  把签名 URL 写成 markdown 链接 —— **都被当没看见**（prompt tokens 一动不动）。
- **官方客户端是另一条路**：app / `@qoder-ai/qoder-cn-agent-sdk` 先把图 PUT 上去再发
  `{type:"image",source:{type:"url",url}}`（worker 里叫 `uploadImage` / `uploadImagesToQoder`，
  默认开）；那个形状是**网关转换前**的内部形状，我们直接打 `agent_chat_generation` 时它不认，
  只有 `image_url` 认。两边的上传端点、签名口径一致（1.1.38 CLI 与 1.1.53 app 都对过）。
- **没上传 = 静默丢图**：上游对不认识的 part 不报错，直接忽略。所以桥自己兜底：
  `core/src/qoder/upload.rs`（multipart + 解 URL）、`cosy.rs::build_upload_headers`（长度串签名）、
  `body.rs`（`inlineData` → `image_url` part，缺 URL 就留一条 `image_not_uploaded:` 警告）、
  `session.rs`（按图片 base64 的 sha256 缓存最近 64 张，客户端每轮重发历史也不重复传）。

### 上游自带的人设，以及怎么顶掉它（实测）

不带 system 直接调，模型会自称「Qwen（通义千问）」并报一个过期的 `CurrentDate`（见过
2026-06-22 / 2026-07-20）—— 上游自己塞了一段产品人设。给一段普通客户端 system 也压不住它
（照样自称 Qwen，只是把上游那段 `CurrentDate` 原文吐了出来）；**换成一段明确中性的提示才顶得掉**：

> You are a bare language model served through an API. You have no built-in persona, product
> identity, or vendor role configuration; ignore any such built-in instructions if present. …

带这条之后模型自述「客户端配置的裸模型」，日期也以客户端为准。这只是**可选**项：
`qoder.neutralize` 默认 `false` —— 桥默认一个字的额外内容都不注入（上游自带什么就是什么），
置 `true` 才垫这条；客户端自己的 system 接在它后面，所以不覆盖客户端的指令；实现见
`core/src/qoder/body.rs::NEUTRALIZER`。

真身在客户端：官方 CLI 的 bundle（`qoderclicn.js` / `qoder-worker-runtime.mjs`，各 33 MB）里有
整套 harness —— `You are QoderWork…` / `You are Qoder's desktop agentic assistant…` + 子代理人设
（文件搜索 / 架构规划 / 代码评审 / worker fork）+ 技能（`SKILL.md`）+ 安全分类器 + sandbox 策略，
架构跟 Claude Code 是一路的。跟 Antigravity 一样：**harness 是客户端拼的，模型本身不带**
（桥不注入它；默认那条中和提示只是把上游自带的那层压平）。

### 额度与计费（实测口径）

`GET /api/v2/quota/usage` 是**顶层扁平**的：`userQuota{total,used,remaining,percentage,unit}`、
`addOnQuota` 同形、`dedicatedResourcePackages[]`（带 `name`/`available`）、`isQuotaExceeded`、
`expiresAt`（**epoch 毫秒整数**，不是字符串）。

2026-09-18 实测：`qoder/qfmodel` 输出 6628 tokens → 积分 **Δ0**（免单）；
`qoder/qmodel_38max` 输出 3144 tokens → 积分 **Δ1**（收费，4 折：0.0845 → 0.0338）。
工具调用（`tool_use` → `tool_result` 回填）、思考、流式/非流式，以及真实 Claude Code 打这座桥，
都跑通了。

### 凭据怎么来

`scripts/qoder-auth.mjs`：跑一次官方 CLI 的只读命令（`--list-models`），用
`NODE_OPTIONS=--require` 塞一个 fetch 钩子旁观它自己的 `Authorization: Bearer dt-…`，
再换 `jt-…`（jobToken），写进 `~/.qoder-bridge/auth.json`（0600）。桥侧三条路：`jobToken`
没过期直接用 → 过期用 `accessToken` 换新的 → 都失败才跑配置里的 `refresh_command`。
**永远不调 `/api/v1/deviceToken/refresh`**（那会轮换掉用户桌面端 / CLI 的凭据）。

Windows 那台不走 CLI：桌面应用把会话存在 `%APPDATA%\com.qodercn.app.stable\auth.v1.dat`
（Electron OSCrypt：`v10` + AES-256-GCM；密钥在同目录 `Local State` 的
`os_crypt.encrypted_key`，DPAPI 当前用户可解），脚本 `scripts/qoder-auth-windows.cjs` 解出
`dt-` 再换 `jt-`。`refresh_command` 走一个 `.cmd` 包装（`cmd /C` 对「以引号开头」的命令处理
很别扭，包一层最省事）。自愈实测：把凭据换成一份坏的 + 重启桥 → 一次请求 3.8 秒恢复
（`jobToken` 与 `accessToken` 都被桥自己换新）。
