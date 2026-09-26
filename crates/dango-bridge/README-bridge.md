# antigravity-bridge

自建的 Antigravity 上游桥。目标就是**替掉 Antigravity Tools**（那台闭源桥，2026-09-18 已从这台
机器上删干净），做同样的事，但协议、翻译、错误处理都归我们自己。

**一条主线**：`core/`（Rust 核心）+ `app/`（Tauri 桌面壳）—— **日常在跑的就是它**：
桌面壳（托盘 + 面板窗口）由 launchd 常驻在 `127.0.0.1:8050`，登录自启、挂了自动拉起
（`scripts/appctl`）；headless 的 CLI（`core/src/bin/bridge.rs`）和它共用同一份 core。
早先还有一份 Node 实现当行为基准和回退，2026-09-18 一并删了 —— 要翻只能翻 git 历史。

## 现在到哪了

**Phase 0 已完成（2026-09-17）**：上游协议摸清，并且用我们自己的代码真跑通了一次完整链路
（自己刷新 `access_token` → `loadCodeAssist` → `fetchAvailableModels` → `retrieveUserQuotaSummary`
→ `streamGenerateContent` 拿到真实回复）。证据在 `research/spike-*.json`。

**Phase 1a 已完成（2026-09-18）**：服务层落地（当时是 Node 那版，现在这份是它的 Rust 移植），
真上游自测 5 步全 200，多轮走查 12 个场景里 10 个一次通过、2 个按实测事实修正后通过。
两个月账号的额度都能查（`/quota?all=1`）。

**Rust 移植完成**：`core/` 把服务层逐模块搬完（请求转换 / 流式翻译 / 账号池 / 签名 /
泄漏修复 / 面板），`cargo test --workspace` 300 个全绿，clippy `-D warnings` 与
`cargo fmt --check` 干净；`app/` 是 Tauri 桌面壳（托盘 + 面板窗口，单实例用端口锁 —— 端口上
已有自己的桥时，再启动一次会把它的面板窗口拿到前台；关窗不退出、退出即停，见下面「桌面壳怎么用」）。
真上游验证：`smoke` 二进制自起一座临时桥跑完 6 步（含工具签名回环）。

**已切流**：8050 上现在是 Rust 桌面壳（launchd Label `local.antigravity-bridge.app`），
客户端一行配置都没动（还是 8050，`/version` 里的 `build.git` 能看出现在跑的是哪次编译）。
老 JS 版那份 launchd job、它的代码和它的 `~/.antigravity_tools/` 账号库都清掉了。

**现在的样子**：
- **账号层已上线**：默认用账号池（库里所有可用账号），按额度选号（分组窗口里最紧的那个窗口算分；
  某个家族额度见底、或刚吃过「额度耗尽」的账号排到最后，但真到都没额度时照样挑一个去试）、
  会话粘性（同一会话回到同一账号；额度差太多、或者它那个家族已经没额度了才让位；`default` 这种非真会话不粘）、
  429/403 熔断退避（60s → 5min → 30min → 2h），
  开流前换号重试（一旦给客户端吐过字节就锁死，不拼两段正文）。
  `--email=x@y.com` 可以退回单账号模式。
- **账号库归自己**：`~/.antigravity-bridge/accounts.json` + `accounts/*.json`（0600）。
  老的 `~/.antigravity_tools/` 只在第一次启动时**拷**过来一次（老文件不动、之后不再读），
  `/healthz` 的 `accountsRoot` / `accountsMigratedFrom` 如实报这件事。
- 熔断粒度按事实来：**429 只记到「那个账号的那个模型」**（免费层下 pro 系稳定 429、flash 正常，
  按家族记会误伤），401/403 才是账号级。**唯一的例外是「额度耗尽」**：429 里点名了
  `reason=QUOTA_EXHAUSTED`（或 message 明说超出配额）时，额度本来就是家族级的，于是记成
  「这个账号的这个家族先别派活」，同家族的其它模型一起避开它；下一次额度刷新看到余额就当场解除。
  拿不准的一律按纯限流算（裸 `RESOURCE_EXHAUSTED` 限流也复用，不能据此封家族）—— 宁少封，不误封。
  所有候选都被拒时回一个 429，消息里列出每个账号的状态，并带 `Retry-After`（最早醒来的熔断）。
- 响应头 `x-bridge-account` 告诉你这次是哪个账号服务的（流式响应里也有）。
- `/healthz` 带池状态：每个账号的会话是否建好、熔断中的键与剩余退避秒数、最近一次错误、缓存额度。
- 额度是「现查现报」，不缓存、不用旧数字顶替；`/quota` 分组窗口 + 模型级剩余。
  选号用的是缓存副本，缓存 90 秒算过时，所以：启动时后台预取一次；之后一个 **60 秒的低频定时器**
  补查过期账号（从没查到过快照的也算过期），面板开着时每次 `/healthz` 也顺手排一次 ——
  于是面板上的额度基本是实时的，而「这个账号还有没有额度」在选号时不会用几分钟前的旧数字。
  节流窗口就是那个 90 秒，面板怎么跳都不会把上游当自家缓存打；面板上「现查额度」是真 force，
  点一次就是真打一次上游。
- **预算按模型表收口**：客户端给 30 万，上游那张表说这个模型只吃 6.4 万，我们就按 6.4 万发，
  并在请求日志的 `warnings` 里写明收过（`max_tokens_clamped:300000->64000`）；
  思考预算低于模型下限时抬到下限。拿不到模型表才退回硬编码上限。
