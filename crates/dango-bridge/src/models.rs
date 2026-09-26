//! 模型名解析：客户端给的名字 → 上游真实 id。
//!
//! 移植自 `src/bridge/upstream.mjs` 的 `resolveModel` 一组函数（`norm`/`versionOf`/
//! `versionKey`/`tokensOf`/`bestByTokens`/`levelOf`/`betterId`/`familyFallback`）。
//! 纯函数，不联网：给上游模型 id 列表 + 客户端要的名字，返回解析结果。
//! 匹配顺序：原样 → 归一化后相同 → 版本号同代里按词元命中 → 词元命中 → 家族兜底 → 上游默认模型。
//!
//! 行为基准是 JS 版和它的测试（`test/resolve-model.test.mjs`）：这里不做「顺手改好」，
//! 只忠实搬运。上游的模型表是动态的（今天 27 个，明天可能变），所以是启发式 + 如实记账。
//!
//! 公开接口：
//!   pub fn resolve_model(ids: &[String], requested: &str, default_model: Option<&str>) -> ResolvedModel

use crate::types::ResolvedModel;

/// 客户端要的模型名 → 上游的模型 id。
///
/// 调用方把 `models.models` 的 key 列表当 `ids` 传进来，`models.defaultAgentModelId`
/// 当 `default_model`。完全对不上时不硬失败：退回上游默认模型，但把替换事实（`substituted_from`）
/// 报出来。
///
/// `reason` 约定（大小写、拼写都不能改，服务层和面板都按这个读）：
///   - `"exact"`：原样命中，没替换。**这是 JS 版里唯一一个没有 `reason` 字段的分支**
///     （JS 返回 `{model: wanted, substitutedFrom: null}`），Rust 侧补成 `reason = "exact"`、
///     `substituted_from = None`。服务层靠它区分「这是真命中，不是兜底」。
///   - `no_model_list` / `normalized` / `version_match` / `token_match` / `family_fallback` /
///     `default_fallback`：与 JS 一一对应。
pub fn resolve_model(
    ids: &[String],
    requested: &str,
    default_model: Option<&str>,
) -> ResolvedModel {
    let wanted = requested.trim();

    // 拿不到模型表：不硬失败，给个能用的默认。
    if ids.is_empty() {
        return ResolvedModel {
            model: if wanted.is_empty() {
                "gemini-3.6-flash-high".to_string()
            } else {
                wanted.to_string()
            },
            substituted_from: None,
            reason: "no_model_list".to_string(),
        };
    }

    // 原样命中（见上文 reason 约定：JS 这里没有 reason 字段，Rust 补 "exact"）。
    if !wanted.is_empty() && ids.iter().any(|id| id.as_str() == wanted) {
        return ResolvedModel {
            model: wanted.to_string(),
            substituted_from: None,
            reason: "exact".to_string(),
        };
    }

    let normalized = norm(wanted);
    if normalized.len() >= 3 {
        let same_norm = ids.iter().find(|id| norm(id) == normalized);
        if let Some(same) = same_norm {
            return ResolvedModel {
                model: same.clone(),
                substituted_from: Some(wanted.to_string()),
                reason: "normalized".to_string(),
            };
        }

        let tokens = tokens_of(wanted);

        // 版本号是强信号：客户端要 3.8，就先只在 3.8 里挑，别再退回 3.6（那是"看着像，其实降级"）。
        let wanted_version = version_key(wanted);
        if let Some(ver) = wanted_version.as_deref() {
            if !tokens.is_empty() {
                let same_series: Vec<&String> = ids
                    .iter()
                    .filter(|id| version_key(id).as_deref() == Some(ver))
                    .collect();
                if let Some(best) = best_by_tokens(&same_series, &tokens) {
                    return ResolvedModel {
                        model: best,
                        substituted_from: Some(wanted.to_string()),
                        reason: "version_match".to_string(),
                    };
                }
            }
        }

        // 词元命中：请求名里的每段（≥3 字符）在上游 id 里出现过就算命中，命中越多越优先。
        let all: Vec<&String> = ids.iter().collect();
        if let Some(best) = best_by_tokens(&all, &tokens) {
            return ResolvedModel {
                model: best,
                substituted_from: Some(wanted.to_string()),
                reason: "token_match".to_string(),
            };
        }
    }

    // 家族兜底 → 上游默认 → 表里第一个 flash → 表里第一个。
    let family = family_fallback(ids, wanted);
    let (model, reason) = if let Some(fam) = family {
        (fam, "family_fallback")
    } else if let Some(d) = default_model {
        (d.to_string(), "default_fallback")
    } else if let Some(f) = ids.iter().find(|id| id.contains("flash")) {
        (f.clone(), "default_fallback")
    } else {
        (ids[0].clone(), "default_fallback")
    };

    ResolvedModel {
        // JS 是 `wanted || null`：原名是空字符串时给 null，照搬。
        substituted_from: if wanted.is_empty() {
            None
        } else {
            Some(wanted.to_string())
        },
        model,
        reason: reason.to_string(),
    }
}

