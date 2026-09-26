use std::path::PathBuf;

/// 桥自己的错误类型。对外只暴露「哪一步坏了」，不泄露凭据。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "找不到 Cursor CLI 可执行文件（找过 {tried:?}）；装好后或用 CURSOR_BRIDGE_AGENT_BIN 指定"
    )]
    AgentNotFound { tried: Vec<PathBuf> },

    #[error("拉起 Cursor CLI 失败：{0}")]
    Spawn(#[source] std::io::Error),

    #[error("读 Cursor CLI 输出失败：{0}")]
    Io(#[source] std::io::Error),

    #[error("Cursor CLI 没有产出 stream-json 事件就退出了（退出码 {code:?}）：{stderr}")]
    EmptyTurn { code: Option<i32>, stderr: String },

    #[error("Cursor CLI 报错：{message}")]
    Upstream { message: String },

    /// 原生后端（不用 CLI 那条路）自己的失败：换 token、连 agent 服务、读流。
    #[error("原生后端：{message}")]
    Native { message: String },

    /// `service` 子命令（launchd）的失败。
    #[error("{message}")]
    Service { message: String },

    #[error("回合超过 {secs} 秒还没结束，已经掐掉")]
    Timeout { secs: u64 },

    #[error("请求里没有可用的对话内容")]
    EmptyPrompt,

    #[error("JSON 解析失败：{0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
