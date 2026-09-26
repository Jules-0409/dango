use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    pub used_percent: f64,
    pub seconds_remaining: Option<i64>,
    pub window_end: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FactoryWindows {
    pub standard: Option<HashMap<String, QuotaWindow>>,
    pub core: Option<HashMap<String, QuotaWindow>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FactoryQuota {
    pub ok: bool,
    pub id: String,
    pub name: String,
    pub plan_name: Option<String>,
    pub org_id: Option<String>,
    pub windows: FactoryWindows,
    pub overage_preference: Option<String>,
    pub extra_usage_balance_cents: Option<i64>,
    pub extra_usage_allowed: Option<bool>,
    pub fetched_at: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DevinQuota {
    pub ok: bool,
    pub id: String,
    pub name: String,
    pub plan_name: Option<String>,
    pub email: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub daily_remaining_percent: Option<f64>,
    pub weekly_remaining_percent: Option<f64>,
    pub daily_reset_at_unix: Option<i64>,
    pub weekly_reset_at_unix: Option<i64>,
    pub overage_balance_micros: Option<i64>,
    pub acu_consumed: Option<i64>,
    pub acu_limit: Option<i64>,
    pub plan_start_unix: Option<i64>,
    pub plan_end_unix: Option<i64>,
    pub source: Option<String>,
    pub fetched_at: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QuotaResults {
    pub factory: Option<FactoryQuota>,
    pub devin: Option<DevinQuota>,
    /// 含 Grok Bot 的周额度：Grok Bot 是 Cursor 的产品，两者共用同一份 Cursor 登录态
    pub cursor: Option<crate::probes::cursor::CursorQuota>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub refresh_minutes: u64,
    pub warn_percent: f64,
    pub danger_percent: f64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            refresh_minutes: 5,
            warn_percent: 70.0,
            danger_percent: 90.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaPayload {
    pub results: QuotaResults,
    pub config: AppConfig,
    pub at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 前端（ui/index.html）直接按 camelCase 读这些字段：`r.autoPercentUsed`、
    /// `r.grokPercentUsed`、`c.warnPercent`…… 一旦有人把 `rename_all` 去掉，
    /// 界面不会报错，只会静默显示「无数据」。
    /// 这里把契约钉死，改字段名会红。
    #[test]
    fn quota_payload_serializes_to_the_camel_case_contract() {
        let payload = QuotaPayload {
            results: QuotaResults {
                factory: Some(FactoryQuota {
                    ok: true,
                    plan_name: Some("Pro".into()),
                    extra_usage_balance_cents: Some(1234),
                    ..Default::default()
                }),
                devin: Some(DevinQuota {
                    ok: true,
                    daily_remaining_percent: Some(25.0),
                    daily_reset_at_unix: Some(1_700_000_000),
                    ..Default::default()
                }),
                cursor: Some(crate::probes::cursor::CursorQuota {
                    ok: true,
                    auto_percent_used: Some(12.5),
                    grok_percent_used: Some(80.0),
                    grok_has_available_usage: Some(false),
                    ..Default::default()
                }),
            },
            config: AppConfig::default(),
            at: 1_700_000_000_000,
        };

        let json = serde_json::to_value(&payload).unwrap();

        // 前端实际读取的每个字段都必须在
        assert_eq!(json["results"]["factory"]["planName"], "Pro");
        assert_eq!(json["results"]["factory"]["extraUsageBalanceCents"], 1234);
        assert_eq!(json["results"]["devin"]["dailyRemainingPercent"], 25.0);
        assert_eq!(
            json["results"]["devin"]["dailyResetAtUnix"],
            1_700_000_000i64
        );
        assert_eq!(json["results"]["cursor"]["autoPercentUsed"], 12.5);
        assert_eq!(json["results"]["cursor"]["grokPercentUsed"], 80.0);
        assert_eq!(json["results"]["cursor"]["grokHasAvailableUsage"], false);
        assert_eq!(json["config"]["refreshMinutes"], 5);
        assert_eq!(json["config"]["warnPercent"], 70.0);
        assert_eq!(json["config"]["dangerPercent"], 90.0);

        // 不能出现 snake_case 的漏网字段
        assert!(
            json["config"].get("refresh_minutes").is_none(),
            "序列化结果里不应出现 snake_case 字段"
        );
        assert!(
            json["results"]["factory"].get("plan_name").is_none(),
            "序列化结果里不应出现 snake_case 字段"
        );
    }

    /// 已删除的 stale 字段不能复活：README 明确说 Devin 不做本地缓存兜底，
    /// 前端也已经不再读它。
    #[test]
    fn devin_has_no_stale_fields() {
        let json = serde_json::to_value(DevinQuota::default()).unwrap();
        assert!(json.get("stale").is_none());
        assert!(json.get("staleReason").is_none());
    }

    /// 默认阈值必须和文档、CLI（bin/quota.rs 里的常量）保持一致。
    #[test]
    fn default_config_matches_documented_thresholds() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.refresh_minutes, 5);
        assert_eq!(cfg.warn_percent, 70.0);
        assert_eq!(cfg.danger_percent, 90.0);
    }

    /// 反序列化要能吃下 camelCase，否则前端回传配置时会失败。
    #[test]
    fn config_deserializes_from_camel_case() {
        let cfg: AppConfig = serde_json::from_str(
            r#"{"refreshMinutes":10,"warnPercent":50.0,"dangerPercent":80.0}"#,
        )
        .unwrap();
        assert_eq!(cfg.refresh_minutes, 10);
        assert_eq!(cfg.warn_percent, 50.0);
        assert_eq!(cfg.danger_percent, 80.0);
    }
}
