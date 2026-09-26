use serde::Deserialize;

use super::GrokUsage;

/// Cursor 官方的 usage-summary：Auto / API / 综合用量百分比与账期
pub(super) async fn fetch_usage_summary(
    client: &reqwest::Client,
    auth: &super::CursorAuth,
) -> Result<serde_json::Value, String> {
    // 网络错误 / 5xx / 429 自动重试（最多 3 次尝试，300ms、900ms 指数退避）
    let resp = crate::probes::http::send_with_retry(|| {
        let mut req = client
            .get("https://cursor.com/api/usage-summary")
            .header("Authorization", format!("Bearer {}", auth.token))
            .timeout(std::time::Duration::from_secs(8));
        // Cookie 的用户段是带 scope 的 auth id；老版本 Cursor 本地只有数字 userId，退回用它
        let cookie_user = if !auth.auth_id.is_empty() {
            auth.auth_id.as_str()
        } else {
            auth.user_id.as_str()
        };
        if !cookie_user.is_empty() {
            req = req.header(
                "Cookie",
                format!(
                    "WorkosCursorSessionToken={}%3A%3A{}",
                    cookie_user, auth.token
                ),
            );
        }
        req
    })
    .await
    .map_err(|e| format!("cursor.net_failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err("cursor.auth_expired".into());
        }
        return Err(format!("HTTP {status}"));
    }

    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| format!("cursor.json_parse_failed: {e}"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SandUsageStatus {
    #[serde(default)]
    pub usage_percent: Option<f64>,
    #[serde(default)]
    pub has_available_usage: Option<bool>,
    /// 服务端这个字段的类型不稳定（ISO 字符串或 epoch 数字），先原样收下再解析
    #[serde(default)]
    pub next_reset_timestamp_utc: Option<serde_json::Value>,
}

/// Grok Bot 的周额度。Grok Bot 是 Cursor 的产品，这个 Connect-RPC 接口吃的就是
/// Cursor 的 accessToken，和 usage-summary 共用同一份登录态。
///
/// 不要用 Grok Bot 桌面应用 `sand-secrets.json` 里的 `cursor-access-token`：那是
/// Electron safeStorage 加密过的密文（base64 解出来以 `v10` 开头），当 Bearer 发会被
/// 服务端拒绝为 401 `ERROR_NOT_LOGGED_IN`。也正因如此，Grok Bot 装没装、跑没跑都不影响
/// 这里能不能取到数——只取决于有没有登录 Cursor。
pub(super) async fn fetch_grok_usage(client: &reqwest::Client, token: &str) -> GrokUsage {
    let failed = |error: String| GrokUsage {
        error: Some(error),
        ..Default::default()
    };

    let resp = match crate::probes::http::send_with_retry(|| {
        client
            .post("https://api2.cursor.sh/aiserver.v1.DashboardService/GetSandUsageStatus")
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            // Connect-RPC 的协议版本声明，不是身份伪装，必须保留
            .header("Connect-Protocol-Version", "1")
            .body("{}")
    })
    .await
    {
        Ok(r) => r,
        Err(e) => return failed(format!("grok.net_failed: {e}")),
    };

    if !resp.status().is_success() {
        let code = resp.status();
        return failed(if code.as_u16() == 401 {
            "cursor.auth_expired".into()
        } else {
            format!("HTTP {code}")
        });
    }

    let data = match resp.json::<SandUsageStatus>().await {
        Ok(d) => d,
        Err(e) => return failed(format!("grok.parse_failed: {e}")),
    };

    GrokUsage {
        percent_used: data.usage_percent,
        has_available_usage: data.has_available_usage,
        reset_unix: data.next_reset_timestamp_utc.and_then(|timestamp| {
            crate::models::timestamp_to_millis(&timestamp)
                .and_then(|millis| i64::try_from(millis / 1000).ok())
        }),
        error: None,
    }
}
