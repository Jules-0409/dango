//! Cursor 的取数（含 Grok Bot 周额度）：本地登录态、官方 `usage-summary` 和
//! Connect-RPC 的 `GetSandUsageStatus`。拆子模块只是为了文件别太长，
//! `crate::probes::cursor::*` 的公开路径保持不变。
//!
//! 几个内部类型（`CursorAuth` / `GrokUsage`）故意留在本模块：子模块之间不能互看
//! 私有字段，放在共同的父模块里，auth / usage 负责构造、quota 负责读取，谁都不用写
//! `pub(super)` 来破封装。

mod auth;
mod events;
mod quota;
mod time;
mod usage;

pub use events::{fetch_usage_events, CursorUsageEvent};
pub use quota::query_cursor_quota;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CursorQuota {
    pub ok: bool,
    pub id: String,
    pub name: String,
    pub plan_name: Option<String>,
    pub email: Option<String>,
    pub user_id: Option<String>,
    pub membership_type: Option<String>,
    pub subscription_status: Option<String>,
    pub auto_percent_used: Option<f64>,
    pub api_percent_used: Option<f64>,
    pub total_percent_used: Option<f64>,
    pub fast_requests_used: Option<i64>,
    pub fast_requests_limit: Option<i64>,
    pub fast_requests_remaining_percent: Option<f64>,
    pub cycle_start: Option<String>,
    pub cycle_end: Option<String>,
    pub cycle_reset_unix: Option<i64>,
    pub auto_message: Option<String>,
    pub api_message: Option<String>,
    /// Grok Bot 的周额度。Grok Bot 是 Cursor 的产品，用量由 Cursor 的
    /// `GetSandUsageStatus` 提供、吃的也是 Cursor 的 accessToken，所以合并在这一张卡里。
    /// 两个接口可能各自独立失败，故 Grok 的错误单独放 `grok_error`。
    pub grok_percent_used: Option<f64>,
    pub grok_has_available_usage: Option<bool>,
    pub grok_reset_unix: Option<i64>,
    pub grok_error: Option<String>,
    pub fetched_at: i64,
    pub error: Option<String>,
}

struct CursorAuth {
    token: String,
    user_id: String,
    /// 网页会话 Cookie 的用户段。Cursor 现在用的是带 scope 的 auth id
    /// （形如 `grok|user_01M…`），不再是 cli-config 里的数字 userId。
    auth_id: String,
    email: Option<String>,
    membership_type: Option<String>,
    subscription_status: Option<String>,
}

#[derive(Default)]
struct GrokUsage {
    percent_used: Option<f64>,
    has_available_usage: Option<bool>,
    reset_unix: Option<i64>,
    error: Option<String>,
}