- **空回合不算成功**：上游 200 但一个可见字节都没有（实测：小 `max_tokens` 被思考吃满就是这个形状），
  以前会当正常收尾、给客户端一个「模型什么都没说」的回答。现在：客户端**没要过思考**时，
  桥拿同一个账号把思考关掉重试一发（`warnings` 记 `empty_turn_retry:thinking_off`，修好了记
  `empty_turn_repaired`，正文记的是最后那一发）；客户端**明确要了思考**就一次都不重试
  （不许偷偷改它的请求），如实报 502 `api_error` 并说明是空回合（`note: empty_turn`）。
  流式那条路同样处理：已经开出去的流里补一个 `error` 事件，不会静默断在「没说话」上。
  这是「失败后修一次」，不做**预先**的预算干预（客户端要多少给多少，见下面「已知行为」）。
- **错误按事实分类**：候选账号全是网络层失败（连不上/超时/会话建不起来）时报 502 `api_error`，
  不再统一报成 429 `rate_limit_error`（那会让客户端以为被限流，退避很久还没道理）；
  只有真被上游拒（401/403/429）才报 429 并带 `retry-after`；混着时按「有账号真被拒」算 429。
- 路由：`POST /v1/messages`、`POST /v1/chat/completions`、`GET /v1/models`（+ `/models` 别名）、
  `GET /v1/models/{id}`（表里没有但能解析的给 200 并带 `resolved_from`，完全对不上的 404；
  表里给了 `maxOutputTokens` 的模型会一并报 `max_output_tokens`）、
  `GET /version`（带 `build{git,builtAt,target}`，见下面「构建身份与 CI」）、
  `GET /healthz`（含账号池状态、常驻内存、`apiKeyRequired`、`bodyLimit`）、`GET /quota`、
  `GET /logs/recent`（列表，默认不带正文）、`GET /logs/entry?id=<条目 id>`（单条全文，含正文，
  见下面「请求详情」）。
  `/props` 故意不给（语义不明，编一个形状不如 404）。
- **账号写操作**：`GET /control/accounts` 看谁被禁着，`POST /control/accounts`
  （`{"id":"<8 位前缀或完整 id>","disabled":true}`）禁用/启用 —— 只影响**以后**的选号，
  正在跑的那条请求不打断；被禁的账号仍留在池子和 `/healthz` 里（面板标「已禁用」，能点回来）。
  名单落 `~/.antigravity-bridge/disabled-accounts.json`（`BRIDGE_STATE_DIR` 可换目录；没配目录
  就只在内存里，响应如实说 `persisted:false`）；认不出的 id（账号被删了）忽略、不拦启动；
  单账号模式（`--email`）没有第二个号能顶上，如实回 400 而不是假装成功。
- **Qoder CN 直连通道**：`qoder/` 前缀的模型走 Qoder 网关（那边 3.8-Flash 限免，只认客户端
  会话，所以凭据是借官方 CLI 的），非前缀的请求一行都不受影响 —— 见下面「Qoder 上游（CN，自用）」。
- **自用面板**（`GET /`，零构建的一个 HTML）：跑没跑、各账号剩多少额度、熔断在退避什么、
  各计数、最近 25 条请求（谁、哪个模型、换过几次号、用了多久）。三个按钮：刷新、
  现查额度（真打上游）、重启服务（只在 launchd 下放行，否则 409 如实说「退了没人拉起来」）。
  最上面是**接入卡**（给 agent 接线用）：Base URL / 模型名（下拉，来自上游模型表，默认选桥的
  `default_model`）/ 最大输出（该模型的上限，上游没报就如实写「按客户端默认」）/ 三个端点，
  每行一个「复制」按钮；「全部复制」给一段 `baseUrl = …`、`model = …`、`maxOutputTokens = …`
  加端点和口令提示的文本，直接粘进客户端配置。账号卡右上角有「禁用/启用」（走
  `/control/accounts`）。页面本体是 `ui/panel.html`，编译期 `include_str!` 进二进制，
  浏览器里也是同一个界面。桌面壳把它装在一个 Tauri 窗口里（原生标题栏，见下面「桌面壳怎么用」）。
- **launchd 常驻**：登录自启、挂了自动拉起、日志在 `~/Library/Logs/antigravity-bridge/`。
  面板上那个「重启服务」就是靠它 —— 进程敢退出，因为有人拉回来（`scripts/appctl`）。
  「退出」是另一回事：真退的时候会先把 job 注销掉，不然 `KeepAlive` 会立刻把它拉回来。
- **内存**（两个口径都留着，因为 GUI 下它俩差得挺远）：
  - **headless CLI**：预热后 `ps` RSS ~17 MB、物理足迹 **~6.5 MB**（凭据在同一份 4 MB 缓冲里
    流式扫，扫完就还）。
  - **桌面壳**：面板没开时足迹 **~62 MB**（同一时刻 `ps` RSS 报 142 MB —— 它把 WebKit / AppKit
    那些**共享**框架页都算在本进程头上，`vmmap` 的 `Physical footprint` 和我们报的数一分不差）；
    面板开着再 **+70 MB** 上下，其中大头是两个 WebKit 帮手进程（GPU ~29 MB + WebContent ~32 MB），
    窗口一关它们就退。所以面板窗口是**按需创建**的：不看不占。
  - `/healthz` 的 `memory.footprintMb` 就是上面那个足迹（macOS `proc_pid_rusage`，面板读它），
    `memory.rssMb` 保留着（早先 Node 版留下的口径，形状不破）。拿不到就返回 `{}`，绝不编数字。
  - 历史坑（记在这儿）：早期实现每个账号会话都把 146 MB 的官方二进制整个读进内存，RSS 冲到
    **612 MB**；改成 4 MB 分块扫 + 一进程只扫一次之后仍有 176 MB（macOS 分配器不把大块还回去），
    最后丢进短命子进程、父进程只收几 KB 候选才落到 100 MB 上下。Rust 侧没有这个坑。
