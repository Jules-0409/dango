//! 账号池：谁可用、该用谁、坏了先歇多久。
//!
//! 逐字移植 `src/bridge/accounts.mjs`（连注释里那些「为什么这么写」的原因一起搬）。
//!
//! 事实依据：
//!   - 上游的额度只有两组（探针 07）：Gemini Models（gemini-weekly / gemini-5h）与
//!     Claude and GPT models（3p-weekly / 3p-5h）。所以「按额度选号」是**按家族**算分，
//!     不是按单个模型。
//!   - 免费层下 pro 系稳定 429、额度却显示 99.8%，所以「429 = 账号被限」是错的：
//!     429 只给**这个账号的这个模型**记一笔，403/401 才是账号级（两个家族一起记）。
//!   - 老桥的熔断退避是 [60, 300, 1800, 7200] 秒，这里沿用同一组数字，方便对照它的行为。
//!
//! 这个模块不做网络请求：会话（签 token、调上游）由外部注入的 [`SessionFactory`] 负责，
//! 所以选号/熔断/粘性这些逻辑可以拿假账号和假会话离线测。
//!
//! 两个 JS → Rust 的等价物：
//!   - `createSession(account) => Promise<session>` → [`SessionFactory`]（返回 boxed future）。
//!     池子内部按账号 id 缓存会话：同一个账号只建一次、建失败不缓存（下次能重试）、
//!     并发调用共享同一次建立（`OnceCell` 保证）。
//!   - `now: () => number` → [`Clock`]，默认 `now_millis`（测试用它伪造时间推进退避台阶）。

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::OnceCell;

use crate::types::{
    iso_from_millis, now_millis, Account, QuotaRow, QuotaSnapshot, Session, UpstreamError,
};

/// 熔断退避台阶（毫秒）：照抄老桥的 [60, 300, 1800, 7200] 秒。
pub const BREAKER_STEPS_MS: [i64; 4] = [60_000, 300_000, 1_800_000, 7_200_000];
/// 额度缓存多久算过时。它同时也是「自动刷新」的节流窗口：`/healthz` 被面板每 15 秒调到一次，
/// 但只在缓存过期时才真的重查。为什么是 90 秒：一次现查要打上游 `retrieveUserQuotaSummary`，
/// 单次可能数秒，跟着轮询频率打等于把上游当自家缓存用；原来 10 分钟又太旧，面板看到的一直是
/// 上一次预热的结果。选号排序同样用这个 TTL。
pub const DEFAULT_QUOTA_TTL_MS: i64 = 90 * 1000;

/// 面板上标「额度缓存已过期」的门槛，比 TTL 宽得多（5 分钟）。
///
/// 为什么要有两个门槛：TTL 90 秒是「该补查了」的线，补查本身是后台 fire-and-forget，面板
/// 下一跳（15 秒后）通常就能看到新数 —— 于是「刚过 90 秒、补查在路上」的这一小段会频繁撞上
/// 面板的轮询，把一句吓人的「已过期」闪出来（实测约五分之一的概率）。而真正值得报警的是
/// 「补查连败」：那才是数据旧了。所以呈现用这个宽门槛，内部判断（补查触发、选号给中性分）
/// 仍然用 90 秒的 TTL。
pub const QUOTA_STALE_WARN_MS: i64 = 5 * 60 * 1000;

/// 「账号 + 家族额度耗尽」标记的兜底保留时长。比一次请求的熔断退避长得多：额度用光是家族窗口
/// 级的限制，不是几十秒就好的事。标记在拿到新的额度快照、看见该家族又有余额时会立刻被擦掉，
/// 所以这个值只在「拿不到新快照」时起作用 —— 宁长勿短，选号会正常降级（全耗尽时照样挑一个试）。
pub const DEFAULT_QUOTA_EXHAUSTED_MS: i64 = 5 * 60 * 1000;

const NEUTRAL_SCORE: f64 = 0.5; // 还没查到额度时给中庸分，既不优先也不惩罚
const STICKY_TOLERANCE: f64 = 0.6; // 粘性账号的额度分低于最高分的这个比例时，让位给额度高的

/// JS 的 `createSession(account) => Promise<session>`：按账号建一个会话。
///
/// 用 boxed future 是为了让工厂本身能放进 `Arc<dyn Fn …>`（返回 `impl Future` 的 trait
/// 对象在稳定 Rust 里写不出来）。失败返回 `UpstreamError`，池子据此决定是否缓存。
pub type SessionFactory = Arc<
    dyn Fn(Account) -> Pin<Box<dyn Future<Output = Result<Arc<dyn Session>, UpstreamError>> + Send>>
        + Send
        + Sync,
>;

/// 注入的时钟（epoch 毫秒）。测试用它伪造时间推进退避台阶。
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// 注入的日志函数（池子里只用来记熔断账与额度预取失败）。
pub type LogFn = Arc<dyn Fn(String) + Send + Sync>;

/// 一个账号的会话 cell：`OnceCell` 保证同一账号只建一次、失败可重试。
type SessionCell = Arc<OnceCell<Arc<dyn Session>>>;

/// 模型属于哪个额度家族：上游只有 Gemini 与 Claude+GPT 两组。
pub fn family_of_model(id: &str) -> &'static str {
    let lower = id.to_ascii_lowercase();
    if lower.contains("claude") || lower.contains("gpt") || lower.contains("oss") {
        "3p"
    } else {
        "gemini"
    }
}

/// 某家族还剩多少（取该家族所有窗口里最小的那个比例）；这个家族一行都没有时返回 `None`。
///
/// 为什么把未知额度（`remaining_percent == None`）当成 `0.0` 参与打分：因为 JS 版就是这么干的。
/// JS 的 `Number(null) === 0`、`Number.isFinite(0) === true`，所以 `remainingPercent` 缺失的行
/// 会以 0 分参与取 min，把该账号在该家族上的分数拉到 0（排序时靠后、算低额度）。差分验证要求
/// 两边一致，所以照搬，不做「Rust 版更聪明」的改动。
// 注：这大概是个 JS 版的隐患（未知额度被当成耗尽），要改就得两边一起改，别只在 Rust 版改。
pub fn score_for_family(rows: &[QuotaRow], family: &str) -> Option<f64> {
    let values: Vec<f64> = rows
        .iter()
        .filter(|r| family_matches(&r.group, family))
        .map(|r| r.remaining_percent.unwrap_or(0.0))
        // JS 的 `Number.isFinite` 也挡掉 NaN；Rust 里对应非有限值。
        .filter(|v| v.is_finite())
        .map(|v| v / 100.0)
        .collect();
    values.iter().copied().reduce(f64::min)
}

fn family_matches(group: &str, family: &str) -> bool {
    let g = group.to_ascii_lowercase();
    if family == "3p" {
        g.contains("claude") || g.contains("gpt")
    } else {
        g.contains("gemini")
    }
}

/// 换号重试有没有意义：账号级失败、网络失败、5xx 值得换；400/404 换了也白换。
pub fn is_retryable(status: u16) -> bool {
    status == 0 || status == 401 || status == 403 || status == 429 || status >= 500
}

/// 要不要给这个账号/家族记一笔熔断：401/403 是账号级，429 只记账到具体模型。
pub fn is_account_failure(status: u16) -> bool {
    status == 401 || status == 403 || status == 429
}

/// 这次 429 是不是「账号的额度用光了」——只有它才配按家族换号。
///
/// 判据是上游的原话：`QUOTA_EXHAUSTED`（实测口径，见 `docs/PROTOCOL.md` 的错误形态）
/// 或 message 里点名「超出配额」。**不把 `RESOURCE_EXHAUSTED` 算进来**：那是 429 通用的
/// gRPC status，纯限流也复用它；模块头记着一个事实 —— 免费层下 pro 系稳定 429 而额度还有
/// 99.8%，把这种当额度耗尽，就会把同一家族里还好的 flash 也一起封掉。
///
/// 拿不准（reason/message 都没有配额字样）一律当纯限流：少封一个号，比误封一个还有额度的号
/// 代价小 —— 纯限流仍会走原有的模型级熔断，换号照旧发生。
pub fn is_quota_exhaustion(info: &FailureInfo) -> bool {
    if info.status != 429 {
        return false;
    }
    let reason = info.reason.to_ascii_lowercase();
    if reason.contains("quota") {
        return true;
    }
    // reason 已经明确说是限流：以它为准，message 里恰好出现 quota 字样也不改判
    // （老代码里就有 reason=RATE_LIMIT、message=quota exceeded 这种混搭）。
    if reason.contains("rate") {
        return false;
    }
    // reason 没给、或只给了通用的 RESOURCE_EXHAUSTED，才轮到 message。要挑明确说「超了」的
    // 措辞，绕开「Resource has been exhausted (e.g. check quota)」这种限流文案里的 quota 字样。
    let message = info.message.to_ascii_lowercase();
    !message.contains("rate limit")
        && (message.contains("quota exceeded")
            || message.contains("exceeded your current quota")
            || message.contains("quota exhausted")
            || message.contains("insufficient quota"))
}

/// 熔断记账的粒度：
///   - 429（纯限流）：记到**具体模型**。免费层下 pro 系稳定 429 而 flash 正常，
///     按家族记会把整个家族一起拖下水（实测过：一次 pro 429 会让后续 flash 也换号）。
///   - 401/403（账号级）：两个家族一起记。
///
/// 注意：明确是「额度耗尽」的 429 不走这里 —— `note_failure` 会先把它判给
/// [`is_quota_exhaustion`]，记成账号 + 家族级的耗尽标记（额度本来就是家族级的）。
pub fn breaker_keys(status: u16, model: Option<&str>) -> Vec<String> {
    if status == 429 {
        if let Some(m) = model {
            if !m.is_empty() {
                return vec![format!("model:{m}")];
            }
        }
    }
    vec!["family:gemini".to_string(), "family:3p".to_string()]
}

/// 选号时给「这次请求」的查询条件（对齐 JS 的 `{ model, sessionKey }`）。
#[derive(Debug, Clone, Default)]
pub struct CandidateQuery {
    pub model: Option<String>,
    /// 真会话的 key；`None` 表示不是真会话，不参与粘性。
    pub session_key: Option<String>,
}

impl CandidateQuery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn for_model(model: impl Into<String>) -> Self {
        Self {
            model: Some(model.into()),
            session_key: None,
        }
    }

    pub fn with_session_key(mut self, key: impl Into<String>) -> Self {
        self.session_key = Some(key.into());
        self
    }
}

/// 一次失败的描述（对齐 JS 的 `{ status, reason, message, model }`）。
#[derive(Debug, Clone, Default)]
pub struct FailureInfo {
    pub status: u16,
    pub reason: String,
    pub message: String,
    pub model: Option<String>,
}

impl FailureInfo {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            ..Default::default()
        }
    }

    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into();
        self
    }

    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// `candidates()` 的一项，字段对齐 JS 的 `{ id, account, family, why, score, cooling }`。
///
/// `why` 取值：`sticky` / `breaker_forced` / `low_quota` / `unknown_quota` / `quota`
/// （单账号池额外有 `single`）。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub account: Account,
    pub family: &'static str,
    pub why: &'static str,
    pub score: Option<f64>,
    pub cooling: bool,
}

