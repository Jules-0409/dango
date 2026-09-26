//! Antigravity 上游桥的核心库：`src/`（JS 版）的逐模块移植。
//!
//! 移植规矩（重要，写给后面接着改的人）：
//!   1. **行为以 JS 版和它的测试为准**。JS 版是行为基准（也是日常在跑的驱动），
//!      Rust 版先做到「同一发请求、同一批测试、同样的输出」，再谈优化。
//!   2. 能用 `serde_json::Value` 的地方就用它，别急着造类型树 —— 瓶颈在网络 I/O，
//!      结构对不齐反而容易和 JS 版跑偏。
//!   3. 注释写「为什么」。JS 版里那些看起来奇怪的判断（429 只记到模型、粘性额度让位、
//!      签名哨兵）都配了原因，搬过来时把原因一起搬。
//!   4. 每个模块的单测跟模块放在一起；端到端用假上游跑真 HTTP，放在 `tests/`。

pub mod accounts;
pub mod anthropic_request;
pub mod anthropic_stream;
pub mod chat;
pub mod config;
pub mod leak_repair;
pub mod logfile;
pub mod login;
pub mod models;
pub mod oauth;
pub mod openai;
pub mod panel;
pub mod qoder;
pub mod quota;
pub mod runtime;
pub mod sanitize;
pub mod server;
pub mod signatures;
pub mod types;
pub mod upstream;
pub mod v1internal;

pub use types::{
    iso_from_millis, now_millis, Account, GenerateRequest, QuotaRow, QuotaSnapshot, ResolvedModel,
    Session, StreamEvent, UpstreamError,
};

// 配额那几个类型定义在 quota 里，别的地方常一起用，顺手从这里转出去
pub use quota::{ModelInfo, ModelLimit, QuotaBucket, QuotaGroup};