- 只监听 `127.0.0.1`。要「反代出去」得显式加 `--host=0.0.0.0`（可配 `--api-key`），
  这一步没有做，因为动暴露面要先问。桌面壳只吃 `--port`（不给 `--host`/`--api-key`），
  要反代就用 CLI（`core/src/bin/bridge.rs`）。

已经切过去了：Factory 的 gemini `baseUrl` 从 byok-proxy 的 8046 改成新桥的 8050
（`~/.factory/settings.json`，备份在同目录 `settings.json.bak-bridge-<时间戳>`）。
8050 背后从 Node 版换成 Rust 桌面壳时**没动客户端配置**（端口没变，`/version` 里的
`build.git` 能看出现在是谁在服务）。

## 桌面壳怎么用

这份壳**同时是那座桥本身**，所以「关窗」和「退出」是两件事：

| 动作 | 结果 |
| --- | --- |
| 左键点托盘图标 / 点 Dock 图标 / Finder 里双击 app | 开面板（已经开着就聚焦，不会再开第二个） |
| 桥已经在跑，又启动一次（双击 exe / 开始菜单再点一回） | **把已有实例的面板窗口拿到前台**：第二个实例打 `POST /control/show-panel`（Windows 没有 macOS 的 Reopen 事件，只能这样跨进程打招呼）；对面是 headless CLI 或旧版（没这个接口）才退回「在浏览器中打开」 |
| 托盘右键菜单 | 打开面板 / 在浏览器中打开 / 退出（停掉桥）；面板本来就是 HTTP 页，浏览器里是同一个界面 |
| 点面板窗口的红按钮 / ⌘W | **收起看板**：窗口销毁、WebView 那套内存还回去，桥照跑（日志写一行「面板关了，桥继续跑」） |
| ⌘Q / 菜单栏「退出 Antigravity Bridge」/ 托盘「退出（停掉桥）」/ Dock 菜单里的退出 | **真退**：先 `launchctl bootout` 把 job 注销掉再退，不会一秒后被 `KeepAlive` 拉回来；下次登录照样自启，`scripts/appctl start` 也能立刻拉起 |
| 面板上的「重启服务」 | 进程退出（code 0）→ `KeepAlive` 立刻拉起来 —— 它就该是这样 |

面板窗口是**按需创建**的（不看不建、关掉即销毁），代价只是每次打开重新拉一遍 `/healthz`
（面板本来就是现拉数据的）。窗口走**原生标题栏**，拖动交给系统、红绿灯在标准位置；
app 是常规 app（有 Dock 图标 + 标准菜单栏），不再是只在状态栏里藏着的附件。

## Windows 上也是这份壳

目标 Windows 机上跑的**同样是这份壳**，不再是 headless `bridge.exe`：托盘常驻 +
按需弹出的面板窗口，端口跟着启动参数走（那边 `--port=8045`，客户端的配置一行都没动）。
和 macOS 只有三处不一样，都是平台事实，不是两份实现：

- **开机自启**走 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`（值名 `AntigravityBridge`），
  不是计划任务：同一种「登录就跑」的语义，一条 `reg` 命令读写，托盘里的开关就是调它
  （`app/src/autostart.rs`）。值里带 `--autostart` —— 应用认得这个参数就只进托盘，不弹面板。
- **日志**落 `%LOCALAPPDATA%\antigravity-bridge\logs\`：`app.log`（壳自己的话，超 5 MB 滚成
  `app.log.1`）和 core 的 `requests.jsonl`（32 MB × 5 份，和 macOS 同一套滚动）。
- 托盘右键比 macOS 多两项：**检查更新**、**开机自启**（勾选状态现读注册表，不靠内存里记账）。

第一次装走 NSIS 安装包，按用户装、不要管理员（`installMode=currentUser`）。

## 更新是「这边推、那边自己换」

`app/src/update.rs` 开头写了为什么**不做**「应用从 GitHub 拉」：那个仓库是私有的，私有仓库的
产物下载必须带凭据（细粒度 token 又不认网页下载链接，只能走 `api.github.com` 的资源接口），
等于在那台机器上常驻一个 GitHub 凭据 —— 而于是凭据一步都不出本机：

```bash
# 日常换版本（都在本机跑，不碰 Actions）
scripts/build-on-windows.sh   # 打 bundle 推过去 → 那边 cargo build --release → 换上新 exe 并重启（几分钟）
                              #   --check 看环境，--build-only 只编不装，--force 覆盖脏工作副本
scripts/release.sh 0.2.0      # 想让版本号 / tag 也跟着走：改版本号（两处真值一起）→ 提交 → 打 tag → 推
                              #   （打 tag 之后没有任何自动化了，见下面「构建身份与 CI」）

