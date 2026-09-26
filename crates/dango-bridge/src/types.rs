//! 跨模块共享的类型与 trait。
//!
//! 这里的类型只放「两个以上模块都要用」的东西；只有自己用的形状留在各自模块里。

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::Sender;

use crate::quota::{ModelInfo, ModelLimit, QuotaGroup};

/// 桥账号库里的一个账号。
///
/// `refresh_token` 只在内存里流转：读自老桥的账号文件（只读），换 access_token 用，
/// 不打印、不落盘、不进仓库。
#[derive(Debug, Clone, Default)]
pub struct Account {
    pub id: String,
    pub email: Option<String>,
    /// 对外只出现掩码邮箱
    pub email_masked: String,
    pub project: Option<String>,
    pub refresh_token: Option<String>,
    pub disabled: bool,
    pub validation_blocked: bool,
    pub is_current: bool,
}

/// 一跳额度摘要：面板与选号都用它（分组名 + 窗口 + 剩余百分比）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaRow {
    pub group: String,
    pub window: String,
    pub remaining_percent: Option<f64>,
    pub reset_time: Option<String>,
}

/// 上游错误。`status == 0` 表示网络层失败（连不上/超时），不是 HTTP 状态码。
/// 熔断口径就看这个 status：401/403 账号级、429 只记到具体模型。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{status} {reason}: {message}")]
pub struct UpstreamError {
    pub status: u16,
    pub reason: String,
    pub message: String,
}

impl UpstreamError {
    pub fn network(reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: 0,
            reason: reason.into(),
            message: message.into(),
        }
    }

    pub fn http(status: u16, reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
            message: message.into(),
        }
    }
}

/// 模型名解析结果（对应 JS 的 `{ model, substitutedFrom, reason }`）。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedModel {
    pub model: String,
    pub substituted_from: Option<String>,
    /// 见 `models::resolve_model`：normalized / version_match / token_match / family_fallback /
    /// default_fallback / no_model_list
    pub reason: String,
}

/// 查额度那次调用的完整结果：分组窗口 + 模型表 + 模型上限。
#[derive(Debug, Clone, Default)]
pub struct QuotaSnapshot {
    pub endpoint: Option<String>,
    pub groups: Vec<QuotaGroup>,
    pub summary: Vec<QuotaRow>,
    pub models: Vec<ModelInfo>,
    pub model_default: Option<String>,
    pub models_deprecated: usize,
    /// 客户端给的预算要按它收口（`max_tokens` 撞上模型硬上限时）
    pub model_limits: HashMap<String, ModelLimit>,
}

/// 一次流式生成的入参。
#[derive(Debug, Clone)]
pub struct GenerateRequest {
    pub model: String,
    pub request: Value,
    /// 真会话的 key（Anthropic 侧的会话指纹）；"default" 表示不是真会话，不参与粘性
    pub session_key: Option<String>,
    pub request_id: String,
}

/// 上游流事件，和 JS 的 `{type:"open"|"chunk"|"no-data"|"error"}` 一一对应。
///
/// 为什么要有 `Open` / `NoData` 这两种控制事件：服务层必须在「还没给客户端吐字节」的
/// 时候换账号或换端点，所以开流成功、开流失败、以及「200 但没有一个 data: 行」
/// （上游对下线模型就是这么回的）这三件事得让上层看得见。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Open { endpoint: String },
    Chunk(Value),
    NoData { endpoint: String, raw_text: String },
    Error(UpstreamError),
}

/// 一个账号的会话：签 token、调上游。池子只用这组接口，所以选号/熔断/粘性可以离线测。
#[async_trait::async_trait]
pub trait Session: Send + Sync {
    /// 对外呈现的身份（掩码邮箱、project、tier）
    fn identity(&self) -> Value;
    async fn load_code_assist(&self) -> Result<Value, UpstreamError>;
    async fn models(&self) -> Result<Value, UpstreamError>;
    async fn quota(&self) -> Result<QuotaSnapshot, UpstreamError>;
    /// 上游专有的「原始额度 JSON」。默认未实现（Antigravity 走 `quota` 就够）；
    /// Qoder 用它把 `GET {openapi}/api/v2/quota/usage` 的响应原样透传给 `/quota/qoder`。
    /// 给默认实现是为了不惊动既有实现者（单测里的假会话照旧）。
    async fn quota_raw(&self) -> Result<Value, UpstreamError> {
        Err(UpstreamError::http(
            501,
            "quota_raw",
            "这个上游没有原始额度接口",
        ))
    }
    async fn resolve_model(&self, requested: &str) -> ResolvedModel;
    /// 逐块把事件推进 channel。`Error` 只在还没吐过 `Chunk` 时发（吐过就不能换端点了）。
    async fn stream_generate(
        &self,
        req: GenerateRequest,
        tx: Sender<StreamEvent>,
    ) -> Result<(), UpstreamError>;
}

/// epoch 毫秒 → ISO8601（UTC、秒精度）。只用于日志与面板，不参与任何判断。
///
/// 手写是为了不引 chrono/time 依赖：日期算法用 Howard Hinnant 的 civil_from_days，
/// 对 1970 年之后的时间足够，测试里钉了几个已知值。
pub fn iso_from_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // days_from_civil 的逆运算：把「1970-01-01 起的天数」换回 (年, 月, 日)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// 现在的 epoch 毫秒（面板与熔断都用它；测试里注入假时钟）。
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_from_millis_matches_known_instants() {
        assert_eq!(iso_from_millis(0), "1970-01-01T00:00:00Z");
        // 下面这几个用 macOS 的 `date -u -r <秒>` 对过（别凭手感改）
        assert_eq!(iso_from_millis(1_789_000_000_000), "2026-09-10T00:26:40Z");
        assert_eq!(iso_from_millis(1_700_000_000_000), "2023-11-14T22:13:20Z");
        // 闰日与跨年边界
        assert_eq!(iso_from_millis(1_709_164_800_000), "2024-02-29T00:00:00Z");
        assert_eq!(iso_from_millis(1_735_689_599_000), "2024-12-31T23:59:59Z");
    }
}
