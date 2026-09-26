//! 服务层：对外说 Anthropic / OpenAI 的协议，对内说 v1internal。
//!
//! 移植自 `src/bridge/server.mjs`（JS 版是行为基准）。路由清单两边必须一字不差：
//!
//! ```text
//! POST /v1/messages            Anthropic Messages（流式 + 整包；整包也带 x-bridge-model/-account）
//! POST /v1/chat/completions    OpenAI Chat Completions（流式 + [DONE]）
//! GET  /v1/models, /models     上游真实模型表（/models 是老桥日志里客户端直接打的别名）
//! GET  /v1/models/{id}         单个模型；表里没有但能解析的 200 + resolved_from
//! GET  /version, /healthz, /quota, /
//! GET  /logs/recent            最近请求（列表；默认不带正文）
//! GET  /logs/entry?id=<条目 id> 单条全文（含截断后的正文；没 id / 滚掉了就 404）
//! POST /control/refresh-quota, /control/restart, /control/accounts
//! GET  /control/accounts      谁被本地禁用了（面板/脚本用；POST 是禁用、启用）
//! ```
//!
//! 两条协议共用一个「推流内核」（pump）：只要还没往客户端吐过一个 chunk，就允许换账号重试；
//! 一旦吐过（流式的响应头也就写出去了），就锁死当前账号，失败如实报错。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Map, Value};

use crate::accounts::{
    AccountPool, Candidate, CandidateQuery, FailureInfo, PoolState, SingleAccountPool,
};
use crate::logfile::append_line;
use crate::panel::render_panel;
use crate::qoder::{self, UpstreamKind};
use crate::quota::model_limits;
use crate::signatures::{sig_stats, SharedSignatures};
use crate::types::{now_millis, Account, QuotaSnapshot, Session, UpstreamError};

/// JS 版常驻服务的 launchd Label（`scripts/bridgectl` 装的那份 plist 用的是它）。
pub const LAUNCHD_LABEL: &str = "local.antigravity-bridge";

/// 桌面壳的 launchd Label（`scripts/appctl` 装的那份 plist 用的是它）。
///
/// 两个 Label 不一样是因为它俩要能同时挂着（对拍时 8050 一份、8051 一份）。
/// 但「我退了有人拉回来」这件事两边都成立，所以 `restart_allowed` 得同时认这两个：
/// 早先只认 JS 那个 Label，桌面壳装到 launchd 下面板的重启按钮照样 409。
pub const LAUNCHD_LABEL_APP: &str = "local.antigravity-bridge.app";

/// 请求体上限（和 JS 版一致）：整包里可能有几十张图的 base64。
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

pub type LogFn = Arc<dyn Fn(String) + Send + Sync>;

// ---------------------------------------------------------------- 错误映射

/// 映射后的错误（对齐 JS 的 `{ http, type, message, retryAfterSeconds }`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedError {
    pub http: u16,
    pub kind: String,
    pub message: String,
    pub retry_after_seconds: Option<i64>,
}

impl MappedError {
    fn new(http: u16, kind: &str, message: impl Into<String>) -> Self {
        Self {
            http,
            kind: kind.to_string(),
            message: message.into(),
            retry_after_seconds: None,
        }
    }
}

/// Google 的错误 → Anthropic 的错误类型。上游文案照抄，类型按语义给。
pub fn map_upstream_error(status: u16, reason: &str, message: &str) -> MappedError {
    let text = message.to_string();
    match status {
        400 => MappedError::new(
            400,
            "invalid_request_error",
            if text.is_empty() {
                "上游拒绝了请求（400）".into()
            } else {
                text
            },
        ),
        401 => MappedError::new(
            401,
            "authentication_error",
            if text.is_empty() {
                "上游不认这个 access_token（401）".into()
            } else {
                text
            },
        ),
        403 => {
            let fallback = format!("上游拒绝了这个账号（403 {reason}）");
            MappedError::new(
                403,
                "permission_error",
                if text.is_empty() { fallback } else { text },
            )
        }
        404 => MappedError::new(
            404,
            "not_found_error",
            if text.is_empty() {
                "上游没有这个方法（404）".into()
            } else {
                text
            },
        ),
        429 => MappedError::new(
            429,
            "rate_limit_error",
            if text.is_empty() {
                "上游限流（429）".into()
            } else {
                text
            },
        ),
        _ => {
            if status >= 500 {
                MappedError::new(
                    502,
                    "overloaded_error",
                    if text.is_empty() {
                        format!("上游 {status}")
                    } else {
                        text
                    },
                )
            } else {
                let fallback = if status == 0 {
                    "上游错误（网络）".to_string()
                } else {
                    format!("上游错误（{status}）")
                };
                MappedError::new(
                    502,
                    "api_error",
                    if text.is_empty() { fallback } else { text },
                )
            }
        }
    }
}

impl From<&UpstreamError> for MappedError {
    fn from(err: &UpstreamError) -> Self {
        map_upstream_error(err.status, &err.reason, &err.message)
    }
}

/// OpenAI 的错误外形（type 的取值和 Anthropic 不完全一样）。
pub fn map_openai_error(mapped: &MappedError) -> Value {
    let kind = match mapped.http {
        401 => "authentication_error",
        403 => "permission_error",
        400 => "invalid_request_error",
        429 => "rate_limit_error",
        _ => "server_error",
    };
    json!({
        "message": mapped.message,
        "type": kind,
        "code": if mapped.http == 429 { Value::String("rate_limit_exceeded".into()) } else { Value::Null },
    })
}

/// 一句话诊断：这次收尾是不是「思考把预算吃掉了」。实测过的坑，写进日志免得下次再查。
pub fn budget_note(stats: &Value) -> Option<String> {
    if stats.get("finishReason").and_then(Value::as_str) != Some("MAX_TOKENS") {
        return None;
    }
    let thoughts = stats
        .get("usage")
        .and_then(|u| u.get("thoughts_token_count"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let output = stats
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if thoughts <= 0.0 {
        return Some("hit_max_tokens".to_string());
    }
    Some(
        if thoughts >= output * 0.5 {
            "thoughts_ate_max_tokens"
        } else {
            "hit_max_tokens"
        }
        .to_string(),
    )
}

/// 常驻内存看一眼就够（自用服务，最关心「这个进程占多大」）。
///
/// 两个口径都报，因为它俩差得挺远、说的也不是一回事：
///
/// - `rssMb`：`ps` 的 RSS，和 JS 版同一个口径（保留它是为了形状不破）。
///   它把 WebKit / AppKit 那些**共享**框架页也算在本进程头上 —— 桌面壳明明是
///   75MB 的物理足迹，`ps` 能报成 170MB+，看着像漏了内存，其实没有。
/// - `footprintMb`（只有 macOS 有）：物理足迹 `ri_phys_footprint`，
///   也就是 Activity Monitor「内存」那一列，只算本进程自己的页。面板优先读它。
///
/// 拿不到就返回 `{}`，面板自己会省掉内存那一段 —— 绝不编一个数字出来。
pub fn memory_stats() -> Value {
    let mut stats = Map::new();
    if let Some(kb) = footprint_kb() {
        stats.insert("footprintMb".to_string(), mb(kb));
    }
    if let Some(kb) = resident_kb() {
        stats.insert("rssMb".to_string(), mb(kb));
    }
    Value::Object(stats)
}

fn mb(kb: u64) -> Value {
    json!(((kb as f64 / 1024.0) * 10.0).round() / 10.0)
}

/// 给日志用的一行内存说明（`（内存 23.4MB）`），口径同 [`memory_stats`]：优先物理足迹，
/// 退回 RSS；都拿不到就空串（不编数字）。
pub fn memory_note() -> String {
    let stats = memory_stats();
    let mb = stats
        .get("footprintMb")
        .or_else(|| stats.get("rssMb"))
        .and_then(Value::as_f64);
    match mb {
        Some(mb) => format!("（内存 {mb}MB）"),
        None => String::new(),
    }
}

/// macOS 的物理足迹（返回 KB）。`proc_pid_rusage` 是 Darwin libproc 的接口，
/// 签名在 libc 里（`libc::rusage_info_v4` 的 `ri_phys_footprint`）。
#[cfg(target_os = "macos")]
fn footprint_kb() -> Option<u64> {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let pid = std::process::id() as libc::pid_t;
    // SAFETY: `info` 是栈上对齐的 POD，全零是合法初值；`proc_pid_rusage` 只往里写，
    // 写多少由 flavor 决定。返回非 0 就是没写成，下面直接返回、不读 `info`。
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            &mut info as *mut libc::rusage_info_v4 as *mut libc::rusage_info_t,
        )
    };
    if rc != 0 {
        return None;
    }
    let kb = info.ri_phys_footprint / 1024;
    (kb > 0).then_some(kb)
}

#[cfg(not(target_os = "macos"))]
fn footprint_kb() -> Option<u64> {
    None
}