# 备选：exe 在别处编好了（要留一份安装包、或临时回退到某个版本）时，走「投递 + 它自己换」
scripts/push-update.sh --exe path.exe   # 算 sha256、写清单，scp 进那台机器的 ~\.antigravity-bridge\updates\
```

那边的应用每 2 分钟看一次 `updates/pending.json`（启动 20 秒后先看一次），清单长这样：

```json
{"version":"0.2.0","file":"antigravity-app-0.2.0.exe","sha256":"…64 位十六进制…"}
```

版本比当前高、sha256 也对得上，才交给一个 `.cmd` 帮手：等旧进程退出 → 把新 exe 搬到原位 →
带着原来的参数（`--port=8045 --autostart`）重新拉起来。Windows 允许给**正在运行的 exe 改名**
（锁的是文件对象，不是路径），所以不用先退应用、也不用重启机器。托盘上的「检查更新」是同一件
事的立即版。

两条底线：**清单缺了、或者哈希对不上，就整体不动**（宁可没更新，也不能把一台没人看着的机器上
的应用换成半个文件）；`file` 只接受文件名，清单里带路径直接拒掉。

## Qoder 上游（CN，自用）

除了 Antigravity，这座桥还能把 **`qoder/` 前缀**的模型转发到 Qoder CN 的网关
（`gateway.qoder.com.cn`）。加它是因为那边 **Qwen3.8-Flash（`qoder/qfmodel`）在限免**，
而那个活动**只认 Qoder 客户端/CLI 的会话**（PAT 通道用不了）—— 所以桥不自己登录，而是
**借官方 CLI 已经登好的会话**：`scripts/qoder-auth.mjs` 跑一次 CLI 的只读命令
（`--list-models`，不做推理、不花额度），用 `NODE_OPTIONS=--require` 塞一个 fetch 钩子旁观
它自己的请求头，拿到会话 token 后去换 job token，写进 `~/.qoder-bridge/auth.json`（0600）。

模型 id 带 `qoder/` 前缀的走 Qoder，其它一律原路走 Antigravity；上游模型表里 `enable` 的那些
都能直接点单（10 分钟缓存）：

```
qoder/qfmodel        Qwen3.8-Flash（实测 billable=false、额度不动）
qoder/qmodel_38max   3.8-Max（收费，实测 4 折：0.0845 → 0.0338）
qoder/auto           交给上游挑模型（挑到哪个就按哪个计费）
```

**桥不往里加东西**：你给什么 system，就原样当第一条 `role:"system"` 消息发过去；你没给就不发。
上游自带什么就是什么 —— 实测它会在前面垫一句「你是 Qwen（通义千问）」+ 一个过期的 `CurrentDate`
（2026-07-20 那种）；那只是一句身份 + 日期，**不是 harness**。Q 的 harness（QoderWork / 子代理 /
技能那套）只存在于官方客户端里（见 `docs/PROTOCOL.md` §10），桥一个字节都不带，所以在别的 Agent
里用它不会出现「两套 harness 叠一起、上下文变长」。可选项：`qoder.neutralize`（默认 `false`）；
置 `true` 时垫一条约 90 token 的中性前导 system，把上游那句 Qwen 人设顶掉、并附上桥的当前日期
（只想让模型当「客户端配置的裸模型」时才需要）。

**凭据怎么续**（字段名照 CLI 那份，只在 0600 下读写）：

1. `jobToken` 没过期（提前 5 分钟算过期）→ 直接用。
2. 过期了 → 拿里面的 `accessToken` 打 `POST {openapi}/api/v1/me/jobToken` 换新的，**写回文件**。
3. 都没有、或者上游回 401/403 → 执行配置里的 `refresh_command`（30 秒超时），跑完重读。

**凭据从哪来、要不要人管**：`scripts/qoder-auth.mjs`（macOS/Linux，旁观官方 CLI 的会话）或
`scripts/qoder-auth-windows.cjs`（Windows，从 Qoder 桌面应用的 OSCrypt 凭证里解：`v10` +
AES-256-GCM，密钥是同目录 `Local State` 里 DPAPI 保护的那把，当前用户即可解）。两条路都只**读**
客户端的会话再换 job token，不碰任何轮换接口。**那个 24 小时不需要人管** —— 上面第 2、3 条就是
自动的（`refresh_command` 也是桥自己触发）；只有官方客户端登出/卸载才需要重新登录一次。
实测（2026-09-18，Windows）：把凭据换成一份坏的、重启桥，下一次请求 **3.8 秒内自愈**。

```json
{
  "port": 8050,
  "qoder": {
    "enabled": true,
    "refresh_command": "node /Users/you/code/bridge/antigravity-bridge/scripts/qoder-auth.mjs"
  }
}
```

```bash
node scripts/qoder-auth.mjs --check   # 现在这份还有多久过期（不联网、不改文件）
node scripts/qoder-auth.mjs           # 立刻刷一份（CLI 会话本身过期后就靠它）
```

两条刻意不做的：**永远不调 `/api/v1/deviceToken/refresh`**（那会把用户桌面端/CLI 的凭据
轮换掉，等于把人挤下线），**日志和错误里永远不出现 token 或签名**（`/healthz` 的 `qoder`
只报能不能用、什么时候过期、uid 前 8 位）。额度看 `/quota/qoder`：上游原始 JSON 透传
（`userQuota` / `isQuotaExceeded` / `expiresAt`…），外加一条归一化的 `summary` 给面板。

实测走通的（`31961c3` 这版）：非流式与流式、思考（`thinking` 块 + `thoughts_token_count`）、
工具调用（`tool_use` → `tool_result` 回填后接着出答案）、以及**真实 Claude Code** 走
`ANTHROPIC_BASE_URL=http://127.0.0.1:8050` + `--model qoder/qfmodel`（含 `Read` 工具那一轮）；
全程 `userQuota.used` 1347 → 1347，一分没扣。**图片**是后来补上的（见「接进 Droid」那一节）。

