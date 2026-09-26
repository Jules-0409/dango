//! 把 Cursor CLI（`cursor-agent`）包装成 Anthropic / OpenAI 兼容的反代。
//!
//! 和 antigravity-bridge 的关系：那边是自己签 token 打上游私有接口，这边是直接驱动
//! 官方 CLI 拿它的 stream-json 输出。两边的对外协议层（Anthropic /v1/messages、
//! OpenAI /v1/chat/completions、SSE 帧、思考块签名）保持同一套习惯，客户端可以互相换。
//!
//! 设计边界（v1）：
//! - 认证：默认**用 CLI 当前登录态**，一个字都不动钥匙串。要跑日抛号就把每个号的 API key
//!   写进 `~/.cursor-bridge/accounts.json`，按请求轮换传 `--api-key`（实测这条认证路径存在
//!   且会校验 key）。为什么不用「每账号一个 `CURSOR_CONFIG_DIR`」：macOS 上凭据在登录钥匙串
//!   的固定条目里，配置目录隔离不了它，第二个号登录会直接覆盖掉用户自己的登录态。
//! - 工具：v1 不做客户端工具透传（那需要 ACP + MCP 那套），CLI 自己的工具在 `--mode ask`
//!   下只读。也就是说这是个「能带思考的聊天反代」，不是能替你改代码的 agent 反代。

pub mod accounts;
pub mod cli;
pub mod config;
pub mod error;
pub mod models;
pub mod native;
pub mod protocol;
pub mod server;
pub mod service;
pub mod turn;
pub mod types;

pub use error::{Error, Result};