/// 跨平台但不引依赖的兜底：Unix 问 `ps`、Windows 问 `tasklist`（都是系统自带）。
#[cfg(unix)]
fn resident_kb() -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

#[cfg(windows)]
fn resident_kb() -> Option<u64> {
    // `"antigravity-bridge.exe","1234","Console","1","12,345 K"` —— 最后一个字段是工作集
    let out = std::process::Command::new("tasklist")
        .args([
            "/FI",
            &format!("PID eq {}", std::process::id()),
            "/FO",
            "CSV",
            "/NH",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let field = text.split(',').next_back()?.trim().trim_matches('"');
    let digits: String = field
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(char::is_ascii_digit)
        .collect();
    digits.parse::<u64>().ok()
}

/// 能不能在请求里自杀重启：只有**咱们自己的 launchd 任务**拉起的进程才敢退（退了会有人拉回来）。
///
/// 注意不能只看「有没有 XPC_SERVICE_NAME」—— 桌面 App 拉起的 shell 里也可能有它，
/// 那种进程退掉同样没人管。所以认 Label（JS 版一个、桌面壳一个）；手动跑想放行就
/// `BRIDGE_ALLOW_RESTART=1`。
pub fn restart_allowed(env_xpc: Option<&str>, allow_restart: Option<&str>) -> bool {
    matches!(env_xpc, Some(LAUNCHD_LABEL) | Some(LAUNCHD_LABEL_APP)) || allow_restart == Some("1")
}

// ---------------------------------------------------------------- 计数器

/// `/healthz` 里那份 counters（键名和 JS 版一字不差，面板直接显示键名）。
#[derive(Debug, Default)]
pub struct Counters {
    requests: AtomicU64,
    streamed: AtomicU64,
    buffered: AtomicU64,
    errors: AtomicU64,
    leaks_repaired: AtomicU64,
    leaks_ignored: AtomicU64,
    sanitized: AtomicU64,
    filled: AtomicU64,
    tool_signature_hits: AtomicU64,
    tool_signature_misses: AtomicU64,
    account_switches: AtomicU64,
}

impl Counters {
    fn bump(&self, slot: &AtomicU64, by: u64) {
        if by > 0 {
            slot.fetch_add(by, Ordering::Relaxed);
        }
    }

    pub fn requests(&self) {
        self.bump(&self.requests, 1);
    }
    pub fn streamed(&self) {
        self.bump(&self.streamed, 1);
    }
    pub fn buffered(&self) {
        self.bump(&self.buffered, 1);
    }
    pub fn errors(&self) {
        self.bump(&self.errors, 1);
    }
    pub fn sanitized(&self) {
        self.bump(&self.sanitized, 1);
    }
    pub fn filled(&self) {
        self.bump(&self.filled, 1);
    }
    pub fn leaks(&self, repaired: u64, ignored: u64) {
        self.bump(&self.leaks_repaired, repaired);
        self.bump(&self.leaks_ignored, ignored);
    }
    pub fn signatures(&self, hits: u64, misses: u64) {
        self.bump(&self.tool_signature_hits, hits);
        self.bump(&self.tool_signature_misses, misses);
    }
    pub fn account_switch(&self) {
        self.bump(&self.account_switches, 1);
    }

    pub fn snapshot(&self) -> Value {
        let n = |slot: &AtomicU64| slot.load(Ordering::Relaxed);
        json!({
            "requests": n(&self.requests),
            "streamed": n(&self.streamed),
            "buffered": n(&self.buffered),
            "errors": n(&self.errors),
            "leaksRepaired": n(&self.leaks_repaired),
            "leaksIgnored": n(&self.leaks_ignored),
            "sanitized": n(&self.sanitized),
            "filled": n(&self.filled),
            "toolSignatureHits": n(&self.tool_signature_hits),
            "toolSignatureMisses": n(&self.tool_signature_misses),
            "accountSwitches": n(&self.account_switches),
        })
    }
}

// ---------------------------------------------------------------- 账号池

/// 两种池子的统一外形。用 enum 而不是 trait：池子的方法里有泛型（`with_any_session`）
/// 和 `self: Arc<Self>`，做成 trait 对象要么不对象安全、要么得绕一圈；只有两个实现，
/// 直接 match 更直白，也和 JS 里「鸭子类型」的用法等价。
pub enum Pool {
    Multi(Arc<AccountPool>),
    Single(Arc<SingleAccountPool>),
}

impl Pool {
    /// 本地禁用的账号（完整 id，有序）。单账号模式没有这个概念，永远是空的。
    pub fn disabled_ids(&self) -> Vec<String> {
        match self {
            Pool::Multi(p) => p.disabled_ids().into_iter().collect(),
            Pool::Single(_) => Vec::new(),
        }
    }

    /// 禁用/启用一个账号（id 或其前缀）。单账号模式如实拒绝 —— `--email` 是显式点名，
    /// 没有第二个号能顶上，禁掉它等于把桥关掉。
    pub fn set_disabled(&self, prefix: &str, disabled: bool) -> Result<String, String> {
        match self {
            Pool::Multi(p) => p.set_disabled(prefix, disabled),
            Pool::Single(_) => Err("单账号模式（--email）没有账号池，禁用/启用不适用".to_string()),
        }
    }

    /// 启动时灌入落盘的禁用名单（单账号模式下无事可做）。
    pub fn set_disabled_ids(&self, ids: impl IntoIterator<Item = String>) {
        match self {
            Pool::Multi(p) => p.set_disabled_ids(ids),
            Pool::Single(_) => {
                let _ = ids;
            }
        }
    }

    pub fn candidates(&self, query: CandidateQuery) -> Vec<Candidate> {
        match self {
            Pool::Multi(p) => p.candidates(query),
            Pool::Single(p) => {
                let _ = query;
                p.candidates()
            }
        }
    }

    pub async fn open(&self, id: &str) -> Result<Arc<dyn Session>, UpstreamError> {
        match self {
            Pool::Multi(p) => p.open(id).await,
            Pool::Single(p) => Ok(p.open().await),
        }
    }

    pub async fn session(&self, id: &str) -> Result<Arc<dyn Session>, UpstreamError> {
        match self {
            Pool::Multi(p) => p.session(id).await,
            Pool::Single(p) => Ok(p.session()),
        }
    }

    pub fn note_success(&self, id: &str) {
        match self {
            Pool::Multi(p) => p.note_success(id),
            Pool::Single(p) => p.note_success(id),
        }
    }

    pub fn note_failure(&self, id: &str, info: FailureInfo) {
        match self {
            Pool::Multi(p) => {
                p.note_failure(id, info);
            }
            Pool::Single(p) => {
                p.note_failure(id, info);
            }
        }
    }

    pub fn remember(&self, session_key: &str, id: &str) {
        match self {
            Pool::Multi(p) => p.remember(session_key, id),
            Pool::Single(p) => p.remember(session_key, id),
        }
    }

    /// JS 里传的是 `{ model, family }`；Rust 版由 `family_of_model` 自己算，语义一致。
    pub fn earliest_retry_in_seconds(&self, model: Option<&str>) -> Option<i64> {
        match self {
            Pool::Multi(p) => p.earliest_retry_in_seconds(model),
            Pool::Single(p) => p.earliest_retry_in_seconds(model),
        }
    }

    pub fn note_quota(&self, id: &str, quota: &QuotaSnapshot) {
        match self {
            Pool::Multi(p) => p.note_quota(id, quota),
            Pool::Single(p) => p.note_quota(id, quota),
        }
    }

    pub fn id_by_email(&self, email: &str) -> Option<String> {
        match self {
            Pool::Multi(p) => p.id_by_email(email),
            Pool::Single(p) => p.id_by_email(email),
        }
    }

    /// `/control/login` 加完账号后：让池子重读账号库吃进新号。
    /// 单账号模式（--email）没有这个语义，返回 0。
    pub fn rescan_accounts(&self) -> usize {
        match self {
            Pool::Multi(p) => p.rescan_accounts(),
            Pool::Single(_) => 0,
        }
    }

    pub fn primary_id(&self) -> Option<String> {
        match self {
            Pool::Multi(p) => p.primary_id(),
            Pool::Single(p) => p.primary_id(),
        }
    }

    pub async fn identity(&self) -> Value {
        match self {
            Pool::Multi(p) => p.identity().await,
            Pool::Single(p) => p.identity().await,
        }
    }

    pub fn state(&self) -> Vec<PoolState> {
        match self {
            Pool::Multi(p) => p.state(),
            Pool::Single(p) => p.state(),
        }
    }

    pub fn refresh_all_in_background(&self, force: bool) {
        match self {
            Pool::Multi(p) => Arc::clone(p).refresh_all_in_background(force),
            Pool::Single(p) => Arc::clone(p).refresh_all_in_background(force),
        }
    }

    /// `/healthz` 顺带做的自动刷新：把过期的额度在后台补查，不改前端轮询的节奏。
    pub fn refresh_stale_in_background(&self) {
        match self {
            Pool::Multi(p) => Arc::clone(p).refresh_stale_in_background(),
            Pool::Single(p) => Arc::clone(p).refresh_stale_in_background(),
        }
    }

    /// 和 JS 的 `withAnySession` 同一套选号顺序：先按「中性模型」排序候选，
    /// 挨个建会话试到成功为止（模型表、模型解析这类与账号无关的调用走这里）。
    pub async fn any_session(
        &self,
        wanted: Option<&str>,
    ) -> Result<Arc<dyn Session>, UpstreamError> {
        let _ = wanted;
        let mut last: Option<UpstreamError> = None;
        for cand in self.candidates(CandidateQuery::for_model("gemini-3.6-flash-high")) {
            match self.session(&cand.id).await {
                Ok(session) => return Ok(session),
                Err(err) => last = Some(err),
            }
        }
        Err(last.unwrap_or_else(|| UpstreamError::network("no_account", "池里没有可用账号")))
    }

    pub async fn models(&self) -> Option<Value> {
        let session = self.any_session(None).await.ok()?;
        session.models().await.ok()
    }

    pub async fn resolve(&self, wanted: &str) -> Option<crate::types::ResolvedModel> {
        let session = self.any_session(Some(wanted)).await.ok()?;
        Some(session.resolve_model(wanted).await)
    }
}

// ---------------------------------------------------------------- 服务

pub struct BridgeOptions {
    /// 账号池（多账号 + 熔断 + 粘性）。Tauri 壳和 `serve()` 用它。
    pub pool: Pool,
    /// Qoder CN 上游池（`Pool::Single`）。`None` = 没配置：`qoder/` 前缀的请求会明确报未启用。
    /// 单账号包装，它的 `note_*` / `refresh_*` 都是 no-op，不会污染 Antigravity 的账号池。
    pub qoder: Option<Pool>,
    pub store: SharedSignatures,
    pub log_dir: Option<PathBuf>,
    /// 面板上「禁用」的账号记在哪儿（`disabled-accounts.json`）。`None` = 只在内存里，
    /// 重启就忘。目录的挑选（`BRIDGE_STATE_DIR` → `~/.antigravity-bridge`）归 runtime 层。
    pub state_dir: Option<PathBuf>,
    /// 请求日志里要不要记正文（客户端请求 + 模型回复，各自有界截断）。默认开 —— 面板的
    /// 「请求详情」要能看正文才叫详情；不想留就 `--no-log-bodies` / 配置里写 `log_bodies: false`。
    pub log_bodies: bool,
    /// 正文截断字符上限，0 表示不截断全部保留
    pub body_limit: usize,
    /// 非空就必须带对（`x-api-key` 或 `Authorization: Bearer`）
    pub api_key: Option<String>,
    /// `None` 表示按环境判断（见 `restart_allowed`），测试可以钉死
    pub allow_restart: Option<bool>,
    pub log: LogFn,
    pub version: String,
}

/// 本地禁用名单的文件名（放在 `state_dir` 里）。
pub const DISABLED_STATE_FILE: &str = "disabled-accounts.json";

/// 桌面壳注册进来的「把面板窗口拿到前台」。headless（CLI）不注册 —— 那种进程没有窗口可开，
/// `/control/show-panel` 会如实拒绝。用 `Fn` 而不是 `FnOnce`：喊几次都行。
pub type PanelShowFn = Arc<dyn Fn() + Send + Sync + 'static>;

/// 桥的一次运行：状态全在这里，axum 的 Router 只是它的一个方法。
pub struct Bridge {
    pool: Pool,
    /// Qoder CN 上游池（可选）。路由判据见 [`Bridge::upstream_for`]。
    qoder: Option<Pool>,
    store: SharedSignatures,
    log_dir: Option<PathBuf>,
    /// 本地禁用名单的落盘路径（没配 state_dir 就是 None == 不落盘）
    disabled_file: Option<PathBuf>,
    /// 请求日志要不要记正文（chat.rs 写条目时按它决定，见 `--no-log-bodies`）
    pub(crate) log_bodies: bool,
    /// 正文截断字符上限，0 表示全部保留
    pub(crate) body_limit: usize,
    api_key: Option<String>,
    allow_restart: Option<bool>,
    /// 桌面壳注册的「把面板窗口拿到前台」（见 [`Bridge::set_panel_show`]）；
    /// headless 座是空的 —— 端口被占时第二个实例靠它跨进程打招呼
    panel_show: std::sync::OnceLock<PanelShowFn>,
    pub log: LogFn,
    version: String,
    started_at: i64,
    pub counters: Counters,
}

impl Bridge {
    pub fn new(opts: BridgeOptions) -> Self {
        let disabled_file = opts.state_dir.map(|dir| dir.join(DISABLED_STATE_FILE));
        let bridge = Self {
            pool: opts.pool,
            qoder: opts.qoder,
            store: opts.store,
            log_dir: opts.log_dir,
            disabled_file,
            log_bodies: opts.log_bodies,
            body_limit: opts.body_limit,
            api_key: opts.api_key,
            allow_restart: opts.allow_restart,
            panel_show: std::sync::OnceLock::new(),
            log: opts.log,
            version: opts.version,
            started_at: now_millis(),
            counters: Counters::default(),
        };
        bridge.load_disabled();
        bridge
    }

    /// 启动时把落盘的禁用名单读回来。文件不存在是常态（没人禁用过）；坏了就当空的 ——
    /// 一份附属名单不该拦住整条桥起来，但得留一行日志。
    fn load_disabled(&self) {
        let Some(path) = &self.disabled_file else {
            return;
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return;
        };
        let items = match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Array(items)) => items,
            Ok(Value::Object(map)) => map
                .get("disabled")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            _ => {
                (self.log)(format!("禁用名单读不动（{}），先当空的", path.display()));
                return;
            }
        };
        let ids: Vec<String> = items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let listed = ids.len();
        self.pool.set_disabled_ids(ids);
        // 池子会丢掉认不出的 id（账号被删了、换了），所以按「实际生效」报数
        let applied = self.pool.disabled_ids().len();
        if applied > 0 {
            let stale = if listed > applied {
                format!("，名单里另外 {} 个已不在账号库里，忽略了", listed - applied)
            } else {
                String::new()
            };
            (self.log)(format!(
                "禁用名单：{} 个账号被面板禁着（{}{}）",
                applied,
                path.display(),
                stale
            ));
        }
    }

    /// 把当前禁用名单写回磁盘。原子写（先写 .tmp 再 rename），免得半截文件把下次启动搞坏。
    /// `Ok(None)` = 这条桥没配 state_dir，名单只在内存里 —— 不算失败，如实告诉调用方。
    fn save_disabled(&self) -> Result<Option<PathBuf>, String> {
        let Some(path) = &self.disabled_file else {
            return Ok(None);
        };
        let ids: Vec<String> = self.pool.disabled_ids().into_iter().collect();
        let body = serde_json::to_string_pretty(&json!({
            "note": "面板 / POST /control/accounts 禁用的账号；删掉这个文件等于全部启用",
            "disabled": ids,
        }))
        .map_err(|e| format!("序列化失败：{e}"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("建目录 {} 失败：{e}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|e| format!("写 {} 失败：{e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("落位 {} 失败：{e}", path.display()))?;
        Ok(Some(path.clone()))
    }

    /// 能不能在请求里自杀重启（allow_restart = None 时看环境）。
    pub fn can_restart(&self) -> bool {
        match self.allow_restart {
            Some(value) => value,
            None => restart_allowed(
                std::env::var("XPC_SERVICE_NAME").ok().as_deref(),
                std::env::var("BRIDGE_ALLOW_RESTART").ok().as_deref(),
            ),
        }
    }

    /// 桌面壳装配时注册「把面板窗口拿到前台」。只认第一次注册 —— 后来的静默忽略，
    /// 免得两处代码抢一个钩子还看不出谁赢。
    pub fn set_panel_show(&self, f: PanelShowFn) {
        let _ = self.panel_show.set(f);
    }

    pub fn router(self: &Arc<Self>) -> Router {
        // Routes whose handler never reads the body never went through
        // `read_json_body`, so chat-only `x-api-key` checks used to leave them
        // open. Now the key middleware covers them: `/v1/*` stays keyed at the
        // handler level (chat does its own key read), while side-effecting
        // local-only endpoints share the same check via the middleware.
        let key_guard = middleware::from_fn_with_state(Arc::clone(self), require_api_key);
        Router::new()
            .route("/", get(handle_panel))
            .route("/healthz", get(handle_healthz))
            .route("/quota", get(handle_quota))
            .route("/quota/qoder", get(handle_quota_qoder))
            .route("/version", get(handle_version))
            .route("/v1/models", get(handle_models))
            .route("/models", get(handle_models))
            .route("/v1/models/{id}", get(handle_model))
            .route("/v1/messages", post(handle_messages))
            .route("/v1/chat/completions", post(handle_chat))
            // Routes below carry the key when one is configured.
            .route("/logs/recent", get(handle_logs).layer(key_guard.clone()))
            .route(
                "/logs/entry",
                get(handle_log_entry).layer(key_guard.clone()),
            )
            .route(
                "/control/refresh-quota",
                post(handle_refresh_quota).layer(key_guard.clone()),
            )
            .route(
                "/control/restart",
                post(handle_restart).layer(key_guard.clone()),
            )
            .route(
                "/control/show-panel",
                post(handle_show_panel).layer(key_guard.clone()),
            )
            .route(
                "/control/accounts",
                get(handle_accounts)
                    .post(handle_accounts_write)
                    .layer(key_guard.clone()),
            )
            .route(
                "/control/login",
                get(handle_login_status)
                    .post(handle_login_start)
                    .layer(key_guard),
            )
            .fallback(handle_not_found)
            .layer(middleware::from_fn(cors_middleware))
            .with_state(Arc::clone(self))
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// 这次请求该走哪个上游。判据只有一条：模型 id 是不是以 `qoder/` 开头。
    pub fn upstream_for(&self, requested_model: &str) -> UpstreamKind {
        if qoder::is_qoder_model(requested_model) {
            UpstreamKind::Qoder
        } else {
            UpstreamKind::Antigravity
        }
    }

    /// 那个上游对应的池子。Qoder 没配置时退回 Antigravity 池（调用方应先看 [`Bridge::qoder`]）。
    pub fn pool_of(&self, kind: UpstreamKind) -> &Pool {
        match kind {
            UpstreamKind::Qoder => self.qoder.as_ref().unwrap_or(&self.pool),
            UpstreamKind::Antigravity => &self.pool,
        }
    }

    /// Qoder 池（没配置就是 `None`）。
    pub fn qoder(&self) -> Option<&Pool> {
        self.qoder.as_ref()
    }

    pub fn store(&self) -> &SharedSignatures {
        &self.store
    }

    fn uptime_seconds(&self) -> i64 {
        (now_millis() - self.started_at) / 1000
    }

    /// 写一行请求日志（`requests.jsonl`）。log_dir 没配就什么都不做。
    ///
    /// 超过上限时的滚动（转成 `.1/.2/…`）在 [`crate::logfile`] 里做；这里只负责把失败
    /// 原样报出来 —— 日志写不进去是自用服务最该知道的事之一，滚动失败也一样。
    pub(crate) async fn write_log(&self, entry: &Value) {
        let Some(dir) = &self.log_dir else { return };
        let line = format!("{entry}\n");
        if let Err(err) = append_line(dir, &line).await {
            (self.log)(format!("写日志失败：{}", truncate(&err.to_string(), 120)));
        }
    }
}

/// 起一个服务（Tauri 壳用它拿到 addr 后自己去 serve；测试用 `serve`）。
pub async fn serve(
    bridge: Arc<Bridge>,
    host: &str,
    port: u16,
) -> std::io::Result<std::net::SocketAddr> {
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    let addr = listener.local_addr()?;
    let app = bridge.router();
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            (bridge.log)(format!("服务退出：{err}"));
        }
    });
    Ok(addr)
}

pub fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

// ---------------------------------------------------------------- 中间件

async fn cors_middleware(req: Request, next: Next) -> Response {
    if req.method() == axum::http::Method::OPTIONS {
        return Response::builder()
            .status(204)
            .header("access-control-allow-origin", "*")
            .header(
                "access-control-allow-methods",
                "GET, POST, PUT, DELETE, OPTIONS, HEAD",
            )
            .header("access-control-allow-headers", "*")
            .header("access-control-max-age", "86400")
            .body(Body::empty())
            .unwrap_or_else(|_| Response::new(Body::empty()));
    }
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        axum::http::HeaderValue::from_static("GET, POST, PUT, DELETE, OPTIONS, HEAD"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        axum::http::HeaderValue::from_static("*"),
    );
    resp
}

// ---------------------------------------------------------------- 响应助手

pub(crate) fn json_response(status: u16, payload: &Value, extra: &[(&str, String)]) -> Response {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    let mut builder = Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("content-length", body.len().to_string());
    for (name, value) in extra {
        builder = builder.header(*name, value);
    }
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Anthropic 形状的错误体。
fn anthropic_error(status: u16, kind: &str, message: &str, retry_after: Option<i64>) -> Response {
    let payload = json!({ "type": "error", "error": { "type": kind, "message": message } });
    let retry = retry_after.map(|s| ("retry-after", s.to_string()));
    match retry {
        Some((name, value)) => json_response(status, &payload, &[(name, value)]),
        None => json_response(status, &payload, &[]),
    }
}

// ---------------------------------------------------------------- 路由

async fn handle_panel(State(bridge): State<Arc<Bridge>>) -> Response {
    let html = render_panel(&bridge.version);
    Response::builder()
        .status(200)
        .header("content-type", "text/html; charset=utf-8")
        .header("content-length", html.len().to_string())
        // 面板是自用工具，改了要立刻看见：不让 WebView/浏览器缓存
        .header("cache-control", "no-store")
        .body(Body::from(html))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

async fn handle_version(State(bridge): State<Arc<Bridge>>) -> Response {
    json_response(
        200,
        &json!({
            "name": "antigravity-bridge",
            "version": bridge.version,
            "uptimeSeconds": bridge.uptime_seconds(),
            // JS 版这里是 node 的版本号；Rust 版如实报自己的运行时（客户端只看 version）
            "runtime": "rust",
            // 编译身份由 core/build.rs 在编译期塞进来（env! 内联读，不开新模块、不加依赖）：
            // version 一直是 0.1.0，只有 git sha + 编译时间能区分这几天编的七八个版本
            "build": {
                "git": env!("BRIDGE_GIT_SHA"),
                "builtAt": env!("BRIDGE_BUILT_AT"),
                "target": env!("BRIDGE_TARGET"),
            },
        }),
        &[],
    )
}

async fn handle_healthz(State(bridge): State<Arc<Bridge>>) -> Response {
    // `memory_stats` shells out (`ps` on Linux) on non-macOS; keep it off the worker.
    let memory = tokio::task::spawn_blocking(memory_stats)
        .await
        .unwrap_or_else(|_| json!({}));
    // 面板 15 秒一跳读的就是这里；顺手把过期的额度排进后台刷新队列，下一次跳就能看到新数。
    // 没有过期账号时这个方法直接返回，等于零成本（有过期账号也只按 TTL 节流着打上游）。
    bridge.pool.refresh_stale_in_background();
    let accounts = crate::upstream::list_accounts().await.unwrap_or_default();
    let upstream = bridge.pool.identity().await;
    let signatures = sig_stats(&bridge.store);
    // Qoder 状态：只报形态（能不能用、什么时候过期、uid 前 8 位），绝不回显 token / 签名
    let qoder_json = match bridge.qoder() {
        Some(pool) => {
            let identity = pool.identity().await;
            let uid = identity
                .get("uid")
                .and_then(Value::as_str)
                .map(id_prefix)
                .unwrap_or_default();
            json!({
                "enabled": true,
                "jobTokenValid": identity.get("jobTokenValid").cloned().unwrap_or(json!(false)),
                "expiresAt": identity.get("expiresAt").cloned().unwrap_or(Value::Null),
                "uid": uid,
            })
        }
        None => json!({ "enabled": false }),
    };
    let accounts_json: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let email_str = a.email.as_deref().unwrap_or(&a.email_masked);
            json!({
                "email": email_str,
                "id": id_prefix(&a.id),
                "project": a.project,
                "current": a.is_current,
                "disabled": a.disabled,
                "validationBlocked": a.validation_blocked,
            })
        })
        .collect();
    json_response(
        200,
        &json!({
            "ok": true,
            "uptimeSeconds": bridge.uptime_seconds(),
            "memory": memory,
            "upstream": upstream,
            "pool": bridge.pool.state(),
            "accounts": accounts_json,
            // 账号库在哪、有没有从老工具迁过来（排查时一眼看到，省得猜）
            "accountsRoot": crate::oauth::accounts_root().map(|p| p.display().to_string()),
            "accountsMigratedFrom": crate::oauth::migration_note(),
            "signatures": signatures,
            "counters": bridge.counters.snapshot(),
            "qoder": qoder_json,
            // 面板的「接入」卡要告诉客户端带不带口令
            "apiKeyRequired": bridge.api_key.is_some(),
            // 正文截断上限（字符数，0 表示全部保留无截断）
            "bodyLimit": bridge.body_limit,
        }),
        &[],
    )
}

/// 账号 id 的前 8 位（和 JS 的 `idPrefix` 一致；完整 id 不对外）。
fn id_prefix(id: &str) -> String {
    id.chars().take(8).collect()
}

/// 额度：默认只查当前账号；`?all=1` 把库里每个可用账号都查一遍（各自的 token 各自签）。
async fn handle_quota(State(bridge): State<Arc<Bridge>>, req: Request) -> Response {
    let query = req.uri().query().unwrap_or("").to_string();
    let params = parse_query(&query);
    let all = params.get("all").map(|v| v == "1").unwrap_or(false);
    let wanted = params.get("account").cloned();

    let list = match crate::upstream::list_accounts().await {
        Ok(list) => list,
        Err(err) => {
            return json_response(
                500,
                &json!({ "error": { "type": "bridge_error", "message": format!("读不到账号库：{}", truncate(&err, 200)) } }),
                &[],
            )
        }
    };

    let targets: Vec<Account> = list
        .into_iter()
        .filter(|a| {
            if a.disabled || a.validation_blocked {
                return false;
            }
            match &wanted {
                Some(wanted) => {
                    a.email_masked.contains(wanted.as_str())
                        || id_prefix(&a.id).starts_with(wanted.as_str())
                        || a.email.as_deref() == Some(wanted.as_str())
                }
                None => all || a.is_current,
            }
        })
        .collect();

    let mut out = Vec::new();
    for account in targets {
        let t0 = now_millis();
        let item = quota_for_account(&bridge, &account).await;
        let elapsed = now_millis() - t0;
        out.push(match item {
            Ok((project, quota)) => {
                let mut obj = Map::new();
                obj.insert("email".into(), json!(account.email_masked));
                obj.insert("id".into(), json!(id_prefix(&account.id)));
                obj.insert("project".into(), json!(project));
                obj.insert("current".into(), json!(account.is_current));
                obj.insert("endpoint".into(), json!(quota.endpoint));
                obj.insert("summary".into(), json!(quota.summary));
                obj.insert("groups".into(), json!(quota.groups));
                obj.insert("models".into(), json!(quota.models));
                obj.insert("modelsDefault".into(), json!(quota.model_default));
                obj.insert("modelsDeprecated".into(), json!(quota.models_deprecated));
                obj.insert("modelLimits".into(), json!(quota.model_limits));
                obj.insert("elapsedMs".into(), json!(elapsed));
                Value::Object(obj)
            }
            Err(err) => json!({
                "email": account.email_masked,
                "id": id_prefix(&account.id),
                "current": account.is_current,
                // 查不到就如实报错，不用缓存里的旧数字顶替
                "error": truncate(&err, 300),
                "elapsedMs": elapsed,
            }),
        });
    }
    let count = out.len();
    json_response(200, &json!({ "accounts": out, "count": count }), &[])
}

/// Qoder 的额度：`GET /quota/qoder`。
///
/// 刻意不碰 [`handle_quota`] 的账号循环（那条路和 Antigravity 账号强耦合）：这里拿 Qoder 池里
/// 那个 `Pool::Single` 的会话打 `GET {openapi}/api/v2/quota/usage`，把原始 JSON 原样透传
/// （`userQuota` / `orgResourcePackage` / `totalUsagePercentage` / `isQuotaExceeded` / `expiresAt`），
/// 另加一条归一化的 `summary` 给面板用。
async fn handle_quota_qoder(State(bridge): State<Arc<Bridge>>) -> Response {
    let Some(pool) = bridge.qoder() else {
        return json_response(200, &json!({ "provider": "qoder", "enabled": false }), &[]);
    };
    let session = match pool.session("default").await {
        Ok(session) => session,
        Err(err) => {
            return json_response(
                502,
                &json!({ "provider": "qoder", "enabled": true, "error": truncate(&err.to_string(), 300) }),
                &[],
            )
        }
    };
    match session.quota_raw().await {
        Ok(raw) => {
            let mut out = Map::new();
            out.insert("provider".into(), json!("qoder"));
            out.insert("enabled".into(), json!(true));
            out.insert(
                "endpoint".into(),
                json!(format!("{}/api/v2/quota/usage", qoder::DEFAULT_OPENAPI)),
            );
            out.insert("summary".into(), json!(qoder::session::summary_rows(&raw)));
            // 上游原始字段（userQuota / orgResourcePackage / totalUsagePercentage / …）原样平铺出去
            if let Value::Object(map) = raw {
                for (key, value) in map {
                    out.insert(key, value);
                }
            }
            json_response(200, &Value::Object(out), &[])
        }
        Err(err) => json_response(
            502,
            &json!({ "provider": "qoder", "enabled": true, "error": truncate(&err.to_string(), 300) }),
            &[],
        ),
    }
}

/// 查一个账号的额度。池子里有它的会话就复用（顺手写缓存给选号用），没有就现建一个。
async fn quota_for_account(
    bridge: &Bridge,
    account: &Account,
) -> Result<(Option<String>, QuotaSnapshot), String> {
    let pool_id = account
        .email
        .as_deref()
        .and_then(|email| bridge.pool.id_by_email(email))
        .or_else(|| {
            if account.is_current {
                bridge.pool.primary_id()
            } else {
                None
            }
        });

    let session: Arc<dyn Session> = match &pool_id {
        Some(id) => bridge.pool.session(id).await.map_err(|e| e.to_string())?,
        None => {
            let upstream = crate::upstream::Upstream::init(crate::upstream::UpstreamOptions {
                email: account.email.clone(),
                endpoints: None,
                log: Arc::clone(&bridge.log),
                client: None,
            })
            .await?;
            Arc::new(upstream)
        }
    };

    let project = session
        .identity()
        .get("project")
        .and_then(Value::as_str)
        .map(str::to_string);
    let quota = session
        .quota()
        .await
        .map_err(|e| truncate(&e.to_string(), 300))?;
    if let Some(id) = &pool_id {
        bridge.pool.note_quota(id, &quota);
    }
    Ok((project, quota))
}

/// 上游模型表里的硬上限（构建器要拿它收口预算）。
/// `qoder/` 前缀的 id 走 Qoder 的模型表；其它走 Antigravity。
pub(crate) async fn model_limits_for(
    bridge: &Bridge,
    model_id: &str,
) -> Option<crate::quota::ModelLimit> {
    if qoder::is_qoder_model(model_id) {
        let models = bridge.qoder()?.models().await?;
        return model_limits(&models, qoder::strip_prefix(model_id));
    }
    let models = bridge.pool.models().await?;
    model_limits(&models, model_id)
}

/// 模型对象的外形：Anthropic 的 `GET /v1/models` 和 OpenAI 的 `GET /v1/models/{id}` 长得一样。
/// 上游模型表给了 `maxOutputTokens` 就一并报出去（面板的「接入」卡拿它当客户端的最大输出），
/// 没给就不编 —— 客户端自己填默认值比读一个假的数安全。
fn model_object(id: &str, m: Option<&Value>, owned_by: &str) -> Value {
    let display = m
        .and_then(|m| m.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_string();
    let max_output = m
        .and_then(|m| m.get("maxOutputTokens"))
        .and_then(Value::as_f64)
        .filter(|v| *v > 0.0);
    let mut obj = json!({
        "id": id,
        "object": "model",
        "type": "model",
        "display_name": display,
        "created": 0,
        "created_at": "1970-01-01T00:00:00Z",
        "owned_by": owned_by,
    });
    if let (Some(max), Value::Object(map)) = (max_output, &mut obj) {
        map.insert("max_output_tokens".into(), json!(max));
    }
    obj
}

/// 上游模型表里的模型对象（按 id 排序）。Qoder 的表挂在 `qoder/` 前缀下。
fn model_entries(models: Option<&Value>, owned_by: &str, prefix: Option<&str>) -> Vec<Value> {
    models
        .and_then(|m| m.get("models"))
        .and_then(Value::as_object)
        .map(|map| {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            entries
                .into_iter()
                .map(|(id, m)| {
                    let id = match prefix {
                        Some(prefix) => format!("{prefix}{id}"),
                        None => id.clone(),
                    };
                    model_object(&id, Some(m), owned_by)
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn handle_models(State(bridge): State<Arc<Bridge>>) -> Response {
    let models = bridge.pool.models().await;
    let mut data = model_entries(models.as_ref(), "antigravity", None);
    // Qoder 的模型带 `qoder/` 前缀、`owned_by:"qoder"`，和 Antigravity 的模型合并成一张表
    if let Some(pool) = bridge.qoder() {
        let qoder_models = pool.models().await;
        let mut qoder_entries =
            model_entries(qoder_models.as_ref(), "qoder", Some(qoder::MODEL_PREFIX));
        data.append(&mut qoder_entries);
        data.sort_by(|a, b| {
            a.get("id")
                .and_then(Value::as_str)
                .cmp(&b.get("id").and_then(Value::as_str))
        });
    }
    let default_model = models
        .as_ref()
        .and_then(|m| m.get("defaultAgentModelId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    json_response(
        200,
        &json!({ "object": "list", "data": data, "has_more": false, "default_model": default_model }),
        &[],
    )
}

async fn handle_model(State(bridge): State<Arc<Bridge>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let wanted = percent_decode(path.trim_start_matches("/v1/models/"));
    // `qoder/` 前缀：只在 Qoder 的模型表里找，找不到就 404（别落到 Antigravity 的 resolve 上）
    if qoder::is_qoder_model(&wanted) {
        let key = qoder::strip_prefix(&wanted);
        let qoder_models = match bridge.qoder() {
            Some(pool) => pool.models().await,
            None => None,
        };
        if let Some(entry) = qoder_models
            .as_ref()
            .and_then(|m| m.get("models"))
            .and_then(|m| m.get(key))
            .cloned()
        {
            return json_response(200, &model_object(&wanted, Some(&entry), "qoder"), &[]);
        }
        return anthropic_error(
            404,
            "not_found_error",
            &format!("没有这个模型：{wanted}"),
            None,
        );
    }
    let models = bridge.pool.models().await;
    let direct = models
        .as_ref()
        .and_then(|m| m.get("models"))
        .and_then(|m| m.get(&wanted))
        .cloned();
    if let Some(entry) = direct {
        return json_response(
            200,
            &model_object(&wanted, Some(&entry), "antigravity"),
            &[],
        );
    }

    let resolved = bridge.pool.resolve(&wanted).await;
    // 只有「真匹配上」才算：default_fallback 意味着谁也没匹配，随便挑了个默认模型，
    // 那种情况下必须回 404 —— 不然客户端拿一个编错的名字来探测也会得到 200。
    let matched = resolved
        .as_ref()
        .map(|r| {
            r.substituted_from.is_some()
                && r.reason != "default_fallback"
                && r.reason != "no_model_list"
        })
        .unwrap_or(false);
    if matched {
        let resolved = resolved.unwrap_or_default();
        let target = models
            .as_ref()
            .and_then(|m| m.get("models"))
            .and_then(|m| m.get(&resolved.model));
        let mut obj = model_object(&resolved.model, target, "antigravity");
        if let Value::Object(map) = &mut obj {
            map.insert("resolved_from".into(), json!(wanted));
            map.insert("resolve_reason".into(), json!(resolved.reason));
        }
        return json_response(200, &obj, &[]);
    }
    anthropic_error(
        404,
        "not_found_error",
        &format!("没有这个模型：{wanted}"),
        None,
    )
}

/// 有客户端把本地端点当 llama.cpp 那种服务探测过（老桥日志里 /version /props /v1/props 各 28 次）。
/// `/props` 语义不明，宁可继续 404，也不编一个可能误导客户端的形状。
async fn handle_logs(State(bridge): State<Arc<Bridge>>, req: Request) -> Response {
    let Some(dir) = &bridge.log_dir else {
        return json_response(
            200,
            &json!({ "lines": [], "note": "启动时没给 --log-dir，没有落盘日志" }),
            &[],
        );
    };
    let query = req.uri().query().unwrap_or("").to_string();
    let n = parse_query(&query)
        .get("n")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(50)
        .min(500);

    match tokio::fs::read_to_string(dir.join("requests.jsonl")).await {
        Ok(text) => {
            let lines: Vec<Value> = text
                .trim()
                .split('\n')
                .rev()
                .take(n)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|line| {
                    let mut value = serde_json::from_str::<Value>(line)
                        .unwrap_or_else(|_| json!({ "raw": line }));
                    // 列表不带正文：一条最多 16K，25 条就够把面板噎住。要看正文
                    // 按 id 去 /logs/entry 取 —— 列表里只留一个「有正文」的记号。
                    if let Value::Object(map) = &mut value {
                        let had =
                            map.remove("request").is_some() | map.remove("response").is_some();
                        if had {
                            map.insert("bodiesOmitted".into(), json!(true));
                        }
                    }
                    value
                })
                .collect();
            json_response(200, &json!({ "lines": lines }), &[])
        }
        Err(err) => json_response(
            200,
            &json!({ "lines": [], "note": truncate(&err.to_string(), 120) }),
            &[],
        ),
    }
}

/// 单条日志的全文（含正文）。列表（`/logs/recent`）默认不带正文，正文只从这儿出 ——
/// 一条最多 16K，几十条一起塞给面板不合适。
///
/// 找不到就 404（老条目没 id、或者已经被滚动删掉），不编一条出来。
/// 只看当前的 `requests.jsonl`：`.1`/`.2` 那些滚出去的历史不在服务范围内。
async fn handle_log_entry(State(bridge): State<Arc<Bridge>>, req: Request) -> Response {
    let Some(dir) = &bridge.log_dir else {
        return json_response(
            200,
            &json!({ "line": null, "note": "启动时没给 --log-dir，没有落盘日志" }),
            &[],
        );
    };
    let query = req.uri().query().unwrap_or("").to_string();
    let wanted = parse_query(&query)
        .get("id")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let Some(id) = wanted else {
        return json_response(
            400,
            &json!({ "error": "缺 id：/logs/entry?id=<条目 id>" }),
            &[],
        );
    };

    match tokio::fs::read_to_string(dir.join("requests.jsonl")).await {
        Ok(text) => {
            let found = text
                .lines()
                .rev()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id.as_str()));
            match found {
                Some(entry) => json_response(200, &json!({ "line": entry }), &[]),
                None => json_response(
                    404,
                    &json!({ "error": format!("日志里没有 id={id} 这一条（可能已经滚掉了）") }),
                    &[],
                ),
            }
        }
        Err(err) => json_response(
            200,
            &json!({ "line": null, "note": truncate(&err.to_string(), 120) }),
            &[],
        ),
    }
}

async fn handle_refresh_quota(State(bridge): State<Arc<Bridge>>) -> Response {
    // 手点「现查额度」：force = true，无视 TTL 真打一遍上游，否则 TTL 内点了等于空转。
    bridge.pool.refresh_all_in_background(true);
    json_response(
        202,
        &json!({ "ok": true, "message": "已让账号池在后台重查额度，几秒后刷新本页" }),
        &[],
    )
}

/// `/control/login` 的状态机：一趟 OAuth 回环最多在跑一个。
/// waiting → done/error；idle 是还没跑过或状态被消费前的兜底。
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginStatus {
    /// idle | waiting | done | error
    state: String,
    detail: Option<String>,
}

static LOGIN_STATUS: std::sync::Mutex<Option<LoginStatus>> = std::sync::Mutex::new(None);

fn login_status_snapshot() -> LoginStatus {
    LOGIN_STATUS.lock().unwrap().clone().unwrap_or(LoginStatus {
        state: "idle".into(),
        detail: None,
    })
}

fn login_status_set(state: &str, detail: Option<String>) {
    *LOGIN_STATUS.lock().unwrap() = Some(LoginStatus {
        state: state.into(),
        detail,
    });
}

/// `POST /control/login`：开一趟 OAuth 回环加账号。
/// 浏览器里点完授权 → 回调换 token → 账号落库 → 池子重读，全程不用重启。
/// 202 立刻返回；进度拿 `GET /control/login` 轮。
async fn handle_login_start(State(bridge): State<Arc<Bridge>>) -> Response {
    if login_status_snapshot().state == "waiting" {
        return json_response(
            409,
            &json!({
                "ok": false,
                "state": "waiting",
                "message": "一趟登录正在进行中——浏览器里那个授权页还没走完",
            }),
            &[],
        );
    }
    login_status_set("waiting", Some("浏览器已打开，等你完成 Google 授权".into()));
    tokio::spawn(async move {
        let client = crate::upstream::http_client();
        match crate::login::run_full(&client).await {
            Ok(outcome) => {
                let added = bridge.pool().rescan_accounts();
                login_status_set(
                    "done",
                    Some(format!(
                        "账号已添加：{}（池子吃进 {} 个新号）",
                        crate::oauth::mask_email(&outcome.email),
                        added
                    )),
                );
            }
            Err(message) => login_status_set("error", Some(message)),
        }
    });
    json_response(202, &json!({ "ok": true, "state": "waiting" }), &[])
}

/// `GET /control/login`：登录进度轮询。
async fn handle_login_status() -> Response {
    let status = login_status_snapshot();
    json_response(
        200,
        &json!({ "ok": true, "state": status.state, "detail": status.detail }),
        &[],
    )
}

async fn handle_restart(State(bridge): State<Arc<Bridge>>) -> Response {
    if !bridge.can_restart() {
        return json_response(
            409,
            &json!({
                "ok": false,
                "error": {
                    "type": "bridge_error",
                    "message": "这个进程不在 launchd 下跑，退掉就没人拉起来了。用 scripts/bridgectl（JS 版常驻）或 scripts/appctl（桌面壳）重启，或手动重启。",
                },
            }),
            &[],
        );
    }
    // 先把响应发出去，再退出（launchd 的 KeepAlive 会把它拉回来）
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        std::process::exit(0);
    });
    json_response(
        200,
        &json!({ "ok": true, "message": "正在退出，launchd 会把它拉回来（几秒后刷新本页）" }),
        &[],
    )
}

/// `POST /control/show-panel`：把已有实例的面板窗口拿到前台。
///
/// 谁在打它：桌面壳的第二个实例 —— 用户又启动了一次（双击 exe / 开始菜单），而端口上已经有
/// 一座自己的桥；与其另开一个浏览器，不如喊一声让那座桥把窗口收出来（Windows 上没有 macOS 的
/// Reopen 事件，「又点了一次图标」只能靠这个接口跨进程打招呼）。
/// headless（CLI）没有窗口可开：如实 409，第二个实例自己退到浏览器。
async fn handle_show_panel(State(bridge): State<Arc<Bridge>>) -> Response {
    let Some(show) = bridge.panel_show.get() else {
        return json_response(
            409,
            &json!({
                "ok": false,
                "error": {
                    "type": "bridge_error",
                    "message": "这个进程没有面板窗口（headless CLI，或者旧的桌面壳）",
                },
            }),
            &[],
        );
    };
    show();
    json_response(
        200,
        &json!({ "ok": true, "message": "已把面板窗口拿到前台" }),
        &[],
    )
}

/// `GET /control/accounts`：面板/脚本看「谁被禁着」。只给控制用得上的字段
/// （8 位 id、打码邮箱、禁用状态）；额度那些在 /healthz 里。
async fn handle_accounts(State(bridge): State<Arc<Bridge>>, request: Request) -> Response {
    if let Err(response) = check_api_key(&bridge, request.headers()) {
        return response;
    }
    handle_accounts_inner(bridge).await
}

async fn handle_accounts_inner(bridge: Arc<Bridge>) -> Response {
    let accounts: Vec<Value> = bridge
        .pool
        .state()
        .iter()
        .map(|a| json!({ "id": a.id, "email": a.email, "disabled": a.disabled }))
        .collect();
    let file = bridge
        .disabled_file
        .as_ref()
        .map(|p| p.display().to_string());
    json_response(
        200,
        &json!({
            "ok": true,
            "accounts": accounts,
            "persisted": file.is_some(),
            "stateFile": file,
        }),
        &[],
    )
}

/// `POST /control/accounts`：`{"id":"bf00c418","disabled":true}` 禁用/启用一个账号。
/// id 直接抄 `/healthz` 里那个 8 位前缀就行（够唯一就够用；不唯一会报错，不替你猜）。
/// 只影响后面的选号，正在跑的那条请求不打断。
async fn handle_accounts_write(State(bridge): State<Arc<Bridge>>, req: Request) -> Response {
    let body = match read_json_body(&bridge, req).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return control_error(400, "缺 id：给账号 id 或它的前 8 位");
    };
    let Some(disabled) = body.get("disabled").and_then(Value::as_bool) else {
        return control_error(400, "缺 disabled：true 禁用，false 启用");
    };
    let full = match bridge.pool.set_disabled(id, disabled) {
        Ok(full) => full,
        Err(message) => return control_error(400, &message),
    };
    let saved = match bridge.save_disabled() {
        Ok(saved) => saved,
        Err(message) => {
            // 内存里已经改了（这次运行有效），只是没落盘 —— 别报「失败」，但也别说存好了
            (bridge.log)(format!("禁用名单存盘失败：{message}"));
            return json_response(
                200,
                &json!({
                    "ok": true,
                    "id": full,
                    "disabled": disabled,
                    "persisted": false,
                    "warning": format!("已生效，但没落盘：{message}"),
                }),
                &[],
            );
        }
    };
    let file = saved.as_ref().map(|p| p.display().to_string());
    json_response(
        200,
        &json!({
            "ok": true,
            "id": full,
            "disabled": disabled,
            "persisted": file.is_some(),
            "stateFile": file,
        }),
        &[],
    )
}

/// 控制端点的报错形状（和 /control/restart 的 409 一致：`{ok:false,error:{...}}`）。
fn control_error(status: u16, message: &str) -> Response {
    json_response(
        status,
        &json!({ "ok": false, "error": { "type": "bridge_error", "message": message } }),
        &[],
    )
}

async fn handle_not_found(req: Request) -> Response {
    let route = format!("{} {}", req.method(), req.uri().path());
    json_response(
        404,
        &json!({ "type": "error", "error": { "type": "not_found_error", "message": format!("没有这个路由：{route}") } }),
        &[],
    )
}

// ---------------------------------------------------------------- 两条协议的入口

/// 读请求体 + 访问口令。返回 `Err(Box<Response>)` 表示已经可以回给客户端了。
/// Box 是为了压小 Err：Linux 上 `Response` 正好越过 clippy `result_large_err` 的 128 字节线
/// （macOS 上没有），套一层指针最省事，也不改变调用点「原样返回」的语义。
/// Check `x-api-key` / `Authorization: Bearer` against the configured key.
/// `None` (no key configured) always passes — the bridge then stays open to
/// loopback only, which is the default personal-machine setup.
fn check_api_key(bridge: &Bridge, headers: &axum::http::HeaderMap) -> Result<(), Response> {
    let Some(expected) = &bridge.api_key else {
        return Ok(());
    };
    let presented = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(|v| {
                    v.strip_prefix("Bearer ")
                        .or_else(|| v.strip_prefix("bearer "))
                        .unwrap_or(v)
                        .to_string()
                })
        })
        .unwrap_or_default();
    if &presented != expected {
        return Err(json_response(
            401,
            &json!({ "type": "error", "error": { "type": "authentication_error", "message": "桥的访问口令不对" } }),
            &[],
        ));
    }
    Ok(())
}