**接进 Droid（本机）**：`~/.factory/settings.json` 的 `customModels` 里加一条就行，Droid 会监听
这个文件，改完不用重启（CLI 里 `/model` 选，桌面壳里在模型选择器的「Custom models」一栏）：

```json
{
  "model": "qoder/qfmodel",
  "displayName": "Qwen3.8-Flash (Qoder)",
  "baseUrl": "http://127.0.0.1:8050",
  "apiKey": "桥配了 api_key 才需要；没配随便填一个占位",
  "provider": "anthropic",
  "maxOutputTokens": 32768,
  "reasoningEffort": "high"
}
```

实测（2026-09-19）：按 Droid 的形状打（流式 + `tools` + `system` + `max_tokens: 32768`）→
HTTP 200 / 2.0 秒，思考块、正文、`signature_delta` 都正常，`stop_reason=end_turn`；
**真 Droid 客户端**跑 `droid exec -m qoder/qfmodel`（含一轮 `Read` 工具调用）也通。
`droid exec` 本身没有贴图入口，图片在客户端聊天界面里贴就行 —— 这条通道已经能吃图了：

**图片**（2026-09-19 起）：`qfmodel` 上游标着 `is_vl: true`，图真能进（实测带图那轮
prompt tokens 71 → 124，回答与图一致）。桥的走法是：客户端照常送 Anthropic 形状的
`{"type":"image","source":{"type":"base64",…}}`，桥先把图传到 Qoder 图床
（`PUT /algo/api/v2/image/upload` → 一张约 30 天有效的签名 OSS URL），再把这一轮的
`content` 从字符串换成 part 数组 `[{type:"text"},{type:"image_url",image_url:{url}}]` ——
实测只有这个 **OpenAI 形状**上游真当图看，Anthropic 的 `image`/`source`、顶层
`image_urls`、markdown 链接都会被无视（细节见 `docs/PROTOCOL.md` §10）。
同一张图按 base64 的 sha256 缓存最近 64 张：客户端每轮重发整段历史，也不会反复上传。
上传失败不拦请求 —— 那张图丢掉，日志里留一条 `image_not_uploaded:`，不闷声。

## 配置文件

除了命令行，常用项还可以写在**可选的**配置文件里：`~/.antigravity-bridge/config.json`
（桌面壳和 headless CLI 共用同一份）。测试或临时换一份用 `BRIDGE_CONFIG=<路径>` 指过去。
这个文件**不是必须的** —— 没有它、字段少写、JSON 写坏了都照常按命令行默认值启动；
坏 JSON 会在启动日志里说一句坏在哪，不会崩、也不会闷声不响。

| 字段 | 类型 | 对应命令行 |
| --- | --- | --- |
| `port` | 数字 | `--port` |
| `host` | 字符串 | `--host` |
| `email` | 字符串 | `--email`（单账号模式） |
| `api_key` | 字符串 | `--api-key` |
| `endpoints` | 字符串数组 | `--endpoint`（数组，按顺序回退） |
| `log_dir` | 字符串 | `--log-dir` |
| `log_bodies` | 布尔 | `--no-log-bodies`（写 `false` 就等于关，见「请求详情」） |
| `body_limit` | 数字 | 正文最多留多少字符（`0` = 不截断，也是默认；只有配置里有这一项） |
| `qoder` | 对象 | Qoder 上游（`enabled` / `auth_file` / `refresh_command` / `base_url` / `machine_id_file` / `neutralize`，见「Qoder 上游（CN，自用）」；只有配置里有这一项） |

**优先级**：命令行显式给的 > 配置文件 > 内建默认值。文件只当「默认值」用，
临时想换一项（比如换端口对拍）照旧敲命令行就行，不必动文件；文件里没写的字段同样退到默认值。
桌面壳只读其中的 `port` 和 `qoder`（`host` / `api_key` / `log_dir` 是 headless CLI 才管的，
壳有自己那套）。
不认识的字段会被忽略（以后加开关时，老文件不用改）。

```json
{
  "port": 8050,
  "email": "you@example.com",
  "endpoints": [
    "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal",
    "https://daily-cloudcode-pa.googleapis.com/v1internal",
    "https://cloudcode-pa.googleapis.com/v1internal"
  ],
  "log_dir": "/Users/you/Library/Logs/antigravity-bridge"
}
```

## 请求详情（日志里记正文）

面板的「最近请求」点任意一行，表格下面会摊开这一条的**全部字段**：账号、试过几次以及每次为什么
不行、警告、`stopReason`、工具调用、用时，外加**请求正文**和**回复正文**。数据分两步拿：
列表走 `/logs/recent`（默认把正文摘掉、留一个 `bodiesOmitted` 记号），点开才按条目 `id`
去 `/logs/entry?id=` 取全文 —— 一条正文最多 16K，二十几条一起塞给面板不合适。
（老条目没带 `id`，面板会直说「只能看列表里这几个字段」，不装作还能取全文。）

