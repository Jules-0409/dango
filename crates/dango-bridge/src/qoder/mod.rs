//! Qoder CN 上游：把 `qoder/` 前缀的请求引到 `gateway.qoder.com.cn`。
//!
//! 协议已经用真实网关跑通并验证（免费模型 `qfmodel` 回 `billable:false`、额度 1347→1347
//! 无变化）。实现照抄本地 PoC `/tmp/qoder-poc/qoder-client.mjs`，并与 MIT 参考项目
//! `simonsmh/pi-provider-qoder` 互证。
//!
//! 结构：
//!   - [`encoding`]：私有 base64（签名覆盖编码后的字节）
//!   - [`cosy`]：COSY 签名头
//!   - [`auth`]：`~/.qoder-bridge/auth.json` 的读取 / 换 job token / 强刷
//!   - [`body`]：Gemini inner → Qoder 请求体
//!   - [`stream`]：SSE 信封解析 + 上游 chunk → Gemini 形状
//!   - [`session`]：[`QoderSession`] 实现 `Session`，外面包 `Pool::Single`

pub mod auth;
pub mod body;
pub mod cosy;
pub mod encoding;
pub mod session;
pub mod stream;
pub mod upload;

pub use session::QoderSession;

/// 路由判据用的模型 id 前缀：带它的请求走 Qoder。
pub const MODEL_PREFIX: &str = "qoder/";
/// 网关（推理 / 模型清单）
pub const DEFAULT_GATEWAY: &str = "https://gateway.qoder.com.cn";
/// openapi（换 job token / userinfo / 额度）
pub const DEFAULT_OPENAPI: &str = "https://openapi.qoder.com.cn";
/// 换 job token 时带的客户端 id
pub const CLIENT_ID: &str = "732aef47-9cf2-46a2-95fe-4cebb5d0d1fa";
/// 网关侧 COSY 版本（旧值会让 model/list 返回缩减列表）
pub const COSY_VERSION: &str = "1.1.38";
/// openapi 侧 COSY 版本
pub const OPENAPI_COSY_VERSION: &str = "1.0.1";
pub const CLIENT_TYPE: &str = "5";

/// 请求该走哪个上游。判据就是模型 id 前缀（前缀常量在 [`MODEL_PREFIX`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamKind {
    Antigravity,
    Qoder,
}

pub fn is_qoder_model(model: &str) -> bool {
    model.starts_with(MODEL_PREFIX)
}

/// 去掉 `qoder/` 前缀（没有前缀就原样返回）。
pub fn strip_prefix(model: &str) -> &str {
    model.strip_prefix(MODEL_PREFIX).unwrap_or(model)
}

/// 加上 `qoder/` 前缀（用来把上游 key 变回对外模型 id）。
pub fn add_prefix(model: &str) -> String {
    format!("{MODEL_PREFIX}{model}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_round_trips() {
        assert!(is_qoder_model("qoder/auto"));
        assert!(is_qoder_model("qoder/qfmodel"));
        assert!(!is_qoder_model("gemini-3.6-flash-high"));
        assert_eq!(strip_prefix("qoder/auto"), "auto");
        assert_eq!(strip_prefix("auto"), "auto");
        assert_eq!(add_prefix("auto"), "qoder/auto");
        assert_eq!(strip_prefix(&add_prefix("qfmodel")), "qfmodel");
    }
}