/// Forward `require_api_key`'s headers into `check_api_key`. Exposed as a
/// middleware so routes whose handler never reads the body (quota, logs,
/// control, accounts-read) get keyed exactly like the chat endpoints.
async fn require_api_key(
    State(bridge): State<Arc<Bridge>>,
    request: Request,
    next: Next,
) -> Response {
    match check_api_key(&bridge, request.headers()) {
        Ok(()) => next.run(request).await,
        Err(response) => response,
    }
}

async fn read_json_body(bridge: &Bridge, req: Request) -> Result<Value, Box<Response>> {
    let (parts, body) = req.into_parts();
    if let Err(response) = check_api_key(bridge, &parts.headers) {
        return Err(Box::new(response));
    }
    let _ = &parts;
    match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => {
            if bytes.is_empty() {
                return Ok(json!({}));
            }
            serde_json::from_slice(&bytes).map_err(|err| {
                Box::new(anthropic_error(
                    400,
                    "invalid_request_error",
                    &format!("请求体不是合法 JSON：{}", truncate(&err.to_string(), 200)),
                    None,
                ))
            })
        }
        Err(err) => Err(Box::new(anthropic_error(
            413,
            "invalid_request_error",
            &format!("请求体不合法或过大：{}", truncate(&err.to_string(), 200)),
            None,
        ))),
    }
}

