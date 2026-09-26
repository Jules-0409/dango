//! 额度：把上游两个接口的返回归一化成一份「人/看板都能直接吃」的形状。
//!
//! 事实（探针 07 实测，2026-09-18，两个账号都验过）：
//!   retrieveUserQuotaSummary → { groups: [{ displayName, description, buckets: [
//!        { bucketId, displayName, window, resetTime, remainingFraction, description }]}]}
//!   分组只有两种：Gemini Models（gemini-weekly / gemini-5h）与 Claude and GPT models（3p-weekly / 3p-5h）。
//!   当组内模型共享同一条窗口额度，所以模型级的 quotaInfo.remainingFraction 就是它所在组的值。
//!   fetchAvailableModels → { models: { <id>: { displayName, quotaInfo:{remainingFraction,resetTime},
//!        maxTokens, maxOutputTokens, supportsThinking, thinkingBudget, minThinkingBudget, ... } } }
//!
//! 这个模块只做纯转换：不联网、不缓存。

use serde::Serialize;
use serde_json::Value;

use crate::types::QuotaRow;

/// 一条窗口额度（上游的 bucket）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaBucket {
    pub id: Option<String>,
    pub window: Option<String>,
    pub label: Option<String>,
    pub remaining_fraction: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub reset_time: Option<String>,
    pub description: Option<String>,
}

/// 一个分组（Gemini Models / Claude and GPT models）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaGroup {
    pub name: String,
    pub description: Option<String>,
    pub buckets: Vec<QuotaBucket>,
}

/// 模型表里一个模型（只留服务层真正用得上的字段）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: Option<String>,
    pub remaining_fraction: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub reset_time: Option<String>,
    pub supports_thinking: bool,
    pub thinking_budget: Option<f64>,
    pub min_thinking_budget: Option<f64>,
    pub max_tokens: Option<f64>,
    pub max_output_tokens: Option<f64>,
}

/// 某个模型在上游模型表里的硬上限与思考参数。
/// 服务层用它来 clamp 客户端给的预算 —— 客户端写 100 万，上游模型只吃 6 万，
/// 与其让上游截断/拒绝，不如我们按事实收一下，并在 warnings 里说明收过。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelLimit {
    pub max_output_tokens: Option<i64>,
    pub min_thinking_budget: Option<i64>,
    pub default_thinking_budget: Option<i64>,
    pub supports_thinking: bool,
}

