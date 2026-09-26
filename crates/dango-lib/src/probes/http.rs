use std::time::Duration;

/// 所有出站请求统一带的 User-Agent：如实报自己的名字和版本。
///
/// 不要伪造成 Cursor / Devin / Factory 的客户端。2026-09-17 实测过：
/// `cursor.com/api/usage-summary` 在伪造 UA、诚实 UA、完全不发 UA 三种情况下都返回 200
/// 且数据完全相同；Devin 的 `GetUserStatus` 也一样，两种请求解析出来的字段和数值
/// 逐一相同（响应体大小差 56 倍，但差的全是本项目不读的部分）。
/// 也就是说伪装没有任何功能收益，只是白白违反对方的服务条款。
///
/// 注意：个别端点会拦默认 UA（Cloudflare 之类）；
/// 那种请求在自己的请求头里显式覆盖这个值，除此之外所有请求都用它。
pub const USER_AGENT: &str = concat!("dango/", env!("CARGO_PKG_VERSION"));

/// 最多尝试次数（首次请求 + 2 次重试）。
const MAX_ATTEMPTS: u32 = 3;

/// 每次重试前的等待时间，指数退避：300ms、900ms。
const BACKOFF_MS: [u64; 2] = [300, 900];

/// 发送 HTTP 请求，对**网络错误、5xx、429** 做有限次重试。
///
/// 4xx（除 429）不重试：401/403 意味着登录态已失效，重试没有意义，
/// 直接把响应交回调用方去生成对应的错误文案。
///
/// 重试完仍失败时：传输层错误返回 `Err(reqwest::Error)`；
/// 可重试的 HTTP 状态码返回最后一次的 `Ok(Response)`，由调用方按状态码报错。
///
/// `RequestBuilder` 是一次性的（`send()` 会消费掉它），因此这里接收一个
/// 每次重新构造请求的闭包，请求本身基于共享的 `reqwest::Client` 构造。
pub(crate) async fn send_with_retry<F>(
    build_request: F,
) -> Result<reqwest::Response, reqwest::Error>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;
        let is_last = attempt >= MAX_ATTEMPTS;

        match build_request().send().await {
            Ok(resp) => {
                if !is_retryable_status(resp.status()) || is_last {
                    return Ok(resp);
                }
            }
            Err(e) => {
                let retryable = e.is_connect() || e.is_timeout() || e.is_request() || e.is_body();
                if !retryable || is_last {
                    return Err(e);
                }
            }
        }

        let delay_ms = BACKOFF_MS[(attempt - 1) as usize];
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
}

/// 哪些 HTTP 状态码值得重试：服务端错误（5xx）和限流（429）。
///
/// 401/403 明确排除：登录态失效时重试只会白等 1.2 秒再把同样的错误报上去，
/// 拖慢「请重新登录」这个真正有用的提示。
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    /// 5xx 与 429 才重试；明确列出边界，防止以后误把 429 从重试名单里删掉。
    #[test]
    fn retries_server_errors_and_rate_limit() {
        for code in [500, 501, 502, 503, 504, 599, 429] {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(is_retryable_status(status), "{code} 应当重试");
        }
    }

    /// 401/403 不重试是有意为之：凭据失效重试没有意义。
    #[test]
    fn does_not_retry_auth_failures() {
        for code in [401u16, 403] {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(!is_retryable_status(status), "{code} 不应重试");
        }
    }

    /// 其余 4xx 与全部 2xx/3xx 都不重试。
    #[test]
    fn does_not_retry_other_client_errors_or_success() {
        for code in [200u16, 201, 204, 301, 302, 400, 404, 409, 422, 451] {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(!is_retryable_status(status), "{code} 不应重试");
        }
    }

    /// 重试次数和退避表要对得上：MAX_ATTEMPTS 是总尝试次数，
    /// 所以退避表长度必须是 MAX_ATTEMPTS - 1，否则循环里会数组越界 panic。
    #[test]
    fn backoff_table_matches_max_attempts() {
        assert_eq!(MAX_ATTEMPTS, 3);
        assert_eq!(BACKOFF_MS.len() as u32, MAX_ATTEMPTS - 1);
        // 指数退避：后一次等待必须比前一次长
        assert!(BACKOFF_MS[1] > BACKOFF_MS[0]);
    }

    /// User-Agent 必须如实带版本号，不能伪造成厂商客户端。
    #[test]
    fn user_agent_reports_own_name() {
        assert!(USER_AGENT.starts_with("dango/"));
        assert!(USER_AGENT.contains(env!("CARGO_PKG_VERSION")));
        for spoofed in ["cursor", "devin", "codeium", "droid", "factory"] {
            assert!(
                !USER_AGENT.to_lowercase().contains(spoofed),
                "UA 不应冒充 {spoofed}: {USER_AGENT}"
            );
        }
    }
}
