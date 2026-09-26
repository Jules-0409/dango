// 前后端契约：全部 camelCase，前端按 plan.buckets[].remainingPercent 读。
use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Bucket {
    pub label: String,
    /// 剩余百分比 0–100；拿不到时 None，界面显示"无数据"
    pub remaining_percent: Option<f64>,
    pub detail: Option<String>,
    /// Unix timestamp in milliseconds when this bucket resets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    /// 额度所属的池（如 "standard"/"core"、"Gemini"、"Claude & GPT"、账号脱敏名）。
    /// 结构化字段，给之后的 UI 用；label 里自带 pool 前缀供现有卡片直接显示。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// 窗口口径，规范化为 "5h" | "day" | "week" | "month" | "total" | "other"。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
}

/// One request observed by a local proxy. Paths never include query strings.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RecentRequest {
    pub at: u64,
    pub method: String,
    pub path: String,
    pub model: Option<String>,
    pub status: Option<u16>,
    pub ms: Option<u64>,
}

/// Convert an ISO-8601 timestamp or Unix seconds/milliseconds to Unix ms.
pub(crate) fn timestamp_to_millis(value: &serde_json::Value) -> Option<u64> {
    if let Some(timestamp) = value.as_i64() {
        return integer_timestamp_to_millis(timestamp);
    }
    if let Some(timestamp) = value.as_u64() {
        return integer_timestamp_to_millis(i64::try_from(timestamp).ok()?);
    }
    let text = value.as_str()?;
    if let Ok(timestamp) = text.parse::<i64>() {
        return integer_timestamp_to_millis(timestamp);
    }
    let timestamp_ms = chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|timestamp| timestamp.and_utc().timestamp_millis())
        })
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .ok()
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .map(|timestamp| timestamp.and_utc().timestamp_millis())
        })?;
    u64::try_from(timestamp_ms).ok()
}

fn integer_timestamp_to_millis(timestamp: i64) -> Option<u64> {
    if timestamp < 0 {
        return None;
    }
    let millis = if timestamp >= 100_000_000_000 {
        timestamp
    } else {
        timestamp.checked_mul(1_000)?
    };
    u64::try_from(millis).ok()
}

pub(crate) fn mask_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else {
        return "账号".into();
    };
    if local.is_empty() || domain.is_empty() {
        return "账号".into();
    }
    format!(
        "{}***@{}***",
        local.chars().next().unwrap_or('*'),
        domain.chars().next().unwrap_or('*')
    )
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PlanQuota {
    /// 稳定 id，前端拿它决定表情和排序
    pub id: String,
    pub name: String,
    pub ok: bool,
    /// 语言无关的错误 key / 简述；ok=false 时前端原样展示
    pub error: Option<String>,
    /// 头条数字：最紧那个窗口的剩余百分比
    pub remaining_percent: Option<f64>,
    pub buckets: Vec<Bucket>,
    /// 附加一行小字（账号数、备注）
    pub note: Option<String>,
    /// 这个套餐名下的反代端点（有就带，没有 None）
    pub proxy: Option<ProxyStatus>,
    /// 头条数字的口径（人话中文，如 "总额度"、"core 池"、"周额度"），各家头条规则各自填。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headline_label: Option<String>,
    /// 头条那个窗口的重置时间（Unix ms），没有重置信息就 None，不编数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ProxyStatus {
    pub name: String,
    pub url: String,
    pub ok: bool,
    pub latency_ms: Option<u64>,
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_today: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<u64>,
    /// Unix timestamp in milliseconds for the latest upstream HTTP response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_upstream_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_available: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounts_available: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounts_cooling: Option<u64>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub plans: Vec<PlanQuota>,
    pub fetched_at: u64,
}

#[cfg(test)]
mod timestamp_tests {
    use super::{timestamp_to_millis, Bucket, PlanQuota};
    use serde_json::json;

    #[test]
    fn timestamp_formats_normalize_to_milliseconds() {
        assert_eq!(
            timestamp_to_millis(&json!(1_700_000_000)),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            timestamp_to_millis(&json!(1_700_000_000_123_u64)),
            Some(1_700_000_000_123)
        );
        assert_eq!(
            timestamp_to_millis(&json!("1700000000123")),
            Some(1_700_000_000_123)
        );
        assert_eq!(
            timestamp_to_millis(&json!("2023-11-14T22:13:20Z")),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            timestamp_to_millis(&json!("2023-11-14T22:13:20")),
            Some(1_700_000_000_000)
        );
        assert_eq!(timestamp_to_millis(&json!("not-a-date")), None);
        assert_eq!(
            timestamp_to_millis(&json!(900_000_000_000_i64)),
            Some(900_000_000_000)
        );
    }

    #[test]
    fn missing_reset_time_is_omitted_from_bucket_json() {
        let bucket = Bucket {
            label: "daily".into(),
            remaining_percent: Some(50.0),
            detail: None,
            resets_at: None,
            pool: None,
            window: None,
        };
        let json = serde_json::to_value(bucket).unwrap();
        assert_eq!(json["label"], "daily");
        assert!(json.get("resetsAt").is_none());
        assert!(json.get("pool").is_none());
        assert!(json.get("window").is_none());
    }

    /// 新契约字段：pool/window 是 camelCase 且带值时才序列化；
    /// headline_label/resetsAt 同理。前端不读这些字段，但必须钉死
    /// 以免有人改 rename 规则时静默改变之后的契约。
    #[test]
    fn new_contract_fields_serialize_camel_case_and_skip_when_none() {
        let bucket = Bucket {
            label: "Gemini · 周".into(),
            remaining_percent: Some(69.7),
            detail: None,
            resets_at: Some(1_790_208_000_000),
            pool: Some("Gemini".into()),
            window: Some("week".into()),
        };
        let json = serde_json::to_value(bucket).unwrap();
        assert_eq!(json["pool"], "Gemini");
        assert_eq!(json["window"], "week");

        let plan = PlanQuota {
            id: "x".into(),
            name: "x".into(),
            ok: true,
            error: None,
            remaining_percent: Some(61.0),
            buckets: vec![],
            note: None,
            proxy: None,
            headline_label: Some("core 池".into()),
            resets_at: Some(1_700_000_000_000),
        };
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["headlineLabel"], "core 池");
        assert_eq!(json["resetsAt"], 1_700_000_000_000_u64);
        assert!(json.get("headline_label").is_none());

        let plain = PlanQuota {
            headline_label: None,
            resets_at: None,
            ..plan
        };
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("headlineLabel").is_none());
        assert!(json.get("resetsAt").is_none());
    }
}
