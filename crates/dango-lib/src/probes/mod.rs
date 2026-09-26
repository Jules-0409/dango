// vendor 自 quota-panel（Apache-2.0，作者同一人）：Cursor/Devin/Factory/Grok Bot
// 的逆向探针。只搬了取数侧——credentials 只读、http 重试、models 契约；
// Tauri 壳、托盘、config 落盘都留在原仓，dango 不依赖它的二进制。
pub mod credentials;
pub mod cursor;
pub mod devin;
pub mod factory;
pub mod http;
pub mod models;

pub use cursor::query_cursor_quota;
pub use cursor::CursorQuota;
pub use cursor::{fetch_usage_events as fetch_cursor_usage_events, CursorUsageEvent};
pub use devin::fetch_devin;
pub use factory::fetch_factory;
pub use models::{DevinQuota, FactoryQuota};

/// 三路并发查一遍，等价于原仓 `query_for_cli()` 的返回值。
pub async fn query_all(client: &reqwest::Client) -> (FactoryQuota, DevinQuota, CursorQuota) {
    let (f, d, c) = tokio::join!(
        factory::fetch_factory(client),
        devin::fetch_devin(client),
        cursor::query_cursor_quota(client),
    );
    (f, d, c)
}