async fn handle_messages(State(bridge): State<Arc<Bridge>>, req: Request) -> Response {
    let body = match read_json_body(&bridge, req).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    bridge.counters.requests();
    (bridge.log)(format!(
        "→ [anthropic] {} 条消息 model={} stream={} 工具={}",
        body.get("messages")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
        body.get("model").and_then(Value::as_str).unwrap_or("-"),
        body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        body.get("tools")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
    ));
    crate::chat::handle_messages(bridge, body).await
}

async fn handle_chat(State(bridge): State<Arc<Bridge>>, req: Request) -> Response {
    let body = match read_json_body(&bridge, req).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    bridge.counters.requests();
    (bridge.log)(format!(
        "→ [openai] {} 条消息 model={} stream={} 工具={}",
        body.get("messages")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
        body.get("model").and_then(Value::as_str).unwrap_or("-"),
        body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        body.get("tools")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
    ));
    crate::chat::handle_chat_completions(bridge, body).await
}

// ---------------------------------------------------------------- 小工具

/// 只解析面板用得到的查询串（`all=1`、`account=x`、`n=25`）。
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(key), percent_decode(value));
    }
    out
}

/// 查询串/路径里的百分号解码（+ 不当空格：路径里不该出现它）。
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{family_of_model, is_account_failure, is_retryable};
    use crate::types::iso_from_millis;

    #[test]
    fn upstream_status_maps_to_anthropic_types() {
        assert_eq!(
            map_upstream_error(400, "", "").kind,
            "invalid_request_error"
        );
        assert_eq!(map_upstream_error(401, "", "").http, 401);
        assert_eq!(
            map_upstream_error(403, "PERMISSION_DENIED", "").message,
            "上游拒绝了这个账号（403 PERMISSION_DENIED）"
        );
        assert_eq!(map_upstream_error(404, "", "").kind, "not_found_error");
        assert_eq!(map_upstream_error(429, "", "").http, 429);
        assert_eq!(map_upstream_error(500, "", "").http, 502);
        assert_eq!(map_upstream_error(500, "", "").kind, "overloaded_error");
        // status 0 是网络层失败，不是 HTTP 码
        let net = map_upstream_error(0, "network", "");
        assert_eq!(net.http, 502);
        assert_eq!(net.message, "上游错误（网络）");
        // 上游文案照抄
        assert_eq!(
            map_upstream_error(429, "", "quota exhausted").message,
            "quota exhausted"
        );
    }

    #[test]
    fn openai_error_shape_matches_js() {
        let mapped = MappedError {
            http: 429,
            kind: "rate_limit_error".into(),
            message: "慢点".into(),
            retry_after_seconds: Some(12),
        };
        let value = map_openai_error(&mapped);
        assert_eq!(value["type"], "rate_limit_error");
        assert_eq!(value["code"], "rate_limit_exceeded");
        assert_eq!(value["message"], "慢点");
        let other = MappedError {
            http: 502,
            kind: "api_error".into(),
            message: "炸".into(),
            retry_after_seconds: None,
        };
        assert_eq!(map_openai_error(&other)["type"], "server_error");
        assert_eq!(map_openai_error(&other)["code"], Value::Null);
    }

    #[test]
    fn budget_note_flags_thoughts_eating_the_budget() {
        let hit = json!({ "finishReason": "MAX_TOKENS", "usage": { "thoughts_token_count": 900, "output_tokens": 1000 } });
        assert_eq!(
            budget_note(&hit).as_deref(),
            Some("thoughts_ate_max_tokens")
        );
        let plain = json!({ "finishReason": "MAX_TOKENS", "usage": { "thoughts_token_count": 10, "output_tokens": 1000 } });
        assert_eq!(budget_note(&plain).as_deref(), Some("hit_max_tokens"));
        let no_thoughts = json!({ "finishReason": "MAX_TOKENS" });
        assert_eq!(budget_note(&no_thoughts).as_deref(), Some("hit_max_tokens"));
        let stop = json!({ "finishReason": "STOP" });
        assert_eq!(budget_note(&stop), None);
        assert_eq!(budget_note(&Value::Null), None);
    }

    #[test]
    fn restart_only_under_our_own_launchd_job_or_explicit_opt_in() {
        assert!(restart_allowed(Some(LAUNCHD_LABEL), None));
        // 桌面壳那份 job 也得认（它的 Label 多一个 .app 后缀）
        assert!(restart_allowed(Some(LAUNCHD_LABEL_APP), None));
        // 别的 App 拉起来的进程也可能带 XPC_SERVICE_NAME，不能只看有没有
        assert!(!restart_allowed(Some("com.apple.Terminal"), None));
        // 长得像但不是：只认整串 Label，不做前缀匹配
        assert!(!restart_allowed(
            Some("local.antigravity-bridge.app.extra"),
            None
        ));
        assert!(!restart_allowed(Some("local.antigravity-bridge-old"), None));
        assert!(!restart_allowed(None, None));
        assert!(restart_allowed(None, Some("1")));
        assert!(!restart_allowed(None, Some("0")));
    }

    #[test]
    fn query_parsing_and_percent_decoding() {
        let params = parse_query("all=1&account=a%40b.com&n=25");
        assert_eq!(params.get("all").map(String::as_str), Some("1"));
        assert_eq!(params.get("account").map(String::as_str), Some("a@b.com"));
        assert!(parse_query("").is_empty());
        assert_eq!(percent_decode("%E4%B8%AD%E6%96%87"), "中文");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn memory_stats_reports_footprint_or_rss_or_nothing() {
        let stats = memory_stats();
        // 面板优先读 footprintMb（macOS），没有就退回 rssMb；都拿不到就是空对象
        for key in ["footprintMb", "rssMb"] {
            if let Some(value) = stats.get(key) {
                assert!(
                    value.as_f64().unwrap_or(0.0) > 0.0,
                    "{key} 要么别报，要么报个正数：{value}"
                );
            }
        }
        // macOS 上 libproc 一定在，这条挂了说明 FFI 结构体对不上
        #[cfg(target_os = "macos")]
        assert!(
            stats.get("footprintMb").is_some(),
            "macOS 上应该能拿到物理足迹：{stats}"
        );
        // 日志那行是给人看的，要么别出现，要么是「（内存 …MB）」
        let note = memory_note();
        assert!(
            note.is_empty() || note.ends_with("MB）"),
            "日志里的内存说明长得不对：{note}"
        );
    }

    #[test]
    fn family_of_model_is_reachable_for_the_retry_header() {
        // pump 里用 family_of_model 给「所有账号都被拒」的 Retry-After 判键
        assert_eq!(family_of_model("claude-sonnet-4-5"), "3p");
        assert_eq!(family_of_model("gemini-3.6-flash-high"), "gemini");
    }

    #[test]
    fn retry_and_account_failure_verdicts_match_the_pump() {
        assert!(is_retryable(429));
        assert!(!is_retryable(400));
        // 401/403/429 都算账号级（pump 靠它决定「全被拒」时回 429 + Retry-After）；
        // 400/404 换了号也白换，所以既不可重试也不当账号级
        assert!(is_account_failure(403));
        assert!(is_account_failure(429));
        assert!(!is_account_failure(400));
        assert!(!is_account_failure(404));
    }

    #[test]
    fn iso_timestamps_are_second_precision() {
        assert_eq!(iso_from_millis(0), "1970-01-01T00:00:00Z");
    }

    struct NullSession;

    #[async_trait::async_trait]
    impl Session for NullSession {
        fn identity(&self) -> Value {
            Value::Null
        }
        async fn load_code_assist(&self) -> Result<Value, UpstreamError> {
            Ok(Value::Null)
        }
        async fn models(&self) -> Result<Value, UpstreamError> {
            Ok(Value::Null)
        }
        async fn quota(&self) -> Result<QuotaSnapshot, UpstreamError> {
            Ok(QuotaSnapshot::default())
        }
        async fn resolve_model(&self, _requested: &str) -> crate::types::ResolvedModel {
            crate::types::ResolvedModel::default()
        }
        async fn stream_generate(
            &self,
            _req: crate::types::GenerateRequest,
            _tx: tokio::sync::mpsc::Sender<crate::types::StreamEvent>,
        ) -> Result<(), UpstreamError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn version_reports_compiled_in_build_identity() {
        // /version 不碰账号池，喂个空会话就够起 Bridge 了（池子里的方法这条路径用不到）
        let bridge = Arc::new(Bridge::new(BridgeOptions {
            pool: Pool::Single(Arc::new(SingleAccountPool::new(Arc::new(NullSession)))),
            qoder: None,
            store: crate::signatures::shared_signatures(),
            log_dir: None,
            state_dir: None,
            log_bodies: true,
            body_limit: 0,
            api_key: None,
            allow_restart: Some(false),
            log: Arc::new(|_msg: String| {}),
            version: "0.0.0-test".to_string(),
        }));

        let response = handle_version(State(Arc::clone(&bridge))).await;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("读不出 /version 响应体");
        let body: Value = serde_json::from_slice(&bytes).expect("响应体不是 JSON");

        // 老字段一个都不能动也不能改名：面板、差分脚本、客户端都在读
        assert_eq!(body["name"], "antigravity-bridge");
        assert_eq!(body["version"], "0.0.0-test");
        assert_eq!(body["runtime"], "rust");
        assert!(body["uptimeSeconds"].as_i64().is_some());

        // build 三兄弟都得是非空字符串：取不到 git 时 build.rs 退化成 "unknown"，照样非空
        for key in ["git", "builtAt", "target"] {
            let value = body["build"][key]
                .as_str()
                .unwrap_or_else(|| panic!("build.{key} 不是字符串：{}", body["build"]));
            assert!(!value.is_empty(), "build.{key} 不能是空串");
        }
    }

    #[tokio::test]
    async fn router_has_cors_headers_and_options_support() {
        use axum::http::Request;
        use tower::ServiceExt;

        let bridge = Arc::new(Bridge::new(BridgeOptions {
            pool: Pool::Single(Arc::new(SingleAccountPool::new(Arc::new(NullSession)))),
            qoder: None,
            store: crate::signatures::shared_signatures(),
            log_dir: None,
            state_dir: None,
            log_bodies: true,
            body_limit: 0,
            api_key: None,
            allow_restart: Some(false),
            log: Arc::new(|_msg: String| {}),
            version: "0.0.0-test".to_string(),
        }));

        let app = bridge.router();

        // 1. GET 请求带 Access-Control-Allow-Origin: *
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );

        // 2. OPTIONS preflight 返回 204
        let req = Request::builder()
            .method("OPTIONS")
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 204);
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );
    }
}