正文**默认不截断**（要限就在配置里写 `body_limit`，单位是字符、`0` = 全部保留也就是默认；
`/healthz` 的 `bodyLimit` 报的就是它）：

- **请求**：客户端发来的 JSON。图片那种巨型 base64（`data` 超过 256 字符）先换成一句说明，
  否则一张图就能吃掉整个日志预算；条目里的 `warnings` 照旧写明桥做过哪些修复。
- **回复**：上游给过的正文与工具调用（思考 part 标成 `[思考] …`，免得跟正文混成一坨）；
  真截断了就带 `truncated: true` 和原文大小，面板上写明「已截断（原文共 N 字节，日志里只留了
  前面的 N 字符）」，不让人把残的当全文。同一条条目里还记 `thoughtsTokens` / `outputTokens`：
  这次思考花掉多少、正文剩多少。

默认不截断，是因为「请求详情」的价值就在正文本身；嫌日志长得快就在配置里写 `body_limit: 2000`，
或者干脆不留正文：命令行 `--no-log-bodies` / 配置 `log_bodies: false`（桌面壳固定记正文，要关就
换 CLI 跑）。关掉之后条目里留一个 `"bodies": "off"` 的记号，免得翻日志的人以为是自己丢了包。
日志仍然**滚动**（当前那份超过 32 MB 轮转成 `.1`…`.5`），`/logs/entry` 只看当前那份 —— 滚出去的
历史不在服务范围内。

## 下一步

1. 多跑一段真实会话，把还没抓到现场的那类「空回复」逮到一个：日志里现在带 `id`、请求/回复正文、
   `attempts`、`thoughtsTokens`/`outputTokens`，`empty_turn*` 或 `thoughts_ate_max_tokens` 的标记
   能直接说清是思考吃满还是别的原因。不对劲先看
   `~/Library/Logs/antigravity-bridge/requests.jsonl` 和面板的「最近请求」。
2. 可选，看需要再动：模型别名/阈值可配、`none/own/mimic` 三档 harness 注入开关、
   Gemini 原生 `/v1beta`。（配置文件已经落地，见上面「配置文件」。）
3. 已知行为（不是 bug，记下来免得误判）：
   - 客户端把 `max_tokens` 给得很小时，思考会把预算烧掉（实测 64 → 61 个思考 token、正文 0 字、
     `stop_reason=max_tokens`）。服务层**不预先**改客户端的预算（2026-09-18 试过替它收口思考预算，
     结论是老 Tool 里没有这套、对真实客户端也几乎不触发，撤掉了），但真出现「一个字都没有」的
     回合会修一次：客户端没要思考就关掉思考重试一发（`empty_turn_retry:thinking_off` /
     `empty_turn_repaired`），要了就如实报 502（`note: empty_turn`）。日志里还记
     `thoughtsTokens` / `outputTokens`，这两项足够看清一次到底是「真想完了」还是被预算截断。
   - 上游给思考**正文**是时有时无的（实测同一天 63 条里只有 1 条带思考文本，多数只给
     `thoughtsTokenCount`）。给的时候桥照 Anthropic 规矩发 `thinking` 块，并在收尾补
     `signature_delta`：上游有真签名就用真的，没有就补哨兵 `skip_thought_signature_validator`
     —— 客户端要拿签名才有资格把思考块留住（没签名的块它直接丢，面板上就是「模型没思考」）。
     哨兵在上行方向上游也认（实测 200，返回正常的 thinking + text）。
   - 免费层下 pro 系模型稳定 429，而额度显示还有 99.8%（误导性文案），桥如实报 `rate_limit_error`；
     账号池会先换另一个号试，都 429 才回报给客户端。

## 构建身份与 CI

`GET /version` 除原有的 `name / version / runtime / uptimeSeconds` 外多报一个 `build` 对象，
用来认出「现在跑的到底是哪一次编译」（`version` 一直是 0.1.0，光看它分不出来）：

```json
{"name":"antigravity-bridge","version":"0.1.0","runtime":"rust","uptimeSeconds":5,
 "build":{"git":"b477f59","builtAt":"2026-09-18T02:38:50Z","target":"aarch64-apple-darwin"}}
```

- `git / builtAt / target` 由 `core/build.rs` 在编译期塞成环境变量（`handle_version` 里 `env!` 内联读，
  不新开模块、不加依赖）。取不到 git（没装 git、源码包里没有 `.git`）时 `git` 退化成 `"unknown"`，
  **绝不因此让构建失败**；`builtAt` 优先认 `SOURCE_DATE_EPOCH`（可复现构建），没有就用编译当下的系统时间。
- 桌面壳的版本号不再写死在 `app/Info.plist`：里面的 `__VERSION__` / `__BUILD__` 占位符由
  `scripts/assemble-app.sh` 从根 `Cargo.toml` 的 `[workspace.package] version` 抠出来替换
  （`CFBundleVersion` 按 Apple 的规矩折成整数）。发版只改 Cargo.toml 一处。
- CI 在 `.github/workflows/ci.yml`，**整套只手动触发**（Actions 页面上的 Run workflow）。
  这个仓库是**私有**的，Actions 分钟数是计费的（ubuntu 1 倍、windows 2 倍、macos 10 倍），
  而这几条腿在本机都有等价做法 —— 就是那几条命令，日常直接在这台开发机上跑：
  `cd antigravity-bridge && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
  && cargo test --workspace`；`cd cursor-bridge && cargo fmt --check && cargo clippy --all-targets
  -- -D warnings && cargo test`。Windows 那条腿是 `scripts/build-on-windows.sh`（在那台机器上
  直接编，编得动就说明成立）。留着这个工作流只当「怀疑是两台机器环境差异」时的对照。