/// 归一化：只留字母数字（把 `3.6` 里的点也去掉）。
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 版本号更大的算「更新」，用于同分时的取舍：`major * 1000 + minor`（minor 缺省 0）。
fn version_of(id: &str) -> i64 {
    let b = id.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let major: i64 = id[start..i].parse().unwrap_or(0);
            let mut minor: i64 = 0;
            // 只有「点后面还跟着数字」才算小数部分（与 JS 的 `(?:\.(\d+))?` 一致）。
            if i < b.len() && b[i] == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
                let mstart = i + 1;
                let mut j = mstart;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                minor = id[mstart..j].parse().unwrap_or(0);
            }
            return major * 1000 + minor;
        }
        i += 1;
    }
    0
}

/// 版本号原样（`"3.8"` / `"4"` / `"120"`），用来做「同代」判断。第一个 `\d+(\.\d+)?`。
fn version_key(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let mut end = i;
            if i < b.len() && b[i] == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                end = j;
            }
            return Some(s[start..end].to_string());
        }
        i += 1;
    }
    None
}

/// 请求名拆成词元：`[^a-z0-9.]+` 切分、每段 `norm`、只留长度 ≥3 的。
fn tokens_of(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
        .map(norm)
        .filter(|t| t.len() >= 3)
        .collect()
}

/// 在一组候选里按「命中词元长度之和」挑最好的，同分用 `better_id` 取舍。
///
/// 平局时保留先出现的候选（对应 JS 里带比较器的稳定排序），所以只在「严格更好」时才换。
fn best_by_tokens(ids: &[&String], tokens: &[String]) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    let mut best: Option<(String, usize)> = None;
    for id in ids {
        let target = norm(id);
        let mut hits = 0usize;
        for t in tokens {
            if target.contains(t.as_str()) {
                hits += t.len();
            }
        }
        if hits == 0 {
            continue;
        }
        let take = match &best {
            None => true,
            Some((bid, bh)) => hits > *bh || (hits == *bh && better_id(id, bid) > 0),
        };
        if take {
            best = Some((id.as_str().to_string(), hits));
        }
    }
    best.map(|(id, _)| id)
}

/// 挡位：high > medium > low，其余算 0。
fn level_of(id: &str) -> i64 {
    if id.contains("high") {
        3
    } else if id.contains("medium") {
        2
    } else if id.contains("low") {
        1
    } else {
        0
    }
}

/// 同分时怎么挑「更好」的那个：版本新的优先 → 挡位高的优先 → id 短的优先（更接近基名）。
///
/// 返回正数表示 a 更好。挑挡位高的是有意的：客户端没写挡位时给它质量，额度消耗在日志里看得见。
fn better_id(a: &str, b: &str) -> i64 {
    let d = version_of(a) - version_of(b);
    if d != 0 {
        return d;
    }
    let l = level_of(a) - level_of(b);
    if l != 0 {
        return l;
    }
    b.chars().count() as i64 - a.chars().count() as i64
}

type Pred = fn(&str) -> bool;

fn fam_test_anthropic(w: &str) -> bool {
    ["claude", "sonnet", "opus", "haiku"]
        .iter()
        .any(|k| w.contains(*k))
}
fn pick_anthropic(id: &str) -> bool {
    id.contains("claude")
}
fn fam_test_gpt(w: &str) -> bool {
    w.contains("gpt") || w.contains("oss")
}
fn pick_gpt(id: &str) -> bool {
    id.contains("gpt") || id.contains("oss")
}
fn fam_test_flash(w: &str) -> bool {
    w.contains("flash")
}
fn pick_flash(id: &str) -> bool {
    id.contains("flash")
}
fn fam_test_pro(w: &str) -> bool {
    w.contains("pro")
}
fn pick_pro(id: &str) -> bool {
    id.contains("pro")
}