/// 分组归一化：同一个 bucket 的字段上游是 camelCase，桥落盘的账号文件是 snake_case，两种都认。
pub fn normalize_groups(summary: &Value) -> Vec<QuotaGroup> {
    let groups = summary
        .get("groups")
        .or_else(|| summary.get("quotaGroups"))
        .or_else(|| summary.get("quota_groups"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    groups
        .iter()
        .map(|g| {
            let buckets = g
                .get("buckets")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|b| {
                    let fraction = pick(b, &["remainingFraction", "remaining_fraction"]);
                    QuotaBucket {
                        id: string_of(pick(b, &["bucketId", "bucket_id"])),
                        window: string_of(pick(b, &["window"])),
                        label: string_of(pick(b, &["displayName", "display_name"])),
                        remaining_fraction: num_or_null(fraction),
                        remaining_percent: percent(fraction),
                        reset_time: string_of(pick(b, &["resetTime", "reset_time"])),
                        description: string_of(pick(b, &["description"])),
                    }
                })
                .collect();
            QuotaGroup {
                name: string_of(pick(g, &["displayName", "display_name"]))
                    .unwrap_or_else(|| "未命名分组".to_string()),
                description: string_of(pick(g, &["description"])),
                buckets,
            }
        })
        .collect()
}

/// 模型归一化：只留服务层真正用得上的字段（含上游告诉我们的思考与输出上限）。
pub fn normalize_models(models: &Value) -> Vec<ModelInfo> {
    let Some(map) = models.get("models").and_then(Value::as_object) else {
        return Vec::new();
    };
    map.iter()
        .map(|(id, m)| {
            let quota = m.get("quotaInfo");
            let fraction = quota.and_then(|q| q.get("remainingFraction"));
            ModelInfo {
                id: id.clone(),
                name: string_of(m.get("displayName")).unwrap_or_else(|| id.clone()),
                provider: string_of(pick(m, &["modelProvider", "apiProvider"])),
                remaining_fraction: num_or_null(fraction),
                remaining_percent: percent(fraction),
                reset_time: string_of(quota.and_then(|q| q.get("resetTime"))),
                supports_thinking: truthy(m.get("supportsThinking")),
                thinking_budget: num_or_null(m.get("thinkingBudget")),
                min_thinking_budget: num_or_null(m.get("minThinkingBudget")),
                max_tokens: num_or_null(m.get("maxTokens")),
                max_output_tokens: num_or_null(m.get("maxOutputTokens")),
            }
        })
        .collect()
}

/// 一跳摘要：给面板与选号用的「还剩多少、什么时候回满」。
/// 只收 weekly / 5h 两条窗口 —— 上游目前只发这两种，别的名字是它自己加的，不参与判断。
pub fn summarize(groups: &[QuotaGroup]) -> Vec<QuotaRow> {
    let mut out = Vec::new();
    for g in groups {
        for b in &g.buckets {
            if b.window.as_deref() == Some("weekly") || b.window.as_deref() == Some("5h") {
                out.push(QuotaRow {
                    group: g.name.clone(),
                    window: b.window.clone().unwrap_or_default(),
                    remaining_percent: b.remaining_percent,
                    reset_time: b.reset_time.clone(),
                });
            }
        }
    }
    out
}

/// 某个模型的硬上限；表里没有这个模型就返回 None（调用方退回硬编码上限）。
pub fn model_limits(models: &Value, id: &str) -> Option<ModelLimit> {
    let m = models.get("models")?.get(id)?;
    let positive = |v: Option<&Value>| -> Option<i64> {
        let n = num_or_null(v)?;
        if n > 0.0 {
            Some(n as i64)
        } else {
            None
        }
    };
    Some(ModelLimit {
        max_output_tokens: positive(m.get("maxOutputTokens")),
        min_thinking_budget: positive(m.get("minThinkingBudget")),
        default_thinking_budget: positive(m.get("thinkingBudget")),
        supports_thinking: truthy(m.get("supportsThinking")),
    })
}

fn pick<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(*k))
}

fn string_of(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Bool(true)))
}

/// JS 里 `Number(v)` + `Number.isFinite` 的忠实翻版，连怪癖一起搬：
///   - 字段**缺失**（undefined）→ NaN → None
///   - 字段是 **null** → `Number(null) === 0` → `Some(0.0)`
///   - 空字符串 → 0；能解析的数字字符串 → 该数字；true/false → 1/0
///   - 数组/对象 → NaN → None
///
/// 这个区别有实际后果：上游如果显式给 `remainingFraction: null`，JS 版会算成 0%（额度耗尽），
/// 缺失才会算成「未知」。Rust 版必须一模一样，否则选号排序会和 JS 版分叉。
pub(crate) fn num_or_null(v: Option<&Value>) -> Option<f64> {
    match v {
        None => None,
        Some(Value::Null) => Some(0.0),
        Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok().filter(|n| n.is_finite())
            }
        }
        Some(Value::Array(_) | Value::Object(_)) => None,
    }
}