- 发版不再有任何自动化（`release.yml` 2026-09-19 删了）：推 `v*` tag 只是打个标记。
  换那台 Windows 上跑的 exe 用 `scripts/build-on-windows.sh`；要 NSIS 安装包（给全新机器装第一次）
  就在那台上装个 Node 手跑 `npx @tauri-apps/cli@2 build`。版本号有**两处真值**：workspace 的
  `Cargo.toml`（`/version` 报的、自动更新比对用的）和 `app/tauri.conf.json`（安装器用的），
  别手改，用 `scripts/release.sh` 一起改。

## 回退

以前那份 Node 实现（连同它的 launchd job 和 `~/.antigravity_tools/` 账号库）2026-09-18 删干净了，
回退到它只能翻 git 历史：`git log --diff-filter=D -- src/bridge/run.mjs` 找到删除那次，
再 `git checkout <删除的父提交> -- src test package.json` 捞回来。
现在这份实现自己有两道网：`core/tests/e2e.rs`（假上游跑真 HTTP/SSE）和 `smoke` 二进制
（真上游自测）。换版本就是换提交：`git checkout <sha> && scripts/appctl build && scripts/appctl restart`。

## 三个最要紧的结论

1. 上游是 Google Cloud Code 的 `v1internal:<方法>`，三个端点按 sandbox → daily → prod 回退。
2. 请求必须带三个头：`User-Agent: antigravity/1.11.5 windows/amd64`、
   `X-Goog-Api-Client: google-cloud-sdk vscode_cloudshelleditor/0.1`、
   `Client-Metadata: {...}`。**少了它们就是 403**，而且报错文案是误导性的
   "You do not have a valid license of this product"。
3. 模型名不能写死，必须每次从 `fetchAvailableModels` 取（上游退役模型时是回一句 200 的纯文本，不报错）。

## 目录

- `core/` —— **Rust 版核心**（服务层，早先 Node 版逐模块移植而来）。
  - `server.rs` —— 路由与推流内核（含 `/control/accounts` 和禁用名单落盘）；`accounts.rs` —— 账号池（选号/熔断/粘性/本地禁用）；
  - `anthropic_request.rs` / `anthropic_stream.rs` / `openai.rs` —— 两侧的进出翻译；
  - `signatures.rs` —— `thoughtSignature` 内存仓库；`leak_repair.rs` / `sanitize.rs` —— 两处修复；
  - `chat.rs` —— 推流内核（空回合修复、错误分类、请求日志装配）；`upstream.rs` / `oauth.rs` ——
    单账号会话、账号库与凭据扫描；`models.rs` / `quota.rs` —— 模型解析、额度；
  - `qoder/` —— **Qoder CN 直连通道**（`mod.rs` 前缀与常量、`cosy.rs` 签名、`encoding.rs`
    私有 base64、`body.rs` 消息与工具映射、`stream.rs` SSE 信封、`auth.rs` 凭据、`session.rs`
    那个 `QoderSession`）；只有 `qoder/` 前缀的请求会走到这儿，其它路径一行都不碰；
  - `runtime.rs` —— 装配层（CLI 和桌面壳共用，状态目录口径也在这儿）；`panel.rs` —— 面板那一页；
  - `config.rs` —— 可选配置文件（命令行永远优先）；`logfile.rs` —— 请求日志的追加与滚动；
  - `build.rs` —— 编译期把 git sha / 编译时间 / target 塞进 `/version`；
  - `core/src/bin/bridge.rs` —— headless CLI（带 `--smoke`）；
  - `core/src/bin/smoke.rs` —— **真上游自测**（默认干跑，`--run` 才真发；`--base` 可打任意一座在跑的桥）；
  - `core/tests/e2e.rs` —— 假上游跑真 HTTP/SSE 的端到端。