/// 家族兜底：claude / gpt / flash / pro 这几个词决定方向，同家族里挑版本最新、挡位最高的。
///
/// 候选要排除 `/image|tab_|agent|chat_/`（这些是子工具/图片模型，不是对话模型）。
fn family_fallback(ids: &[String], wanted: &str) -> Option<String> {
    let w = wanted.to_lowercase();
    let families: [(Pred, Pred); 4] = [
        (fam_test_anthropic, pick_anthropic),
        (fam_test_gpt, pick_gpt),
        (fam_test_flash, pick_flash),
        (fam_test_pro, pick_pro),
    ];
    for (test, pick) in families {
        if !test(&w) {
            continue;
        }
        let mut best: Option<&String> = None;
        for id in ids {
            if !pick(&id.to_lowercase()) {
                continue;
            }
            if id.contains("image")
                || id.contains("tab_")
                || id.contains("agent")
                || id.contains("chat_")
            {
                continue;
            }
            let take = match best {
                None => true,
                Some(b) => better_id(id.as_str(), b.as_str()) > 0,
            };
            if take {
                best = Some(id);
            }
        }
        if let Some(b) = best {
            return Some(b.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 抄一份今天（2026-09-17）真实的模型表 —— 与 `test/resolve-model.test.mjs` 的 `IDS` 完全一致。
    const IDS: [&str; 27] = [
        "gemini-3.5-flash-low",
        "gemini-3-flash",
        "gemini-3.1-flash-lite",
        "gpt-oss-120b-medium",
        "gemini-2.5-flash-thinking",
        "gemini-3.8-flash-tiered",
        "gemini-3.1-pro-low",
        "gemini-3.6-flash-medium",
        "gemini-3.1-pro-high",
        "claude-sonnet-4-6",
        "gemini-3.1-flash-image",
        "claude-opus-4-6-thinking",
        "gemini-2.5-flash",
        "gemini-3.6-flash-low",
        "gemini-pro-agent",
        "gemini-3-flash-agent",
        "tab_jump_flash_lite_preview",
        "tab_flash_lite_preview",
        "gemini-3.5-flash-extra-low",
        "gemini-2.5-flash-lite",
        "gemini-3.5-flash-lite",
        "gemini-2.5-pro",
        "chat_23310",
        "gemini-3.6-flash-high",
        "gemini-3.7-flash-tiered",
        "chat_20706",
        "gemini-3.6-flash-tiered",
    ];

    const DEFAULT: &str = "gemini-3.6-flash-high";

    fn ids() -> Vec<String> {
        IDS.iter().map(|s| s.to_string()).collect()
    }

    /// JS 测试里的 `offline()`：模型表 + 默认 model，不联网。
    fn resolve(requested: &str) -> ResolvedModel {
        resolve_model(&ids(), requested, Some(DEFAULT))
    }

    #[test]
    fn upstream_id_passes_through_untouched() {
        // JS: "上游有的 id 原样透传"
        let r = resolve("gemini-3.6-flash-high");
        assert_eq!(r.model, "gemini-3.6-flash-high");
        assert_eq!(r.substituted_from, None);
        // JS 这条没有 reason 字段；Rust 补成 "exact"（服务层靠它判断「这是真命中，不是兜底」）。
        assert_eq!(r.reason, "exact");
    }

    #[test]
    fn case_and_separator_differences_use_normalized_match() {
        // JS: "只差大小写/分隔符时按归一化匹配"
        let r = resolve("gemini_3.6 flash high");
        assert_eq!(r.model, "gemini-3.6-flash-high");
        assert_eq!(r.reason, "normalized");
    }

    #[test]
    fn single_letter_does_not_participate_in_fuzzy_matching() {
        // JS: "单字母不参与模糊匹配（不然 extra 里的 x 会中招）"
        let r = resolve("x");
        assert_eq!(r.model, DEFAULT);
        assert_eq!(r.reason, "default_fallback");
    }

    #[test]
    fn dated_anthropic_name_lands_on_upstream_claude() {
        // JS: "带日期的 Anthropic 名字落到上游的 claude"
        let r = resolve("claude-sonnet-4-5-20250929");
        assert_eq!(r.model, "claude-sonnet-4-6");
        assert_eq!(r.reason, "version_match");
    }

    #[test]
    fn gpt_family_lands_on_gpt_oss() {
        // JS: "gpt 家族落到 gpt-oss"
        let r = resolve("gpt-4o");
        assert_eq!(r.model, "gpt-oss-120b-medium");
        assert_eq!(r.substituted_from.as_deref(), Some("gpt-4o"));
        assert!(r.reason == "token_match" || r.reason == "family_fallback");
    }

    #[test]
    fn flash_family_picks_newer_version_and_higher_level() {
        // JS: "flash 家族：同家族里挑版本更新、挡位更高的"
        let r = resolve("gemini-3.8-flash");
        assert_eq!(r.model, "gemini-3.8-flash-tiered");
    }

    #[test]
    fn pro_family_never_picks_flash() {
        // JS: "pro 家族不会挑到 flash"
        let r = resolve("gemini-3.1-pro");
        assert_eq!(r.model, "gemini-3.1-pro-high");
    }

    #[test]
    fn version_pin_does_not_downgrade_across_generations() {
        // JS: "带版本号时不跨代降级：要 3.8 就只在 3.8 里挑"
        let r = resolve("gemini-3.8-flash-high");
        assert_eq!(r.model, "gemini-3.8-flash-tiered");
        assert_eq!(r.reason, "version_match");
    }

    #[test]
    fn unmatched_version_falls_back_by_tokens_including_level_word() {
        // JS: "版本对不上时不会硬套同代，按词元（含挡位词）继续退让"
        let r = resolve("gemini-3.9-flash-high");
        assert_eq!(r.model, "gemini-3.6-flash-high");
        assert_eq!(r.reason, "token_match");
        assert_eq!(r.substituted_from.as_deref(), Some("gemini-3.9-flash-high"));
    }

    #[test]
    fn unknown_name_falls_back_to_default() {
        // JS: "完全不认识的名字退回默认模型，并说清楚是兜底"
        let r = resolve("some-internal-model-name");
        assert_eq!(r.model, DEFAULT);
        assert_eq!(r.reason, "default_fallback");
    }

    #[test]
    fn missing_model_name_uses_default_without_calling_it_a_substitution() {
        // JS: "没给模型名时用默认模型（不算替换）"（JS 传 undefined）
        let r = resolve("");
        assert_eq!(r.model, DEFAULT);
        assert_eq!(r.substituted_from, None);
        assert_eq!(r.reason, "default_fallback");
    }

    #[test]
    fn no_model_list_does_not_hard_fail() {
        // JS: "拿不到模型表时不硬失败"
        let r = resolve_model(&[], "whatever", Some(DEFAULT));
        assert_eq!(r.model, "whatever");
        assert_eq!(r.reason, "no_model_list");
    }

    // ---- 下面几条是 JS 测试没断言到的边界，补上。 ----

    #[test]
    fn empty_request_with_no_model_list_uses_builtin_default() {
        // JS 空表分支里的 `wanted || "gemini-3.6-flash-high"`
        let r = resolve_model(&[], "", Some(DEFAULT));
        assert_eq!(r.model, "gemini-3.6-flash-high");
        assert_eq!(r.substituted_from, None);
        assert_eq!(r.reason, "no_model_list");
    }

    #[test]
    fn whitespace_only_request_is_treated_as_missing() {
        // JS 的 `String(requested ?? "").trim()`
        let r = resolve("   ");
        assert_eq!(r.model, DEFAULT);
        assert_eq!(r.substituted_from, None);
        assert_eq!(r.reason, "default_fallback");
    }

    #[test]
    fn family_fallback_is_reported_when_no_token_matches() {
        // "flashy" 走不到词元命中（没有 id 含 flashy），但家族测试 /flash/ 命中 → family_fallback。
        let r = resolve("flashy");
        assert_eq!(r.model, "gemini-3.8-flash-tiered");
        assert_eq!(r.reason, "family_fallback");
        assert_eq!(r.substituted_from.as_deref(), Some("flashy"));
    }

    #[test]
    fn better_id_breaks_token_ties_by_level() {
        // 同代 3.6 的四个 flash 变体词元命中完全相同，靠 betterId 的挡位高低决出 high。
        let r = resolve("gemini-3.6-flash");
        assert_eq!(r.model, "gemini-3.6-flash-high");
        assert_eq!(r.reason, "version_match");
    }

    #[test]
    fn no_default_model_falls_back_to_a_flash_entry() {
        // default_model 为 None 时退到表里第一个含 flash 的 id（再退到 ids[0]）。
        let r = resolve_model(&ids(), "totally-unknown-zzz", None);
        assert_eq!(r.model, "gemini-3.5-flash-low");
        assert_eq!(r.reason, "default_fallback");
    }

    #[test]
    fn version_helpers_match_js_regex_behaviour() {
        assert_eq!(version_of("gemini-3.8-flash"), 3008);
        assert_eq!(version_of("gemini-3-flash"), 3000);
        assert_eq!(version_of("gpt-oss-120b-medium"), 120_000);
        assert_eq!(version_of("no-digits-here"), 0);
        // "点后面必须跟数字" 才算小数
        assert_eq!(version_of("v2.abc"), 2000);
        assert_eq!(version_key("gemini-3.1-pro-high").as_deref(), Some("3.1"));
        assert_eq!(version_key("gpt-oss-120b-medium").as_deref(), Some("120"));
        assert_eq!(version_key("tab_flash_lite_preview"), None);
    }

    #[test]
    fn norm_and_tokens_match_js_behaviour() {
        assert_eq!(norm("Gemini_3.6 Flash"), "gemini36flash");
        // 只留长度 ≥3 的词元；"3.6" 归一化成 "36"（长度 2）被丢掉
        assert_eq!(tokens_of("gemini-3.6-flash"), vec!["gemini", "flash"]);
        assert_eq!(tokens_of("a-b-c"), Vec::<String>::new());
    }
}