/// 比例 → 百分比（一位小数）。JS 是 `Math.round(n * 1000) / 10`：
/// 对非负数来说 Math.round 和 f64::round 一致（都是四舍五入到最近整数）。
fn percent(v: Option<&Value>) -> Option<f64> {
    let n = num_or_null(v)?;
    Some((n * 1000.0).round() / 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_groups_accepts_both_namings() {
        let camel = json!({"groups":[{"displayName":"Gemini Models","buckets":[
            {"bucketId":"gemini-weekly","window":"weekly","remainingFraction":0.83,"resetTime":"2026-09-24T00:00:00Z"}]}]});
        let g = normalize_groups(&camel);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].name, "Gemini Models");
        assert_eq!(g[0].buckets[0].id.as_deref(), Some("gemini-weekly"));
        assert_eq!(g[0].buckets[0].remaining_percent, Some(83.0));

        // 老桥落盘的账号文件是 snake_case
        let snake = json!({"quota_groups":[{"display_name":"3p","buckets":[{"bucket_id":"3p-5h","window":"5h","remaining_fraction":0.999}]}]});
        let g = normalize_groups(&snake);
        assert_eq!(g[0].name, "3p");
        assert_eq!(g[0].buckets[0].remaining_percent_or_default(), 99.9);
    }

    #[test]
    fn normalize_groups_tolerates_garbage() {
        assert!(normalize_groups(&json!({})).is_empty());
        assert!(normalize_groups(&json!(null)).is_empty());
        let g = normalize_groups(&json!({"groups":[{}]}));
        assert_eq!(g[0].name, "未命名分组");
        assert!(g[0].buckets.is_empty());
    }

    #[test]
    fn missing_and_null_fraction_differ_like_js() {
        // 缺失 → 未知（None）；显式 null → JS 的 Number(null) === 0 → 0%
        let g = normalize_groups(&json!({"groups":[{"displayName":"x","buckets":[
            {"window":"weekly"},{"window":"5h","remainingFraction":null}]}]}));
        assert_eq!(g[0].buckets[0].remaining_percent, None);
        assert_eq!(g[0].buckets[1].remaining_percent, Some(0.0));
    }

    #[test]
    fn summarize_keeps_only_weekly_and_5h() {
        let groups = normalize_groups(&json!({"groups":[{"displayName":"Gemini Models","buckets":[
            {"window":"weekly","remainingFraction":0.83},{"window":"5h","remainingFraction":0.989},{"window":"daily","remainingFraction":0.5}]}]}));
        let rows = summarize(&groups);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].window, "weekly");
        assert_eq!(rows[0].remaining_percent, Some(83.0));
        assert_eq!(rows[1].window, "5h");
    }

    #[test]
    fn normalize_models_keeps_limits_and_thinking() {
        let models = json!({"models":{"gemini-3.6-flash-high":{
            "displayName":"Gemini 3.6 Flash (High)","modelProvider":"gemini","quotaInfo":{"remainingFraction":0.9,"resetTime":"x"},
            "supportsThinking":true,"thinkingBudget":10000,"minThinkingBudget":512,"maxTokens":65536,"maxOutputTokens":64000}}});
        let list = normalize_models(&models);
        assert_eq!(list.len(), 1);
        let m = &list[0];
        assert_eq!(m.id, "gemini-3.6-flash-high");
        assert_eq!(m.name, "Gemini 3.6 Flash (High)");
        assert_eq!(m.remaining_percent, Some(90.0));
        assert!(m.supports_thinking);
        assert_eq!(m.max_output_tokens, Some(64000.0));
        assert_eq!(m.min_thinking_budget, Some(512.0));
        assert_eq!(
            model_limits(&models, "gemini-3.6-flash-high")
                .unwrap()
                .max_output_tokens,
            Some(64000)
        );
        assert!(model_limits(&models, "不存在").is_none());
    }

    #[test]
    fn model_limits_ignores_non_positive_values() {
        let models = json!({"models":{"m":{"maxOutputTokens":0,"minThinkingBudget":-1,"supportsThinking":false}}});
        let l = model_limits(&models, "m").unwrap();
        assert_eq!(l.max_output_tokens, None);
        assert_eq!(l.min_thinking_budget, None);
        assert!(!l.supports_thinking);
    }

    /// 测试里方便断言百分比的小工具
    impl QuotaBucket {
        fn remaining_percent_or_default(&self) -> f64 {
            self.remaining_percent.unwrap_or(f64::NAN)
        }
    }
}