- `app/` —— **桌面壳**（Tauri，macOS 和 Windows 共用这一份）：托盘 + 面板窗口；单实例用端口锁
  （端口起不来就在开任何窗口之前退出；是自家实例在跑就把它的面板窗口拿到前台）；窗口按需创建、
  关掉即销毁（关窗≠退出，见「桌面壳怎么用」）。
  `app/src/main.rs` 是装配与主循环，`app/src/tray.rs` 是托盘与窗口，`app/src/update.rs` 是
  Windows 的推送式自动更新，`app/src/autostart.rs` 是 Windows 的开机自启。打包态日志：
  macOS `~/Library/Logs/antigravity-bridge/`，Windows `%LOCALAPPDATA%\antigravity-bridge\logs\`。
- `ui/panel.html` —— 面板本体（编译期 `include_str!` 进二进制）。
- `docs/PROTOCOL.md` —— 上游协议备忘。每条结论标了证据等级（A 官方二进制/公开源码、
  B 桥的运行日志、C 桥的数据文件、V 我们自己实测、D 待验证）。
- `scripts/appctl` —— 桌面壳的 launchd 控制（build/install/start/stop/restart/status/logs/open；
  `install --takeover` 会把占着 8050 的手动进程停掉再装）。
- `scripts/build-app.sh` / `scripts/assemble-app.sh` —— 离线构建裸二进制 / 手工组装 macOS .app
  （这台 Mac 的日常路径，离线、不依赖 tauri-cli；tauri 自带的打包器只在出 Windows 的 NSIS
  安装包时用，而且要在那台 Windows 上跑）。
- `scripts/release.sh` —— 发版：改版本号（`Cargo.toml` + `app/tauri.conf.json` 一起）、提交、
  打 tag、推。打完之后没有任何自动化：换机器上跑的 exe 靠 `build-on-windows.sh`。
- `scripts/build-on-windows.sh` —— **Windows 那条腿的日常路径**：不经过 GitHub Actions，把代码
  打成 git bundle 走 SSH 推给那台机器，在**它本地** `cargo build --release`，再换上 exe 并重启。
  工作副本默认放那台的 `D:\code2\bridge`（`BRIDGE_WINDOWS_SRC` 能改），辅助文件放
  `~\.antigravity-bridge\` 下。
  `--check` 看那边环境、`--setup` 一次性装工具链、`--build-only` 只编不装、`--from-github`
  改成那边自己拉、`--offline` 用缓存依赖、`--force` 覆盖脏工作副本。
- `scripts/push-update.sh` —— 备选路径（exe 在别处编好了）：`--exe` 指过去，算 sha256、写清单，
  `scp` 进目标机器的 `~\.antigravity-bridge\updates\`，让应用自己换上来。
- `scripts/qoder-auth.mjs` —— 借官方 CLI 自己的会话刷新 Qoder 凭据（`--check` 只看状态，
  见「Qoder 上游（CN，自用）」）。
- `research/` —— 二进制词汇表 + spike / smoke / stress 运行报告（报告不进仓库）。

## 怎么跑

```bash
# 日常在跑的就是它
cargo test --workspace               # 300 个测试（core 241 单元 + bridge 4 + smoke 10 + 37 端到端 + 桌面壳 8）
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check
cargo run -p antigravity-core --bin bridge -- --port=8051 --smoke   # headless 起一座（端口躲开常驻那份），起来后自打一发真请求
cargo run -p antigravity-core --bin smoke                             # 干跑：只打印计划，不发任何请求
cargo run -p antigravity-core --bin smoke -- --run                    # 真上游自测：自起一座临时桥（随机端口），6 步全过
cargo run -p antigravity-core --bin smoke -- --run --base=http://127.0.0.1:8050   # 只当客户端，打已经在跑的那座桥
cargo run -p antigravity-core --bin smoke -- --run --only=tool        # 只跑工具回环那两步
scripts/appctl build                 # 离线构建 + 组装 target/AntigravityBridge.app
scripts/appctl install               # 交给 launchd 常驻（登录自启、挂了自动拉起，端口 8050）
scripts/appctl install --takeover    # 端口被手动起的进程占着时：先停掉再装
scripts/appctl status|restart|stop|logs|open

# 客户端把 base_url 指过来即可
#   Anthropic : http://127.0.0.1:8050        （POST /v1/messages）
#   OpenAI    : http://127.0.0.1:8050/v1     （POST /v1/chat/completions）
#   面板      : http://127.0.0.1:8050/       （状态、额度、计数、最近请求、重启按钮）
#   状态      : /healthz（含账号池状态与内存）   /v1/models   /logs/recent
#   详情      : /logs/entry?id=<条目 id>（单条全文，含截断后的正文）
#   额度      : /quota          （当前账号）
#               /quota?all=1    （库里每个账号各查一遍）
#               /quota/qoder    （Qoder 上游的额度，见「Qoder 上游（CN，自用）」）
```

面板和 `scripts/appctl` 都是**本机自用**：只监听 127.0.0.1，不做局域网/中转/多用户那套。
launchd 的 stdout/stderr 在 `~/Library/Logs/antigravity-bridge/`（不放仓库里：早先仓库在
`~/Desktop` 下，macOS 不让 launchd 拉起的进程碰桌面目录，连打开 stdout 文件都会被拒；
现在仓库挪到了 `~/code/bridge`，落点仍是这里）。
请求日志（`requests.jsonl`）分两种情况：桌面壳 / 打包态落 `~/Library/Logs/antigravity-bridge/`，
开发态（`cargo run`）落仓库的 `logs/`；面板的「最近请求」读的就是它。
**会滚动**：当前那份超过 32 MB 就轮转成 `requests.jsonl.1`（旧的依次 `.2` … `.5`，只留 5 份），
正常追加只在内存里记账、不为了判断阈值每行都 `stat` 一次；滚动失败也照样把这一行追加进去，
并把原因报回调用方（日志写不进去是自用服务最该知道的事之一）。`/logs/recent` 读的始终是
当前那份 `requests.jsonl`，语义没变 —— 每条带一个 `id`，正文按需去 `/logs/entry` 取
（见上面「请求详情」）。

只想知道协议、不想发请求，就直接读 `docs/PROTOCOL.md`。

## 边界（重要）

- **不改任何系统代理 / 网络设置**，不碰 Clash/Surge/hosts/DNS/环境变量里的代理。
- **只读**官方 App 的二进制（扫 OAuth 客户端凭据用；只读、不打印、不落盘、不进仓库）。
  账号库是**我们自己的** `~/.antigravity-bridge/`；老的 `~/.antigravity_tools/` 只在第一次启动时
  拷一次（原文件不动），之后不再读。
- 凭据只读、不打印、不落盘、不进仓库。spike 报告写盘前会断言不含 token 原文。
- 上游是**未公开接口**，随时会变；这就是每条结论都要标证据等级的原因。