/// 某个熔断键的原始状态（`family:gemini` / `model:<id>`）。
#[derive(Debug, Clone, Default)]
pub struct BreakerState {
    pub failures: u32,
    pub until: i64,
    pub trips: u32,
    pub cooling: bool,
}

/// `cooldown_for` 的结果。
#[derive(Debug, Clone)]
pub struct Cooldown {
    pub cooling: bool,
    pub until: i64,
    pub reasons: Vec<String>,
}

/// `note_failure` 的结果：这一发有没有把某个熔断键记上账。
#[derive(Debug, Clone)]
pub struct FailureNote {
    pub tripped: bool,
    pub breakers: Vec<TrippedBreaker>,
}

/// 被记账的一个熔断键。
#[derive(Debug, Clone)]
pub struct TrippedBreaker {
    pub key: String,
    pub failures: u32,
    pub until: i64,
    pub step_ms: i64,
}

/// 额度的最近一次结果（可能过时，选号只用它排序；对外呈现永远走现查）。
#[derive(Debug, Clone)]
pub struct CachedQuota {
    pub at: i64,
    pub stale: bool,
    pub rows: Vec<QuotaRow>,
}

/// 给 `/healthz` 与面板用的、可序列化的池状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolState {
    pub email: String,
    /// 原 id 的前 8 位
    pub id: String,
    pub project: Option<String>,
    /// 被本地（面板/`/control/accounts`）禁用的账号：还在名单里，但不再参与选号。
    /// 和账号库自带的 `disabled` 不是一回事 —— 那种账号压根不会出现在这里。
    pub disabled: bool,
    pub session_ready: bool,
    pub breakers: Vec<PoolBreaker>,
    pub last_error: Option<PoolLastError>,
    pub last_used_at: Option<String>,
    pub quota: Option<PoolQuota>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolBreaker {
    pub key: String,
    pub failures: u32,
    pub trips: u32,
    pub cooling: bool,
    pub until: Option<String>,
    pub retry_in_seconds: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolLastError {
    pub at: String,
    pub status: u16,
    pub reason: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolQuota {
    pub at: String,
    pub stale: bool,
    pub rows: Vec<QuotaRow>,
}

/// `AccountPool` 的构造参数（对齐 JS 的 options 对象）。
pub struct AccountPoolOptions {
    pub factory: SessionFactory,
    pub accounts: Vec<Account>,
    pub log: LogFn,
    pub now: Clock,
    pub breaker_steps: Vec<i64>,
    pub quota_ttl_ms: i64,
    pub min_fraction: f64,
    pub sticky_max: usize,
    /// 「账号 + 家族额度耗尽」标记的兜底时长（见 [`DEFAULT_QUOTA_EXHAUSTED_MS`]）。
    pub quota_exhausted_ms: i64,
}

impl AccountPoolOptions {
    pub fn new(factory: SessionFactory, accounts: Vec<Account>) -> Self {
        Self {
            factory,
            accounts,
            log: Arc::new(|_msg: String| {}),
            now: Arc::new(now_millis),
            breaker_steps: BREAKER_STEPS_MS.to_vec(),
            quota_ttl_ms: DEFAULT_QUOTA_TTL_MS,
            min_fraction: 0.02,
            sticky_max: 2000,
            quota_exhausted_ms: DEFAULT_QUOTA_EXHAUSTED_MS,
        }
    }

    pub fn log(mut self, log: LogFn) -> Self {
        self.log = log;
        self
    }

    pub fn now(mut self, now: Clock) -> Self {
        self.now = now;
        self
    }

    pub fn breaker_steps(mut self, steps: Vec<i64>) -> Self {
        self.breaker_steps = steps;
        self
    }

    pub fn quota_ttl_ms(mut self, ttl: i64) -> Self {
        self.quota_ttl_ms = ttl;
        self
    }

    pub fn quota_exhausted_ms(mut self, ms: i64) -> Self {
        self.quota_exhausted_ms = ms;
        self
    }

    pub fn min_fraction(mut self, min_fraction: f64) -> Self {
        self.min_fraction = min_fraction;
        self
    }

    pub fn sticky_max(mut self, sticky_max: usize) -> Self {
        self.sticky_max = sticky_max;
        self
    }
}

#[derive(Debug, Clone, Default)]
struct Breaker {
    failures: u32,
    until: i64,
    trips: u32,
}

#[derive(Debug, Clone)]
struct LastError {
    at: i64,
    status: u16,
    reason: String,
    message: String,
}

#[derive(Debug, Clone, Default)]
struct AccountState {
    /// 用 `Vec` 而不是 `HashMap`：JS 的 `Map` 保留插入顺序，`/healthz` 里 breakers 的顺序
    /// 就是记账顺序（账号级失败先记 gemini 再记 3p）。
    breakers: Vec<(String, Breaker)>,
    /// 额度耗尽的家族 → 什么时候可以再把它排回前面（epoch 毫秒）。
    ///
    /// 刻意和 `breakers` 分开：熔断是「上游拒了这次请求」，一次成功就该全清；额度耗尽不会
    /// 因为别的家族跑成功就回来（所以 `note_success` 不许碰它）。只有新额度快照看见该家族
    /// 又有余额了，或者这条兜底标记自然到期，才该失效。
    exhausted: Vec<(String, i64)>,
    last_error: Option<LastError>,
    last_used_at: i64,
}

#[derive(Debug, Clone)]
struct QuotaCache {
    at: i64,
    rows: Vec<QuotaRow>,
}

/// `candidates()` 排序中间态（对齐 JS 里那个 `rows` 数组）。
#[derive(Debug, Clone)]
struct Row {
    id: String,
    account: Account,
    family: &'static str,
    cooling: bool,
    until: i64,
    failures: u32,
    score: Option<f64>,
    /// 这个账号的这个家族是不是「没额度了」：快照分 ≤ 0，或有「账号 + 家族耗尽」标记。
    /// 它只影响排序（排到最后），不把账号踢出候选 —— 全耗尽时仍要挑一个去试。
    exhausted: bool,
    low_quota: bool,
    sticky: bool,
    last_used_at: i64,
}

/// 账号池：谁可用、该用谁、坏了先歇多久。
pub struct AccountPool {
    factory: SessionFactory,
    log: LogFn,
    now: Clock,
    breaker_steps: Vec<i64>,
    quota_ttl_ms: i64,
    quota_exhausted_ms: i64,
    min_fraction: f64,
    sticky_max: usize,
    /// 账号名单：启动快照 + `/control/login` 加进来的新号（RwLock 因为要中途扩员）。
    accounts: std::sync::RwLock<Vec<Account>>,
    /// 本地禁用名单（完整 id）：面板上「先别用这个号」的那一份。和账号库自带的
    /// `disabled` 不是一回事 —— 那份是「这个账号本身就不能用了」（待验证/没 refresh_token），
    /// 归一化时已经折进 `Account.disabled`。这份只有内存 + 一个落盘文件，池子不认识磁盘。
    disabled: Mutex<BTreeSet<String>>,
    /// id → 会话 cell。cell 在位表示「建过或正在建」；建失败会把它摘掉（下次能重试）。
    sessions: Mutex<HashMap<String, SessionCell>>,
    states: Mutex<HashMap<String, AccountState>>,
    quotas: Mutex<HashMap<String, QuotaCache>>,
    /// 上次触发「过期额度后台刷新」的时刻（epoch 毫秒，0 = 从未）。面板 15 秒一跳，
    /// 而一次上游查询要数秒：用它把自动刷新压到每个 TTL 窗口一次，别让轮询变成持续施压。
    last_stale_refresh_at: AtomicI64,
    /// sessionKey → id，保序（JS 的 `Map` 语义：改已存在的 key 不改变它的位置）。
    sticky: Mutex<Vec<(String, String)>>,
}

impl AccountPool {
    pub fn new(options: AccountPoolOptions) -> Self {
        Self {
            factory: options.factory,
            log: options.log,
            now: options.now,
            breaker_steps: options.breaker_steps,
            quota_ttl_ms: options.quota_ttl_ms,
            quota_exhausted_ms: options.quota_exhausted_ms,
            min_fraction: options.min_fraction,
            sticky_max: options.sticky_max,
            accounts: std::sync::RwLock::new(
                options
                    .accounts
                    .into_iter()
                    .map(normalize_account)
                    .collect(),
            ),
            disabled: Mutex::new(BTreeSet::new()),
            sessions: Mutex::new(HashMap::new()),
            states: Mutex::new(HashMap::new()),
            quotas: Mutex::new(HashMap::new()),
            last_stale_refresh_at: AtomicI64::new(0),
            sticky: Mutex::new(Vec::new()),
        }
    }

    /// 能参与选号的账号：账号库没标废，而且没被本地禁掉。
    /// 返回 owned 副本 —— 名单在 RwLock 里，引用走不出守卫。
    fn usable(&self) -> Vec<Account> {
        let disabled = self.disabled.lock().unwrap();
        self.accounts
            .read()
            .unwrap()
            .iter()
            .filter(|a| !a.disabled && !disabled.contains(&a.id))
            .cloned()
            .collect()
    }

    /// `/healthz` 该列出来的账号：**包括**被本地禁用的那些（不然面板上没法再启用回来），
    /// 只排除账号库自己标了 disabled 的 —— 那种连会话都建不起来，列出来只会误导。
    fn visible(&self) -> Vec<Account> {
        self.accounts
            .read()
            .unwrap()
            .iter()
            .filter(|a| !a.disabled)
            .cloned()
            .collect()
    }

    /// 字段名与方法名同名是允许的（不同命名空间）；`self.now` 是注入的时钟，`self.now()` 读它。
    fn now(&self) -> i64 {
        (self.now)()
    }

    pub fn account(&self, id: &str) -> Option<Account> {
        self.accounts
            .read()
            .unwrap()
            .iter()
            .find(|a| a.id == id)
            .cloned()
    }

    /// 重读账号库，把新出现的账号加进池子（`/control/login` 之后调用）。
    /// 只增不删：运行中的会话和熔断状态都是按 id 挂的，撤号走禁用名单那条路。
    /// 返回新加进来的账号数。
    pub fn rescan_accounts(&self) -> usize {
        let Ok((raw_accounts, _)) = crate::oauth::load_bridge_accounts() else {
            return 0;
        };
        let mut guard = self.accounts.write().unwrap();
        let mut added = 0;
        for raw in raw_accounts {
            let account = normalize_account(raw);
            if !guard.iter().any(|a| a.id == account.id) {
                guard.push(account);
                added += 1;
            }
        }
        added
    }

    /// 本地禁用名单（完整 id）：落盘和 `/control/accounts` 用。
    pub fn disabled_ids(&self) -> BTreeSet<String> {
        self.disabled.lock().unwrap().clone()
    }

    /// 面板只拿得到 8 位 id 前缀（`/healthz` 里就是这么给的），按前缀找回完整 id。
    /// 匹配到多个时如实报错，不替人猜一个。
    fn resolve_id(&self, prefix: &str) -> Result<String, String> {
        let hits: Vec<String> = self
            .accounts
            .read()
            .unwrap()
            .iter()
            .filter(|a| a.id.starts_with(prefix))
            .map(|a| a.id.clone())
            .collect();
        match hits.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(format!("没有这个账号：{prefix}")),
            _ => Err(format!(
                "这个前缀不唯一（{} 个账号都匹配），给长一点：{prefix}",
                hits.len()
            )),
        }
    }

    /// 禁用/启用一个账号（只改内存，落盘归服务层）。返回完整 id。
    pub fn set_disabled(&self, prefix: &str, disabled: bool) -> Result<String, String> {
        let id = self.resolve_id(prefix)?;
        let mut set = self.disabled.lock().unwrap();
        if disabled {
            set.insert(id.clone());
        } else {
            set.remove(&id);
        }
        Ok(id)
    }

    /// 启动时把落盘的名单灌进来。认不出的 id（账号被删了、换了）直接忽略：
    /// 一份过期的名单不该拦住启动，也不该把不认识的号塞进名单。
    pub fn set_disabled_ids(&self, ids: impl IntoIterator<Item = String>) {
        let mut set = self.disabled.lock().unwrap();
        set.clear();
        let accounts = self.accounts.read().unwrap();
        for id in ids {
            if accounts.iter().any(|a| a.id == id) {
                set.insert(id);
            }
        }
    }

    fn state_of(&self, id: &str) -> AccountState {
        let mut states = self.states.lock().unwrap();
        states.entry(id.to_string()).or_default().clone()
    }

    fn with_state<R>(&self, id: &str, f: impl FnOnce(&mut AccountState) -> R) -> R {
        let mut states = self.states.lock().unwrap();
        let st = states.entry(id.to_string()).or_default();
        f(st)
    }

    fn breakers_snapshot(&self, id: &str) -> Vec<(String, Breaker)> {
        self.states
            .lock()
            .unwrap()
            .get(id)
            .map(|s| s.breakers.clone())
            .unwrap_or_default()
    }

    /// 某个熔断键的原始状态（`family:gemini` / `model:<id>`）。
    pub fn breaker_of(&self, id: &str, key: &str) -> BreakerState {
        let breakers = self.breakers_snapshot(id);
        let b = breaker_get(&breakers, key).cloned().unwrap_or_default();
        let cooling = b.until > self.now();
        BreakerState {
            failures: b.failures,
            until: b.until,
            trips: b.trips,
            cooling,
        }
    }

    /// 这个账号能不能接这个模型的活：账号级熔断或该模型的熔断任一命中就算冷却。
    ///
    /// JS 还允许调用方显式覆盖 `family`；Rust 侧没有任何调用方用到，故只按 `model` 推导。
    pub fn cooldown_for(&self, id: &str, model: Option<&str>) -> Cooldown {
        let family = family_of_model(model.unwrap_or(""));
        let now = self.now();
        let breakers = self.breakers_snapshot(id);
        let mut keys = vec![format!("family:{family}")];
        if let Some(m) = model {
            keys.push(format!("model:{m}"));
        }
        let mut hits: Vec<(String, i64)> = Vec::new();
        for key in keys {
            if let Some(b) = breaker_get(&breakers, &key) {
                if b.until > now {
                    hits.push((key, b.until));
                }
            }
        }
        if hits.is_empty() {
            return Cooldown {
                cooling: false,
                until: 0,
                reasons: Vec::new(),
            };
        }
        let until = hits.iter().map(|(_, u)| *u).max().unwrap_or(0);
        Cooldown {
            cooling: true,
            until,
            reasons: hits.into_iter().map(|(k, _)| k).collect(),
        }
    }

    /// 这个账号的这个家族额度是不是还没恢复（标记未过期）。选号据此把账号排到最后。
    pub fn family_exhausted(&self, id: &str, family: &str) -> bool {
        self.family_exhausted_until(id, family) > self.now()
    }

    fn family_exhausted_until(&self, id: &str, family: &str) -> i64 {
        let st = self.state_of(id);
        st.exhausted
            .iter()
            .find(|(f, _)| f == family)
            .map(|(_, until)| *until)
            .unwrap_or(0)
    }

    fn has_session(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(id)
    }

    fn drop_cell(&self, id: &str, cell: &SessionCell) {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(existing) = sessions.get(id) {
            if Arc::ptr_eq(existing, cell) {
                sessions.remove(id);
            }
        }
    }

    /// 懒建会话：同一个账号只建一次；建失败不缓存（下次还能再试）。
    pub async fn session(&self, id: &str) -> Result<Arc<dyn Session>, UpstreamError> {
        let account = match self.account(id) {
            Some(a) => a,
            None => {
                return Err(UpstreamError::network(
                    "no_account",
                    format!("池里没有账号 {id}"),
                ));
            }
        };
        // 先拿 cell（同一账号的并发调用会拿到同一个 cell），立刻放锁，别把锁带过 await。
        let cell = {
            let mut sessions = self.sessions.lock().unwrap();
            sessions
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        let factory = Arc::clone(&self.factory);
        let init = cell.get_or_try_init(|| {
            let factory = Arc::clone(&factory);
            let account = account.clone();
            async move { factory(account).await }
        });
        match init.await {
            Ok(session) => Ok(Arc::clone(session)),
            Err(err) => {
                // 建失败不缓存：把 cell 摘掉，下次重新建。Arc::ptr_eq 防止误删并发新建的 cell。
                self.drop_cell(id, &cell);
                Err(err)
            }
        }
    }

    /// 开一个会话用于这次请求（顺带记 lastUsedAt）；失败会抛，由调用方换下一个号。
    pub async fn open(&self, id: &str) -> Result<Arc<dyn Session>, UpstreamError> {
        let session = self.session(id).await?;
        let now = self.now();
        self.with_state(id, |st| st.last_used_at = now);
        Ok(session)
    }

    /// 这次请求该按什么顺序试哪些账号。
    /// 顺序：粘性账号（如果它没在冷却）→ 额度分高的 → 额度低但还行的 → 冷却中的（先醒的先试）。
    /// 冷却中的也会返回：全部都在冷却时不能让用户干等，先试最早醒的那个。
    pub fn candidates(&self, query: CandidateQuery) -> Vec<Candidate> {
        let family = family_of_model(query.model.as_deref().unwrap_or(""));
        let sticky_id = query
            .session_key
            .as_deref()
            .and_then(|key| self.sticky_get(key));
        let mut rows: Vec<Row> = Vec::new();
        for a in self.usable() {
            let st = self.state_of(&a.id);
            let cooldown = self.cooldown_for(&a.id, query.model.as_deref());
            let score = self
                .quota_rows(&a.id)
                .and_then(|rows| score_for_family(&rows, family));
            let model_failures = match query.model.as_deref() {
                Some(m) => self.breaker_of(&a.id, &format!("model:{m}")).failures,
                None => 0,
            };
            let family_failures = self.breaker_of(&a.id, &format!("family:{family}")).failures;
            rows.push(Row {
                id: a.id.clone(),
                account: a.clone(),
                family,
                cooling: cooldown.cooling,
                until: cooldown.until,
                failures: model_failures.max(family_failures),
                score,
                // 分 ≤ 0（快照说这个家族一点不剩）或吃过「额度耗尽」的 429：两者都算没额度。
                // 额度未知（`score == None`）不算 —— 那是「还没查到」，不能被 0 分误伤。
                exhausted: score.is_some_and(|s| s <= 0.0) || self.family_exhausted(&a.id, family),
                low_quota: score.is_some_and(|s| s < self.min_fraction),
                sticky: Some(&a.id) == sticky_id.as_ref(),
                last_used_at: st.last_used_at,
            });
        }

        let (mut fresh, mut cooling): (Vec<Row>, Vec<Row>) =
            rows.into_iter().partition(|r| !r.cooling);
        // 没冷却的：有额度（含未知）的在前、耗尽的垫底 → 分数降序 → 失败次数升序 →
        // 最久没用过的优先 → id 字典序。
        // 为什么「耗尽」要单列一把钥匙：额度分只覆盖「快照说还剩多少」，而标记来自上游 429，
        // 二者都可能单独成立；把它们并到分数里排序，标记就会在分数还是有值时被忽略。
        fresh.sort_by(|x, y| {
            let ys = y.score.unwrap_or(NEUTRAL_SCORE);
            let xs = x.score.unwrap_or(NEUTRAL_SCORE);
            x.exhausted
                .cmp(&y.exhausted)
                .then_with(|| ys.partial_cmp(&xs).unwrap_or(Ordering::Equal))
                .then_with(|| x.failures.cmp(&y.failures))
                .then_with(|| x.last_used_at.cmp(&y.last_used_at))
                .then_with(|| x.id.cmp(&y.id))
        });
        // 冷却中的：先醒的先试。
        cooling.sort_by_key(|x| x.until);

        // 粘性：只在它「没冷却，而且额度没差太多」时插到队首。
        // 不这么干的话，所有没带会话标记的请求都会粘死在同一个账号上，多账号就白搭了。
        // 耗尽的账号不参与插队：粘性不该把「没额度」的号重新顶到有额度的候选前面。
        if query.session_key.is_some() {
            if let Some(idx) = fresh.iter().position(|r| r.sticky) {
                if idx > 0 && !fresh[idx].exhausted {
                    let best = fresh[0].score.unwrap_or(NEUTRAL_SCORE);
                    let sticky_score = fresh[idx].score.unwrap_or(NEUTRAL_SCORE);
                    if sticky_score >= best * STICKY_TOLERANCE {
                        let item = fresh.remove(idx);
                        fresh.insert(0, item);
                    }
                }
            }
        }

        fresh
            .into_iter()
            .chain(cooling)
            .enumerate()
            .map(|(i, r)| {
                let why = if i == 0 && r.sticky {
                    "sticky"
                } else if r.cooling {
                    "breaker_forced"
                } else if r.exhausted {
                    "exhausted"
                } else if r.low_quota {
                    "low_quota"
                } else if r.score.is_none() {
                    "unknown_quota"
                } else {
                    "quota"
                };
                Candidate {
                    id: r.id,
                    account: r.account,
                    family: r.family,
                    why,
                    score: r.score,
                    cooling: r.cooling,
                }
            })
            .collect()
    }

    /// 这一发成功了：清掉这个账号在**两个家族**上的失败计数。
    pub fn note_success(&self, id: &str) {
        self.with_state(id, |st| {
            if !st.breakers.is_empty() {
                st.breakers.clear();
            }
            st.last_error = None;
        });
    }

    /// 这一发失败在哪：
    ///   - 「额度耗尽」的 429（上游明确说 QUOTA）：记**账号 + 家族**耗尽，不记模型熔断。
    ///     额度是家族级的（见模块头），这个家族空了，换同家族的别的模型也白搭；继续只记模型
    ///     的话，只有撞过 429 的那个模型会被避开，同家族的下一个模型照样会派给它。
    ///   - 其它 401/403：账号级（两个家族一起记）。
    ///   - 纯限流的 429：仍旧只记到**具体模型**（免费层 pro 稳定 429 而 flash 正常，
    ///     按家族记会误伤；这也是「别把模型级限流放大成家族封禁」）。
    ///   - 400/404/5xx/网络：不算账号的账，只留一条 lastError 供排查。
    pub fn note_failure(&self, id: &str, info: FailureInfo) -> FailureNote {
        let now = self.now();
        let tripped = is_account_failure(info.status);
        let label = self
            .account(id)
            .map(|a| a.email_masked)
            .unwrap_or_else(|| id.to_string());
        let log = Arc::clone(&self.log);
        let steps = self.breaker_steps.clone();
        let quota_exhausted_ms = self.quota_exhausted_ms;
        let exhausted_family = is_quota_exhaustion(&info)
            .then(|| family_of_model(info.model.as_deref().unwrap_or("")));
        let message = truncate_chars(&info.message, 200);
        let keys = if tripped && exhausted_family.is_none() {
            breaker_keys(info.status, info.model.as_deref())
        } else {
            Vec::new()
        };
        let mut recorded: Vec<TrippedBreaker> = Vec::new();
        self.with_state(id, |st| {
            st.last_error = Some(LastError {
                at: now,
                status: info.status,
                reason: info.reason.clone(),
                message,
            });
            if let Some(family) = exhausted_family {
                let until = now + quota_exhausted_ms;
                exhausted_set(&mut st.exhausted, family, until);
                log(format!(
                    "账号 {label} 的 {family} 家族额度耗尽（{} {}）：{} 前不再优先选它",
                    info.status,
                    info.reason,
                    iso_from_millis(until)
                ));
            }
            for key in keys {
                let prev = breaker_get(&st.breakers, &key).cloned().unwrap_or_default();
                let failures = prev.failures + 1;
                let idx = ((failures - 1) as usize).min(steps.len().saturating_sub(1));
                let step = steps.get(idx).copied().unwrap_or(0);
                let until = now + step;
                let is_new_trip = prev.until <= now;
                breaker_set(
                    &mut st.breakers,
                    &key,
                    Breaker {
                        failures,
                        until,
                        trips: prev.trips + u32::from(is_new_trip),
                    },
                );
                log(format!(
                    "账号 {label} 在 {key} 上被记一笔（{} {}）：第 {failures} 次，退避 {}s",
                    info.status,
                    info.reason,
                    ((step as f64) / 1000.0).round() as i64
                ));
                recorded.push(TrippedBreaker {
                    key: key.clone(),
                    failures,
                    until,
                    step_ms: step,
                });
            }
        });
        if tripped {
            FailureNote {
                tripped: true,
                breakers: recorded,
            }
        } else {
            FailureNote {
                tripped: false,
                breakers: Vec::new(),
            }
        }
    }

    /// 会话粘性：这次是谁服务的，会话就记住它（有上限，老的先丢）。"default" 不是真会话，不记。
    pub fn remember(&self, session_key: &str, id: &str) {
        if session_key.is_empty() || session_key == "default" || id.is_empty() {
            return;
        }
        let mut sticky = self.sticky.lock().unwrap();
        // JS 的 Map.set：改已存在的 key 不改变它的插入位置。
        if let Some(entry) = sticky.iter_mut().find(|(k, _)| k == session_key) {
            entry.1 = id.to_string();
            return;
        }
        while self.sticky_max > 0 && sticky.len() >= self.sticky_max {
            sticky.remove(0);
        }
        sticky.push((session_key.to_string(), id.to_string()));
    }

    fn sticky_get(&self, key: &str) -> Option<String> {
        self.sticky
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    fn quota_cache(&self, id: &str) -> Option<QuotaCache> {
        self.quotas.lock().unwrap().get(id).cloned()
    }

    fn quota_rows(&self, id: &str) -> Option<Vec<QuotaRow>> {
        self.quota_cache(id).map(|c| c.rows)
    }

    /// 额度的最近一次结果（可能过时，选号只用它排序；对外呈现永远走现查）。
    pub fn quota_of(&self, id: &str) -> Option<CachedQuota> {
        self.quota_cache(id).map(|c| CachedQuota {
            at: c.at,
            stale: self.now() - c.at > self.quota_ttl_ms,
            rows: c.rows,
        })
    }

    /// 别的地方（比如 /quota 路由）刚查到一份现成的额度：顺手写进缓存，选号就能用上。
    pub fn note_quota(&self, id: &str, quota: &QuotaSnapshot) {
        if id.is_empty() {
            return;
        }
        self.store_quota(id, quota.summary.clone());
    }

    /// 落一份新的额度快照：写缓存，并顺便校准「额度耗尽」标记。
    ///
    /// 为什么要在写的时候校准：标记是上游 429 给的事实，但额度窗口会重置。新快照里某个家族
    /// 明确又有余额了，就该立刻把标记擦掉 —— 不然一个已经恢复的号会被一直排到最后，
    /// 直到兜底 TTL 到点。只信「明确大于 0」：字段缺失会被 `score_for_family` 按 0 处理
    /// （JS 口径），那是「不知道」而不是「恢复了」。
    fn store_quota(&self, id: &str, rows: Vec<QuotaRow>) {
        let now = self.now();
        let recovered: Vec<&str> = ["gemini", "3p"]
            .into_iter()
            .filter(|f| score_for_family(&rows, f).is_some_and(|s| s > 0.0))
            .collect();
        if !recovered.is_empty() {
            self.with_state(id, |st| {
                st.exhausted
                    .retain(|(f, _)| !recovered.contains(&f.as_str()));
            });
        }
        self.quotas
            .lock()
            .unwrap()
            .insert(id.to_string(), QuotaCache { at: now, rows });
    }

    pub fn id_by_email(&self, email: &str) -> Option<String> {
        self.accounts
            .read()
            .unwrap()
            .iter()
            .find(|a| a.email.as_deref() == Some(email))
            .map(|a| a.id.clone())
    }

    /// 这个模型上，最早的熔断什么时候醒（给「全都试过了」时的 Retry-After 用）。
    pub fn earliest_retry_in_seconds(&self, model: Option<&str>) -> Option<i64> {
        let family = family_of_model(model.unwrap_or(""));
        let now = self.now();
        let mut keys = vec![format!("family:{family}")];
        if let Some(m) = model {
            keys.push(format!("model:{m}"));
        }
        let mut waits: Vec<i64> = Vec::new();
        for a in self.usable() {
            let breakers = self.breakers_snapshot(&a.id);
            for key in &keys {
                if let Some(b) = breaker_get(&breakers, key) {
                    if b.until > now {
                        waits.push(b.until);
                    }
                }
            }
        }
        waits
            .iter()
            .min()
            .map(|min| (((min - now) as f64) / 1000.0).ceil() as i64)
    }

    /// 查一个账号的额度并缓存（`force = true` 无视 TTL）。
    pub async fn refresh_quota(
        &self,
        id: &str,
        force: bool,
    ) -> Result<Vec<QuotaRow>, UpstreamError> {
        if !force {
            if let Some(cached) = self.quota_cache(id) {
                if self.now() - cached.at < self.quota_ttl_ms {
                    return Ok(cached.rows);
                }
            }
        }
        let session = self.session(id).await?;
        let quota = session.quota().await?;
        let rows = quota.summary;
        self.store_quota(id, rows.clone());
        Ok(rows)
    }

    /// 后台把每个账号的额度刷一遍（不阻塞请求，错了就算了 —— 选号只是排序，不是硬门槛）。
    /// `force` 透给 `refresh_quota`：面板「现查额度」要的是真查，启动预热只是填缓存、不必。
    pub fn refresh_all_in_background(self: &Arc<Self>, force: bool) {
        for a in self.usable() {
            let id = a.id.clone();
            let email = a.email_masked.clone();
            let pool = Arc::clone(self);
            tokio::spawn(async move {
                if let Err(err) = pool.refresh_quota(&id, force).await {
                    (pool.log)(format!(
                        "额度预取失败（{email}）：{}",
                        truncate_chars(&err.to_string(), 140)
                    ));
                }
            });
        }
    }

    /// 把过期的额度在后台刷一遍 —— 面板轮询能拿到实时额度就靠它。
    ///
    /// 要补的账号有两类：**有快照但过期了**，以及**从没查到过快照**（新加进池子的、
    /// 从没预热成功的 —— 不然它们会永远停在 `NEUTRAL_SCORE`，面板也一直没数）。两类都
    /// 由同一个 TTL 节流窗口管着：常态下（缓存还新）一次上游都不打，`/healthz` 每 15 秒
    /// 调一次几乎零成本；一次现查可能数秒，跟着轮询频率打就成倍放大上游压力了。
    /// 只有真的触发刷新时才记时刻，所以「空转」不会推迟下一轮。
    pub fn refresh_stale_in_background(self: &Arc<Self>) {
        let now = self.now();
        let last = self.last_stale_refresh_at.load(AtomicOrdering::SeqCst);
        if now - last < self.quota_ttl_ms {
            return;
        }
        let stale: Vec<(String, String)> = self
            .usable()
            .into_iter()
            // 没有快照（`None`）也算要补：它同样「不可用于选号排序」，只是原因不同。
            .filter(|a| match self.quota_of(&a.id) {
                Some(q) => q.stale,
                None => true,
            })
            .map(|a| (a.id.clone(), a.email_masked.clone()))
            .collect();
        if stale.is_empty() {
            return;
        }
        // 用 CAS 抢节流位而不是直接 store：并发进来的 /healthz（面板 + 手点 + 多标签页）
        // 只有第一个能触发这一轮，其余看到窗口已开就退回去。
        if self
            .last_stale_refresh_at
            .compare_exchange(last, now, AtomicOrdering::SeqCst, AtomicOrdering::SeqCst)
            .is_err()
        {
            return;
        }
        for (id, email) in stale {
            let pool = Arc::clone(self);
            tokio::spawn(async move {
                if let Err(err) = pool.refresh_quota(&id, true).await {
                    (pool.log)(format!(
                        "额度自动刷新失败（{email}）：{}",
                        truncate_chars(&err.to_string(), 140)
                    ));
                }
            });
        }
    }

    /// 主账号（列表第一个可用的），用于 /v1/models、模型解析这类与账号无关的调用。
    pub fn primary_id(&self) -> Option<String> {
        self.usable().first().map(|a| a.id.clone())
    }

    pub async fn identity(&self) -> Value {
        let Some(id) = self.primary_id() else {
            return json!({});
        };
        let account = self.account(&id).unwrap_or_default();
        if !self.has_session(&id) {
            return json!({
                "email": account.email_masked,
                "project": account.project,
                "tier": Value::Null,
            });
        }
        match self.session(&id).await {
            Ok(session) => session.identity(),
            Err(_) => json!({ "email": account.email_masked, "project": account.project }),
        }
    }

    /// 模型解析/模型表：先用主账号，失败就轮到别的账号（模型表跟账号走，内容应该一致）。
    pub async fn with_any_session<T, F, Fut>(&self, f: F) -> Result<T, UpstreamError>
    where
        F: Fn(Arc<dyn Session>, Candidate) -> Fut,
        Fut: Future<Output = Result<T, UpstreamError>>,
    {
        let order = self.candidates(CandidateQuery::for_model("gemini-3.6-flash-high"));
        let mut last_error: Option<UpstreamError> = None;
        for cand in order {
            let id = cand.id.clone();
            match self.session(&id).await {
                Ok(session) => match f(session, cand).await {
                    Ok(value) => return Ok(value),
                    Err(err) => last_error = Some(err),
                },
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error.unwrap_or_else(|| UpstreamError::network("no_account", "池里没有可用账号")))
    }

    /// 给 /healthz 看的池状态：谁在冷却、谁还剩多少、最后一笔错是什么。
    pub fn state(&self) -> Vec<PoolState> {
        let now = self.now();
        let local_disabled = self.disabled_ids();
        self.visible()
            .into_iter()
            .map(|a| {
                let st = self.state_of(&a.id);
                let cached = self.quota_cache(&a.id);
                let breakers = st
                    .breakers
                    .iter()
                    .map(|(key, b)| {
                        let cooling = b.until > now;
                        PoolBreaker {
                            key: key.clone(),
                            failures: b.failures,
                            trips: b.trips,
                            cooling,
                            until: if cooling {
                                Some(iso_from_millis(b.until))
                            } else {
                                None
                            },
                            retry_in_seconds: if cooling {
                                (((b.until - now) as f64) / 1000.0).round() as i64
                            } else {
                                0
                            },
                        }
                    })
                    .collect();
                PoolState {
                    email: a.email_masked.clone(),
                    id: a.id.chars().take(8).collect(),
                    project: a.project.clone(),
                    disabled: local_disabled.contains(&a.id),
                    session_ready: self.has_session(&a.id),
                    breakers,
                    last_error: st.last_error.as_ref().map(|e| PoolLastError {
                        at: iso_from_millis(e.at),
                        status: e.status,
                        reason: e.reason.clone(),
                        message: e.message.clone(),
                    }),
                    last_used_at: if st.last_used_at != 0 {
                        Some(iso_from_millis(st.last_used_at))
                    } else {
                        None
                    },
                    quota: cached.map(|c| PoolQuota {
                        at: iso_from_millis(c.at),
                        // 呈现用的门槛比补查用的 TTL 宽（见 QUOTA_STALE_WARN_MS）：刚过 TTL、
                        // 补查还在路上的那一小段，不该在面板上闪一句「已过期」。
                        stale: now - c.at > QUOTA_STALE_WARN_MS,
                        rows: c.rows,
                    }),
                }
            })
            .collect()
    }
}

/// 对齐 JS 构造里那次账号归一化：补 id/emailMasked，把「禁用 / 待验证 / 没 refresh_token」
/// 统一折进 `disabled`（池子后面只看这一个标志）。
fn normalize_account(mut a: Account) -> Account {
    if a.id.is_empty() {
        a.id = a
            .email
            .clone()
            .or_else(|| {
                if a.email_masked.is_empty() {
                    None
                } else {
                    Some(a.email_masked.clone())
                }
            })
            .unwrap_or_default();
    }
    if a.email_masked.is_empty() {
        a.email_masked = a.email.clone().unwrap_or_default();
    }
    a.disabled = a.disabled || a.validation_blocked || a.refresh_token.is_none();
    a
}

fn breaker_get<'a>(breakers: &'a [(String, Breaker)], key: &str) -> Option<&'a Breaker> {
    breakers.iter().find(|(k, _)| k == key).map(|(_, b)| b)
}

fn breaker_set(breakers: &mut Vec<(String, Breaker)>, key: &str, value: Breaker) {
    if let Some(entry) = breakers.iter_mut().find(|(k, _)| k == key) {
        entry.1 = value;
    } else {
        breakers.push((key.to_string(), value));
    }
}

/// 写「账号 + 家族额度耗尽」标记：取旧值与新值的较大者，只延不缩。
///
/// 为什么只延不缩：两次耗尽之间可能夹着一份「还有余额」的旧快照，那不代表恢复；
/// 宁可多等一会儿，也不要把一个真耗尽的号提前排回前面白撞一次。
fn exhausted_set(entries: &mut Vec<(String, i64)>, family: &str, until: i64) {
    if let Some(entry) = entries.iter_mut().find(|(f, _)| f == family) {
        if until > entry.1 {
            entry.1 = until;
        }
    } else {
        entries.push((family.to_string(), until));
    }
}

/// JS 的 `String(x).slice(0, n)`；按字符截断（不是字节），避免把 UTF-8 切坏。
fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// 单账号伪装成池：给 `createBridgeServer` 的老接口（只传 upstream）兜底用。
///
/// 方法名与 [`AccountPool`] 对齐，但参数/返回按「只有一个账号」简化：
/// `open`/`session` 直接给注入的会话，`candidates` 恒为一条 `why = "single"`。
pub struct SingleAccountPool {
    session: Arc<dyn Session>,
}

impl SingleAccountPool {
    pub fn new(session: Arc<dyn Session>) -> Self {
        Self { session }
    }

    /// JS 的 `upstream` 字段。
    pub fn upstream(&self) -> Arc<dyn Session> {
        Arc::clone(&self.session)
    }

    /// 同步版：单账号不需要懒建。
    pub fn session(&self) -> Arc<dyn Session> {
        Arc::clone(&self.session)
    }

    pub async fn open(&self) -> Arc<dyn Session> {
        Arc::clone(&self.session)
    }

    fn single_candidate(&self) -> Candidate {
        Candidate {
            id: "default".to_string(),
            account: Account::default(),
            family: "gemini",
            why: "single",
            score: None,
            cooling: false,
        }
    }

    pub fn candidates(&self) -> Vec<Candidate> {
        vec![self.single_candidate()]
    }

    pub fn account(&self, _id: &str) -> Option<Account> {
        None
    }

    pub fn primary_id(&self) -> Option<String> {
        Some("default".to_string())
    }

    pub async fn identity(&self) -> Value {
        self.session.identity()
    }

    pub fn note_success(&self, _id: &str) {}

    pub fn note_failure(&self, _id: &str, _info: FailureInfo) -> FailureNote {
        FailureNote {
            tripped: false,
            breakers: Vec::new(),
        }
    }

    pub fn note_quota(&self, _id: &str, _quota: &QuotaSnapshot) {}

    pub fn remember(&self, _session_key: &str, _id: &str) {}

    pub fn quota_of(&self, _id: &str) -> Option<CachedQuota> {
        None
    }

    pub fn id_by_email(&self, _email: &str) -> Option<String> {
        None
    }

    pub fn earliest_retry_in_seconds(&self, _model: Option<&str>) -> Option<i64> {
        None
    }

    pub fn state(&self) -> Vec<PoolState> {
        Vec::new()
    }

    /// 单账号没有池子可刷：额度显示本来就跟着每次请求走，两个后台刷新口都是空实现。
    pub fn refresh_all_in_background(self: &Arc<Self>, _force: bool) {}

    pub fn refresh_stale_in_background(self: &Arc<Self>) {}

    pub async fn with_any_session<T, F, Fut>(&self, f: F) -> Result<T, UpstreamError>
    where
        F: Fn(Arc<dyn Session>, Candidate) -> Fut,
        Fut: Future<Output = Result<T, UpstreamError>>,
    {
        f(Arc::clone(&self.session), self.single_candidate()).await
    }
}

/// JS 的 `singleAccountPool(upstream)`：把单个会话包装成池的形状。
pub fn single_account_pool(session: Arc<dyn Session>) -> SingleAccountPool {
    SingleAccountPool::new(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

    use tokio::sync::mpsc::Sender;

    use crate::types::{GenerateRequest, ResolvedModel, StreamEvent};

    /// 假会话：JS 测试里那些 `{}` 对象的等价物。
    #[derive(Default)]
    struct FakeSession {
        identity: Value,
        quota: Option<QuotaSnapshot>,
    }

    #[async_trait::async_trait]
    impl Session for FakeSession {
        fn identity(&self) -> Value {
            self.identity.clone()
        }

        async fn load_code_assist(&self) -> Result<Value, UpstreamError> {
            Ok(Value::Null)
        }

        async fn models(&self) -> Result<Value, UpstreamError> {
            Ok(Value::Null)
        }

        async fn quota(&self) -> Result<QuotaSnapshot, UpstreamError> {
            Ok(self.quota.clone().unwrap_or_default())
        }

        async fn resolve_model(&self, requested: &str) -> ResolvedModel {
            ResolvedModel {
                model: requested.to_string(),
                substituted_from: None,
                reason: "test".to_string(),
            }
        }

        async fn stream_generate(
            &self,
            _req: GenerateRequest,
            _tx: Sender<StreamEvent>,
        ) -> Result<(), UpstreamError> {
            Ok(())
        }
    }

    fn account(email: &str) -> Account {
        let at = email.find('@').unwrap();
        Account {
            id: email.to_string(),
            email: Some(email.to_string()),
            email_masked: format!("{}***{}", &email[..1], &email[at..]),
            project: Some("proj-x".to_string()),
            refresh_token: Some("rt".to_string()),
            ..Default::default()
        }
    }

    /// `rows(gemini, threeP)`：与 JS 测试同一份额度跳摘要。
    fn rows(gemini: f64, three_p: f64) -> Vec<QuotaRow> {
        vec![
            QuotaRow {
                group: "Gemini Models".to_string(),
                window: "weekly".to_string(),
                remaining_percent: Some(gemini),
                reset_time: None,
            },
            QuotaRow {
                group: "Gemini Models".to_string(),
                window: "5h".to_string(),
                remaining_percent: Some((gemini + 10.0f64).min(100.0)),
                reset_time: None,
            },
            QuotaRow {
                group: "Claude and GPT models".to_string(),
                window: "weekly".to_string(),
                remaining_percent: Some(three_p),
                reset_time: None,
            },
        ]
    }

    fn snap(rows: Vec<QuotaRow>) -> QuotaSnapshot {
        QuotaSnapshot {
            summary: rows,
            ..Default::default()
        }
    }

    fn fixed_clock(t: i64) -> Clock {
        Arc::new(move || t)
    }

    /// 可推进的假时钟（JS 测试里那个 `let t = 1_000_000; now: () => t`）。
    struct TestClock {
        t: Arc<Mutex<i64>>,
    }

    impl TestClock {
        fn new(t: i64) -> Self {
            Self {
                t: Arc::new(Mutex::new(t)),
            }
        }

        fn clock(&self) -> Clock {
            let t = Arc::clone(&self.t);
            Arc::new(move || *t.lock().unwrap())
        }

        fn add(&self, delta: i64) {
            *self.t.lock().unwrap() += delta;
        }
    }

    fn fake_sessions(emails: &[&str]) -> HashMap<String, Arc<dyn Session>> {
        emails
            .iter()
            .map(|e| {
                (
                    (*e).to_string(),
                    Arc::new(FakeSession::default()) as Arc<dyn Session>,
                )
            })
            .collect()
    }

    /// JS 的 `makePool`：返回池子，外加一个「会话建过哪些账号」的记录。
    fn build(
        accounts: Vec<Account>,
        sessions: HashMap<String, Arc<dyn Session>>,
        now: Clock,
        sticky_max: Option<usize>,
    ) -> (Arc<AccountPool>, Arc<Mutex<Vec<String>>>) {
        let created: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let created_for_factory = Arc::clone(&created);
        let sessions = Arc::new(sessions);
        let factory: SessionFactory = Arc::new(move |a: Account| {
            let created = Arc::clone(&created_for_factory);
            let sessions = Arc::clone(&sessions);
            Box::pin(async move {
                let email = a.email.clone().unwrap_or_default();
                created.lock().unwrap().push(email.clone());
                match sessions.get(&email) {
                    Some(s) => Ok(Arc::clone(s)),
                    None => Err(UpstreamError::network(
                        "missing_session",
                        format!("没有 {email} 的会话"),
                    )),
                }
            })
        });
        let mut options = AccountPoolOptions::new(factory, accounts).now(now);
        if let Some(max) = sticky_max {
            options = options.sticky_max(max);
        }
        (Arc::new(AccountPool::new(options)), created)
    }

    #[test]
    fn family_of_model_splits_claude_gpt_into_3p() {
        assert_eq!(family_of_model("gemini-3.8-flash-tiered"), "gemini");
        assert_eq!(family_of_model("claude-sonnet-4-6"), "3p");
        assert_eq!(family_of_model("gpt-oss-120b-medium"), "3p");
    }

    #[test]
    fn score_for_family_takes_smallest_ratio_in_family() {
        let r = rows(80.0, 20.0);
        assert_eq!(score_for_family(&r, "gemini"), Some(0.8));
        assert_eq!(score_for_family(&r, "3p"), Some(0.2));
        assert_eq!(score_for_family(&[], "3p"), None);

        // JS 的 `Number(null) === 0`：字段缺失的行按 0 分参与取 min，把家族分拉到 0。
        let missing = vec![QuotaRow {
            group: "Gemini Models".to_string(),
            window: "weekly".to_string(),
            remaining_percent: None,
            reset_time: None,
        }];
        assert_eq!(score_for_family(&missing, "gemini"), Some(0.0));
        // 同家族里混一行缺失 + 一行有值：取 min 仍是 0。
        let mixed = vec![
            QuotaRow {
                group: "Gemini Models".to_string(),
                window: "weekly".to_string(),
                remaining_percent: None,
                reset_time: None,
            },
            QuotaRow {
                group: "Gemini Models".to_string(),
                window: "5h".to_string(),
                remaining_percent: Some(90.0),
                reset_time: None,
            },
        ];
        assert_eq!(score_for_family(&mixed, "gemini"), Some(0.0));
    }

    #[test]
    fn retryable_and_account_failure_classification() {
        for status in [0u16, 401, 403, 429, 500, 503] {
            assert!(is_retryable(status), "{status} 应该可以换号");
        }
        assert!(!is_retryable(400));
        assert!(!is_retryable(404));
        assert!(is_account_failure(429));
        assert!(!is_account_failure(500));
    }

    #[test]
    fn candidates_rank_by_quota() {
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            fake_sessions(&["a@x.com", "b@x.com"]),
            fixed_clock(1_000_000),
            None,
        );
        pool.note_quota("a@x.com", &snap(rows(10.0, 50.0)));
        pool.note_quota("b@x.com", &snap(rows(90.0, 50.0)));

        let order: Vec<String> = pool
            .candidates(CandidateQuery::for_model("gemini-3.8-flash-tiered"))
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(order, vec!["b@x.com", "a@x.com"]);

        // 只按家族算：3p 家族里 a 反而更高
        let order_3p: Vec<String> = pool
            .candidates(CandidateQuery::for_model("claude-sonnet-4-6"))
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(order_3p, vec!["a@x.com", "b@x.com"]);
    }

    #[test]
    fn candidates_put_sticky_first_when_within_tolerance() {
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            HashMap::new(),
            fixed_clock(1_000_000),
            None,
        );
        pool.note_quota("a@x.com", &snap(rows(70.0, 50.0)));
        pool.note_quota("b@x.com", &snap(rows(90.0, 50.0)));
        pool.remember("sess-1", "a@x.com");
        let order = pool.candidates(
            CandidateQuery::for_model("gemini-3.8-flash-tiered").with_session_key("sess-1"),
        );
        assert_eq!(order[0].id, "a@x.com"); // 70% ≥ 90% × 0.6，会话连续性优先
        assert_eq!(order[0].why, "sticky");
    }

    #[test]
    fn sticky_yields_when_its_quota_drops_too_far() {
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            HashMap::new(),
            fixed_clock(1_000_000),
            None,
        );
        pool.note_quota("a@x.com", &snap(rows(5.0, 50.0)));
        pool.note_quota("b@x.com", &snap(rows(90.0, 50.0)));
        pool.remember("sess-1", "a@x.com");
        // 5% < 90% × 0.6 → 不让粘性挡住选号
        let order = pool.candidates(
            CandidateQuery::for_model("gemini-3.8-flash-tiered").with_session_key("sess-1"),
        );
        assert_eq!(order[0].id, "b@x.com");
        assert_eq!(order[1].id, "a@x.com");
    }

    #[test]
    fn default_session_key_is_not_sticky() {
        let (pool, _created) = build(
            vec![account("a@x.com")],
            HashMap::new(),
            fixed_clock(1_000_000),
            None,
        );
        pool.remember("default", "a@x.com");
        assert_eq!(pool.sticky.lock().unwrap().len(), 0);
    }

    #[test]
    fn breaker_429_scopes_to_model_and_403_scopes_to_account() {
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            HashMap::new(),
            clock.clock(),
            None,
        );
        pool.note_failure(
            "a@x.com",
            FailureInfo::new(429)
                .reason("RATE_LIMIT")
                .model("gemini-3.8-flash-tiered"),
        );
        assert!(
            pool.cooldown_for("a@x.com", Some("gemini-3.8-flash-tiered"))
                .cooling
        );
        // 同一家族的别的模型不受影响：免费层下 pro 系稳定 429，flash 是好的
        assert!(
            !pool
                .cooldown_for("a@x.com", Some("gemini-3.6-flash-high"))
                .cooling
        );
        assert!(
            !pool
                .cooldown_for("a@x.com", Some("claude-sonnet-4-6"))
                .cooling
        );

        let ids = |model: &str| -> Vec<String> {
            pool.candidates(CandidateQuery::for_model(model))
                .into_iter()
                .map(|c| format!("{}:{}", c.id, c.cooling))
                .collect()
        };
        // 冷却只影响那一个模型：别的模型照常排前面
        assert_eq!(
            ids("gemini-3.8-flash-tiered"),
            vec!["b@x.com:false", "a@x.com:true"]
        );
        assert_eq!(
            ids("claude-sonnet-4-6"),
            vec!["a@x.com:false", "b@x.com:false"]
        );

        // 403 是账号级：两个家族一起记
        pool.note_failure(
            "a@x.com",
            FailureInfo::new(403)
                .reason("SUBSCRIPTION_REQUIRED")
                .model("gemini-3.8-flash-tiered"),
        );
        assert!(
            pool.cooldown_for("a@x.com", Some("claude-sonnet-4-6"))
                .cooling
        );
        assert_eq!(
            ids("claude-sonnet-4-6"),
            vec!["b@x.com:false", "a@x.com:true"]
        );
    }

    #[test]
    fn breaker_backoff_steps_and_reset_on_success() {
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(
            vec![account("a@x.com")],
            HashMap::new(),
            clock.clock(),
            None,
        );
        for _ in 0..5 {
            pool.note_failure(
                "a@x.com",
                FailureInfo::new(429)
                    .reason("RATE_LIMIT")
                    .model("gemini-3-flash"),
            );
        }
        let key = "model:gemini-3-flash";
        assert_eq!(pool.breaker_of("a@x.com", key).failures, 5);
        // 第 5 次用的是最后一档
        assert_eq!(
            pool.breaker_of("a@x.com", key).until - 1_000_000,
            BREAKER_STEPS_MS[BREAKER_STEPS_MS.len() - 1]
        );

        pool.note_success("a@x.com");
        assert!(!pool.breaker_of("a@x.com", key).cooling);
        assert_eq!(pool.breaker_of("a@x.com", key).failures, 0);
    }

    #[test]
    fn all_cooling_still_returns_candidates_and_earliest_retry() {
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            HashMap::new(),
            clock.clock(),
            None,
        );
        let model = "gemini-3.8-flash-tiered";
        pool.note_failure("a@x.com", FailureInfo::new(429).model(model)); // 60s → 1_060_000
        clock.add(10_000);
        pool.note_failure("b@x.com", FailureInfo::new(429).model(model)); // 到 1_070_000
        let order = pool.candidates(CandidateQuery::for_model(model));
        assert_eq!(order.len(), 2);
        assert_eq!(order[0].id, "a@x.com"); // 先醒
        assert_eq!(order[0].why, "breaker_forced");
        assert_eq!(order[1].id, "b@x.com");
        assert_eq!(pool.earliest_retry_in_seconds(Some(model)), Some(50));
    }

    #[test]
    fn low_quota_accounts_are_demoted_not_excluded() {
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            HashMap::new(),
            fixed_clock(1_000_000),
            None,
        );
        pool.note_quota("a@x.com", &snap(rows(0.5, 50.0))); // 0.5% → 低于 2% 阈值
        pool.note_quota("b@x.com", &snap(rows(30.0, 50.0)));
        let order = pool.candidates(CandidateQuery::for_model("gemini-3.8-flash-tiered"));
        assert_eq!(order[0].id, "b@x.com");
        assert_eq!(order[1].id, "a@x.com");
        assert_eq!(order[1].why, "low_quota");
    }

    #[test]
    fn disabled_accounts_are_not_candidates() {
        let mut blocked = account("b@x.com");
        blocked.validation_blocked = true;
        let mut no_token = account("c@x.com");
        no_token.refresh_token = None;
        let mut disabled = account("d@x.com");
        disabled.disabled = true;
        let (pool, _created) = build(
            vec![account("a@x.com"), blocked, no_token, disabled],
            HashMap::new(),
            fixed_clock(1_000_000),
            None,
        );
        let ids: Vec<String> = pool
            .candidates(CandidateQuery::for_model("gemini-3-flash"))
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["a@x.com"]);
    }

    #[tokio::test]
    async fn session_is_created_once_and_failures_are_not_cached() {
        let fail = Arc::new(AtomicBool::new(true));
        let count = Arc::new(AtomicUsize::new(0));
        let fail_for_factory = Arc::clone(&fail);
        let count_for_factory = Arc::clone(&count);
        let factory: SessionFactory = Arc::new(move |_a: Account| {
            let fail = Arc::clone(&fail_for_factory);
            let count = Arc::clone(&count_for_factory);
            Box::pin(async move {
                count.fetch_add(1, AtomicOrdering::SeqCst);
                if fail.load(AtomicOrdering::SeqCst) {
                    return Err(UpstreamError::network("sign", "签不动"));
                }
                Ok(Arc::new(FakeSession::default()) as Arc<dyn Session>)
            })
        });
        let pool = AccountPool::new(AccountPoolOptions::new(factory, vec![account("a@x.com")]));

        assert!(pool.session("a@x.com").await.is_err());
        fail.store(false, AtomicOrdering::SeqCst);
        let session = pool.session("a@x.com").await.unwrap();
        let _ = session.identity();
        pool.session("a@x.com").await.unwrap();
        assert_eq!(count.load(AtomicOrdering::SeqCst), 2); // 第一次失败 + 第二次成功，第三次走缓存
    }

    #[test]
    fn sticky_map_is_bounded_dropping_oldest() {
        let (pool, _created) = build(
            vec![account("a@x.com")],
            HashMap::new(),
            fixed_clock(1_000_000),
            Some(2),
        );
        pool.remember("s1", "a@x.com");
        pool.remember("s2", "a@x.com");
        pool.remember("s3", "a@x.com");
        let sticky = pool.sticky.lock().unwrap();
        assert_eq!(sticky.len(), 2);
        assert!(!sticky.iter().any(|(k, _)| k == "s1"));
        assert!(sticky.iter().any(|(k, _)| k == "s3"));
    }

    #[test]
    fn state_reports_cooling_quota_and_last_error() {
        let (pool, _created) = build(
            vec![account("a@x.com")],
            HashMap::new(),
            fixed_clock(1_000_000),
            None,
        );
        pool.note_quota("a@x.com", &snap(rows(42.0, 50.0)));
        pool.note_failure(
            "a@x.com",
            FailureInfo::new(429)
                .reason("RATE_LIMIT")
                .message("quota exceeded")
                .model("gemini-3-flash"),
        );
        let state = pool.state();
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].email, "a***@x.com");
        assert_eq!(state[0].breakers.len(), 1);
        assert!(state[0].breakers[0].cooling);
        assert!(state[0].breakers[0].retry_in_seconds > 0);
        assert_eq!(state[0].last_error.as_ref().unwrap().status, 429);
        assert_eq!(state[0].quota.as_ref().unwrap().rows.len(), 3);
    }

    #[test]
    fn breaker_keys_429_without_model_falls_back_to_families() {
        let both = vec!["family:gemini".to_string(), "family:3p".to_string()];
        assert_eq!(breaker_keys(429, None), both);
        assert_eq!(breaker_keys(429, Some("")), both.clone());
        assert_eq!(breaker_keys(403, Some("gemini-3-flash")), both);
        assert_eq!(
            breaker_keys(429, Some("gemini-3-flash")),
            vec!["model:gemini-3-flash".to_string()]
        );
    }

    #[test]
    fn earliest_retry_is_none_without_breakers() {
        let (pool, _created) = build(
            vec![account("a@x.com")],
            HashMap::new(),
            fixed_clock(1_000_000),
            None,
        );
        assert_eq!(pool.earliest_retry_in_seconds(Some("gemini-3-flash")), None);
        assert_eq!(pool.earliest_retry_in_seconds(None), None);
    }

    #[tokio::test]
    async fn open_returns_session_and_stamps_last_used() {
        let (pool, _created) = build(
            vec![account("a@x.com")],
            fake_sessions(&["a@x.com"]),
            fixed_clock(1_000_000),
            None,
        );
        let session = pool.open("a@x.com").await.unwrap();
        let _ = session.identity();
        let state = pool.state();
        assert!(state[0].session_ready);
        assert_eq!(
            state[0].last_used_at.as_deref(),
            Some("1970-01-01T00:16:40Z")
        );
    }

    #[tokio::test]
    async fn refresh_quota_caches_within_ttl() {
        let session = FakeSession {
            quota: Some(snap(rows(80.0, 50.0))),
            ..Default::default()
        };
        let mut sessions: HashMap<String, Arc<dyn Session>> = HashMap::new();
        sessions.insert("a@x.com".to_string(), Arc::new(session));
        let (pool, created) = build(
            vec![account("a@x.com")],
            sessions,
            fixed_clock(1_000_000),
            None,
        );
        let first = pool.refresh_quota("a@x.com", false).await.unwrap();
        assert_eq!(first.len(), 3);
        assert!(pool.quota_of("a@x.com").is_some());
        let _ = pool.refresh_quota("a@x.com", false).await.unwrap();
        assert_eq!(created.lock().unwrap().len(), 1); // TTL 内第二次走缓存，不再建会话
    }

    /// 数 `quota()` 真被调了几次；`fail` 打开后模拟上游查额度失败（不写缓存的那种）。
    struct QuotaCountingSession {
        calls: Arc<AtomicUsize>,
        fail: AtomicBool,
        rows: Vec<QuotaRow>,
    }

    #[async_trait::async_trait]
    impl Session for QuotaCountingSession {
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
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if self.fail.load(AtomicOrdering::SeqCst) {
                return Err(UpstreamError::network(
                    "quota_failed",
                    "测试：上游查额度失败",
                ));
            }
            Ok(QuotaSnapshot {
                summary: self.rows.clone(),
                ..Default::default()
            })
        }

        async fn resolve_model(&self, requested: &str) -> ResolvedModel {
            ResolvedModel {
                model: requested.to_string(),
                substituted_from: None,
                reason: "test".to_string(),
            }
        }

        async fn stream_generate(
            &self,
            _req: GenerateRequest,
            _tx: Sender<StreamEvent>,
        ) -> Result<(), UpstreamError> {
            Ok(())
        }
    }

    /// 后台刷新是 spawn 出去就撒手，测试只能等计数涨上来；涨不上去就失败而不是空转。
    async fn wait_for_quota_calls(calls: &Arc<AtomicUsize>, want: usize) {
        for _ in 0..200 {
            if calls.load(AtomicOrdering::SeqCst) >= want {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!(
            "等不到第 {want} 次额度查询（现在 {}）",
            calls.load(AtomicOrdering::SeqCst)
        );
    }

    #[tokio::test]
    async fn refresh_quota_refetches_after_ttl() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = Arc::new(QuotaCountingSession {
            calls: Arc::clone(&calls),
            fail: AtomicBool::new(false),
            rows: rows(80.0, 50.0),
        });
        let mut sessions: HashMap<String, Arc<dyn Session>> = HashMap::new();
        sessions.insert("a@x.com".to_string(), session as Arc<dyn Session>);
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(vec![account("a@x.com")], sessions, clock.clock(), None);

        pool.refresh_quota("a@x.com", false).await.unwrap();
        pool.refresh_quota("a@x.com", false).await.unwrap(); // TTL 内第二次走缓存
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        assert!(!pool.quota_of("a@x.com").unwrap().stale);

        clock.add(DEFAULT_QUOTA_TTL_MS + 1);
        pool.refresh_quota("a@x.com", false).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2); // 过期后必须真查
        assert!(!pool.quota_of("a@x.com").unwrap().stale);
    }

    #[tokio::test]
    async fn refresh_stale_in_background_triggers_once_per_ttl_window() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = Arc::new(QuotaCountingSession {
            calls: Arc::clone(&calls),
            fail: AtomicBool::new(false),
            rows: rows(80.0, 50.0),
        });
        let mut sessions: HashMap<String, Arc<dyn Session>> = HashMap::new();
        sessions.insert(
            "a@x.com".to_string(),
            Arc::clone(&session) as Arc<dyn Session>,
        );
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(vec![account("a@x.com")], sessions, clock.clock(), None);

        // 等价于启动预热刚填完缓存：没过 TTL 时面板怎么跳都不打上游……
        pool.refresh_quota("a@x.com", true).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        pool.refresh_stale_in_background();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        // ……而且「空转」不能占用节流窗口，否则真过期的那一刻会被无谓地推迟一个 TTL。
        assert_eq!(pool.last_stale_refresh_at.load(AtomicOrdering::SeqCst), 0);

        // 上游一直失败：缓存不会更新，账号会持续 stale —— 节流防的就是这种时候每 15 秒接着打。
        session.fail.store(true, AtomicOrdering::SeqCst);
        clock.add(DEFAULT_QUOTA_TTL_MS + 1);
        assert!(pool.quota_of("a@x.com").unwrap().stale);
        pool.refresh_stale_in_background();
        wait_for_quota_calls(&calls, 2).await;
        pool.refresh_stale_in_background(); // 同一 TTL 窗口内再跳：被节流挡住
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);

        // 窗口过去之后允许再试：一次失败不该把额度永久锁死在旧值上。
        clock.add(DEFAULT_QUOTA_TTL_MS);
        pool.refresh_stale_in_background();
        wait_for_quota_calls(&calls, 3).await;
    }

    #[tokio::test]
    async fn refresh_stale_in_background_also_fetches_accounts_without_a_snapshot() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = Arc::new(QuotaCountingSession {
            calls: Arc::clone(&calls),
            fail: AtomicBool::new(false),
            rows: rows(80.0, 50.0),
        });
        let mut sessions: HashMap<String, Arc<dyn Session>> = HashMap::new();
        sessions.insert(
            "a@x.com".to_string(),
            Arc::clone(&session) as Arc<dyn Session>,
        );
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(vec![account("a@x.com")], sessions, clock.clock(), None);

        // 从没查过：`quota_of` 是 None。以前这里会被跳过，账号永远拿不到快照
        // （选号一直用 0.5 中性分、面板也一直没数）。
        assert!(pool.quota_of("a@x.com").is_none());
        pool.refresh_stale_in_background();
        wait_for_quota_calls(&calls, 1).await;
        let cached = pool.quota_of("a@x.com").expect("补查后该有快照了");
        assert!(!cached.stale);

        // 还是同一个 TTL 节流窗口：紧接着再跳一次不会再打上游 —— 「没有快照」不能被
        // 当成「每 15 秒都要查一次」的借口。
        pool.refresh_stale_in_background();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);

        // 窗口过去、快照也过期：照常补。
        clock.add(DEFAULT_QUOTA_TTL_MS + 1);
        pool.refresh_stale_in_background();
        wait_for_quota_calls(&calls, 2).await;
    }

    /// 指定 id 的账号：前缀唯一性/歧义要自己掌握 id，不能靠邮箱撞出来。
    fn account_with_id(id: &str, email: &str) -> Account {
        let mut a = account(email);
        a.id = id.to_string();
        a
    }

    #[test]
    fn local_disable_takes_an_account_out_of_the_running_but_keeps_it_visible() {
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            HashMap::new(),
            fixed_clock(1_000_000),
            None,
        );
        assert_eq!(pool.usable().len(), 2);
        assert_eq!(pool.set_disabled("a@x.com", true).unwrap(), "a@x.com");
        // 选号里没了
        let ids: Vec<String> = pool
            .candidates(CandidateQuery::for_model("gemini-3.8-flash-tiered"))
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["b@x.com"]);
        // 但 /healthz（state）里还得在，而且标着 disabled —— 不然面板上没法再启用回来
        let state = pool.state();
        assert_eq!(state.len(), 2);
        assert!(state.iter().find(|s| s.id == "a@x.com").unwrap().disabled);
        assert!(!state.iter().find(|s| s.id == "b@x.com").unwrap().disabled);
        // 启用回来
        pool.set_disabled("a@x.com", false).unwrap();
        assert_eq!(pool.usable().len(), 2);
        assert!(pool.state().iter().all(|s| !s.disabled));
    }

    #[test]
    fn account_lookup_needs_a_unique_prefix() {
        let (pool, _created) = build(
            vec![
                account_with_id("bf00c418aaaa", "a@x.com"),
                account_with_id("bf00c418bbbb", "b@x.com"),
                account_with_id("cc11", "c@x.com"),
            ],
            HashMap::new(),
            fixed_clock(1_000_000),
            None,
        );
        assert_eq!(pool.set_disabled("cc11", true).unwrap(), "cc11");
        // 面板给的是 8 位前缀：撞车时如实报错，不替人挑一个
        let err = pool.set_disabled("bf00c418", true).unwrap_err();
        assert!(err.contains("不唯一"), "{err}");
        // 给长一点就唯一了
        assert_eq!(
            pool.set_disabled("bf00c418aa", true).unwrap(),
            "bf00c418aaaa"
        );
        assert!(pool
            .set_disabled("没人用这个 id", true)
            .unwrap_err()
            .contains("没有这个账号"));
        assert_eq!(pool.disabled_ids().len(), 2);
    }

    #[test]
    fn seeding_the_disabled_list_replaces_it_and_drops_strangers() {
        let (pool, _created) = build(
            vec![account("a@x.com")],
            HashMap::new(),
            fixed_clock(1_000_000),
            None,
        );
        // 认不出的 id（账号被删了、换过库）不该进名单
        pool.set_disabled_ids(vec!["a@x.com".to_string(), "已经不在库里的号".to_string()]);
        assert_eq!(
            pool.disabled_ids().into_iter().collect::<Vec<String>>(),
            vec!["a@x.com".to_string()]
        );
        // 再灌一次是「替换」，不是追加
        pool.set_disabled_ids(Vec::new());
        assert!(pool.disabled_ids().is_empty());
        assert_eq!(pool.usable().len(), 1);
    }

    #[test]
    fn store_disabled_accounts_are_not_even_listed() {
        let mut broken = account("c@x.com");
        broken.disabled = true; // 账号库标了废（归一化时折进来的）
        let (pool, _created) = build(
            vec![account("a@x.com"), broken],
            HashMap::new(),
            fixed_clock(1_000_000),
            None,
        );
        assert_eq!(pool.usable().len(), 1);
        let state = pool.state();
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].id, "a@x.com");
    }

    #[test]
    fn only_explicit_quota_429_counts_as_exhaustion() {
        // 实测的额度耗尽口径（docs/PROTOCOL.md）：ErrorInfo.reason = QUOTA_EXHAUSTED
        assert!(is_quota_exhaustion(
            &FailureInfo::new(429).reason("QUOTA_EXHAUSTED")
        ));
        // 有的网关只给 message：要挑明确说「超了」的措辞
        assert!(is_quota_exhaustion(
            &FailureInfo::new(429).message("You exceeded your current quota")
        ));
        // RESOURCE_EXHAUSTED 是 429 的通用 status（限流也用它）：不能据此封掉整个家族
        assert!(!is_quota_exhaustion(
            &FailureInfo::new(429).reason("RESOURCE_EXHAUSTED")
        ));
        assert!(!is_quota_exhaustion(
            &FailureInfo::new(429)
                .reason("RATE_LIMIT")
                .message("Rate limit exceeded")
        ));
        // reason 明确是限流时，message 里出现 quota 字样也不改判
        assert!(!is_quota_exhaustion(
            &FailureInfo::new(429)
                .reason("RATE_LIMIT")
                .message("quota exceeded")
        ));
        // 「check quota」这类限流文案里的 quota 字样不该被当成耗尽
        assert!(!is_quota_exhaustion(
            &FailureInfo::new(429).message("Resource has been exhausted (e.g. check quota)")
        ));
        // 别的状态码一律走原路
        assert!(!is_quota_exhaustion(
            &FailureInfo::new(403).reason("QUOTA_EXHAUSTED")
        ));
    }

    #[test]
    fn quota_exhausted_429_switches_to_the_other_account() {
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            HashMap::new(),
            clock.clock(),
            None,
        );
        // 两个号额度都正常、a 的还更高：不干预的话它会一直排第一。
        pool.note_quota("a@x.com", &snap(rows(90.0, 50.0)));
        pool.note_quota("b@x.com", &snap(rows(60.0, 50.0)));
        let model = "gemini-3.8-flash-tiered";
        assert_eq!(
            pool.candidates(CandidateQuery::for_model(model))[0].id,
            "a@x.com"
        );

        // a 在上游明确回了「额度耗尽」。
        pool.note_failure(
            "a@x.com",
            FailureInfo::new(429)
                .reason("QUOTA_EXHAUSTED")
                .message("You exceeded your current quota")
                .model(model),
        );
        assert!(pool.family_exhausted("a@x.com", "gemini"));
        let order: Vec<String> = pool
            .candidates(CandidateQuery::for_model(model))
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(order, vec!["b@x.com", "a@x.com"]);
        // 同家族的另一个模型也要避开它：额度是家族级的，不是模型级的。
        let order_other: Vec<String> = pool
            .candidates(CandidateQuery::for_model("gemini-3.6-flash-high"))
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(order_other, vec!["b@x.com", "a@x.com"]);
        // 别的家族（3p）不受影响：a 的 3p 额度是好的（别把封禁放大到家族之外）。
        let order_3p: Vec<String> = pool
            .candidates(CandidateQuery::for_model("claude-sonnet-4-6"))
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(order_3p, vec!["a@x.com", "b@x.com"]);
    }

    #[test]
    fn pure_rate_limit_429_stays_model_scoped() {
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            HashMap::new(),
            clock.clock(),
            None,
        );
        pool.note_quota("a@x.com", &snap(rows(90.0, 50.0)));
        pool.note_quota("b@x.com", &snap(rows(60.0, 50.0)));
        pool.note_failure(
            "a@x.com",
            FailureInfo::new(429)
                .reason("RATE_LIMIT")
                .model("gemini-3.8-flash-tiered"),
        );
        // 没有家族耗尽标记：同家族的别的模型照样能用 a（pro 429 不该拖 flash 下水）
        assert!(!pool.family_exhausted("a@x.com", "gemini"));
        let ids = |m: &str| -> Vec<String> {
            pool.candidates(CandidateQuery::for_model(m))
                .into_iter()
                .map(|c| c.id)
                .collect()
        };
        assert_eq!(ids("gemini-3.8-flash-tiered"), vec!["b@x.com", "a@x.com"]);
        // 同家族的别的模型：a 只是被记了一笔模型熔断，没被家族标记连坐，a 的 90% 仍然最高。
        assert_eq!(ids("gemini-3.6-flash-high"), vec!["a@x.com", "b@x.com"]);
    }

    #[test]
    fn all_accounts_exhausted_still_returns_a_candidate() {
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            HashMap::new(),
            clock.clock(),
            None,
        );
        let model = "gemini-3.8-flash-tiered";
        pool.note_quota("a@x.com", &snap(rows(0.0, 50.0)));
        pool.note_quota("b@x.com", &snap(rows(0.0, 50.0)));
        let cands = pool.candidates(CandidateQuery::for_model(model));
        // 全都耗尽也不能空手：必须挑一个去试（「不能因为看起来都没额度就不给服务」）
        assert_eq!(cands.len(), 2);
        assert!(cands.iter().all(|c| c.why == "exhausted"), "{cands:?}");
        // 一个都没有了也不能返回空：把仅剩的那个标记过的号交出去
        pool.set_disabled("b@x.com", true).unwrap();
        let only = pool.candidates(CandidateQuery::for_model(model));
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].id, "a@x.com");
        assert_eq!(only[0].why, "exhausted");
    }

    #[test]
    fn unknown_or_stale_quota_accounts_are_still_selectable() {
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            HashMap::new(),
            clock.clock(),
            None,
        );
        // a 从来没查到过额度：未知不等于耗尽，不能被 0 分误伤。
        pool.note_quota("b@x.com", &snap(rows(90.0, 50.0)));
        assert!(!pool.family_exhausted("a@x.com", "gemini"));
        let order: Vec<String> = pool
            .candidates(CandidateQuery::for_model("gemini-3.8-flash-tiered"))
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(order, vec!["b@x.com", "a@x.com"]);

        // a 的额度有过、但已经 stale：仍然照常参与（未知/过期 ≠ 耗尽）。
        pool.note_quota("a@x.com", &snap(rows(80.0, 50.0)));
        clock.add(DEFAULT_QUOTA_TTL_MS + 1);
        assert!(pool.quota_of("a@x.com").unwrap().stale);
        let order: Vec<String> = pool
            .candidates(CandidateQuery::for_model("gemini-3.8-flash-tiered"))
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(order, vec!["b@x.com", "a@x.com"]);
        assert!(order.contains(&"a@x.com".to_string()));
    }

    #[test]
    fn exhausted_sticky_account_does_not_block_switching() {
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(
            vec![account("a@x.com"), account("b@x.com")],
            HashMap::new(),
            clock.clock(),
            None,
        );
        pool.note_quota("a@x.com", &snap(rows(90.0, 50.0)));
        pool.note_quota("b@x.com", &snap(rows(60.0, 50.0)));
        pool.remember("sess-1", "a@x.com");
        let model = "gemini-3.8-flash-tiered";
        // 额度耗尽后，即使 a 是会话粘性账号，也不能靠粘性被顶回队首。
        pool.note_failure(
            "a@x.com",
            FailureInfo::new(429).reason("QUOTA_EXHAUSTED").model(model),
        );
        let order: Vec<String> = pool
            .candidates(CandidateQuery::for_model(model).with_session_key("sess-1"))
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(order, vec!["b@x.com", "a@x.com"]);
    }

    #[test]
    fn quota_exhaustion_marker_is_family_scoped_and_expires() {
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(
            vec![account("a@x.com")],
            HashMap::new(),
            clock.clock(),
            None,
        );
        // 纯限流：只记模型熔断，不碰家族标记。
        pool.note_failure(
            "a@x.com",
            FailureInfo::new(429)
                .reason("RATE_LIMIT")
                .model("gemini-3.8-flash-tiered"),
        );
        assert!(!pool.family_exhausted("a@x.com", "gemini"));
        assert!(
            pool.cooldown_for("a@x.com", Some("gemini-3.8-flash-tiered"))
                .cooling
        );

        // 明确说配额用尽：写家族标记，而且不记模型熔断（否则一次成功会把额度事实也清掉）。
        pool.note_failure(
            "a@x.com",
            FailureInfo::new(429)
                .reason("QUOTA_EXHAUSTED")
                .message("quota exceeded")
                .model("claude-sonnet-4-6"),
        );
        assert!(pool.family_exhausted("a@x.com", "3p"));
        assert!(!pool.family_exhausted("a@x.com", "gemini"));
        assert!(
            !pool
                .breaker_of("a@x.com", "model:claude-sonnet-4-6")
                .cooling
        );

        // 到点自然过期，不需要别人来擦。
        clock.add(DEFAULT_QUOTA_EXHAUSTED_MS + 1);
        assert!(!pool.family_exhausted("a@x.com", "3p"));
    }

    #[test]
    fn fresh_quota_snapshot_clears_the_exhaustion_marker() {
        let clock = TestClock::new(1_000_000);
        let (pool, _created) = build(
            vec![account("a@x.com")],
            HashMap::new(),
            clock.clock(),
            None,
        );
        pool.note_failure(
            "a@x.com",
            FailureInfo::new(429)
                .reason("QUOTA_EXHAUSTED")
                .model("gemini-3.8-flash-tiered"),
        );
        assert!(pool.family_exhausted("a@x.com", "gemini"));
        // 新快照说 gemini 又有余额了：标记立刻擦掉，别把恢复的号一直排最后。
        pool.note_quota("a@x.com", &snap(rows(80.0, 50.0)));
        assert!(!pool.family_exhausted("a@x.com", "gemini"));
        // 只擦有余额的那个家族：3p 的标记留着。
        pool.note_failure(
            "a@x.com",
            FailureInfo::new(429)
                .reason("QUOTA_EXHAUSTED")
                .model("claude-sonnet-4-6"),
        );
        assert!(pool.family_exhausted("a@x.com", "3p"));
        pool.note_quota("a@x.com", &snap(rows(80.0, 0.0)));
        assert!(pool.family_exhausted("a@x.com", "3p"));
    }
}
