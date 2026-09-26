//! 「把桥顶起来」的这一小段：装账号池、预热、起服务。
//!
//! CLI（`src/bin/bridge.rs`）和 Tauri 壳共用这里，免得两边各写一份装配逻辑
//! —— JS 那边这份逻辑在 `src/bridge/run.mjs` 里。

use std::sync::Arc;

use serde_json::Value;

use crate::accounts::{AccountPool, AccountPoolOptions, SessionFactory, SingleAccountPool};
use crate::server::{LogFn, Pool};
use crate::types::{Account, Session, UpstreamError};
use crate::upstream::{self, Upstream, UpstreamOptions};

/// 状态文件（本地禁用名单等）该往哪个目录写：`BRIDGE_STATE_DIR` 优先，否则 `~/.antigravity-bridge`
/// —— 和可选配置文件同一个目录。拿不到 HOME 就 `None`（那就不落盘，行为由 server 如实反映）。
///
/// 放在装配层，是因为「进程往哪写状态」是部署决定，CLI 和桌面壳要同一个口径；
/// 这个变量名只归它自己用，别拿它当配置文件位置的别名（那是 `BRIDGE_CONFIG`）。
pub fn state_dir() -> Option<std::path::PathBuf> {
    match std::env::var_os("BRIDGE_STATE_DIR") {
        Some(dir) => Some(std::path::PathBuf::from(dir)),
        None => Some(crate::oauth::home_dir()?.join(".antigravity-bridge")),
    }
}

/// 账号池里每个账号的会话工厂（JS 里是 `createSession: (account) => new Upstream({...}).init()`）。
///
/// 注意候选凭据（client_id/secret）在 `Upstream::init` 里是**整进程只扫一次**的，
/// 多个账号共用；这里每次只是新建一个会话对象。
/// 传入 `client`（缺省时原地建一个）让工厂产出的所有会话共用同一个 HTTP 连接池。
pub fn session_factory(
    endpoints: Option<Vec<String>>,
    log: LogFn,
    client: Option<reqwest::Client>,
) -> SessionFactory {
    let client = client.unwrap_or_else(upstream::http_client);
    Arc::new(move |account: Account| {
        let endpoints = endpoints.clone();
        let log = Arc::clone(&log);
        let client = client.clone();
        Box::pin(async move {
            let upstream = Upstream::init(UpstreamOptions {
                email: account.email.clone(),
                endpoints,
                log,
                client: Some(client),
            })
            .await
            .map_err(|err| UpstreamError::network("init_failed", err))?;
            Ok(Arc::new(upstream) as Arc<dyn Session>)
        })
    })
}

/// 让人看得懂的一句话：用的是哪个账号 / 池子里有几个。
pub struct PoolSetup {
    pub pool: Pool,
    pub summary: String,
}

/// 额度后台刷新间隔。比 90 秒的 TTL 短一截：缓存刚过期就有人在后面补，选号不会长时间
/// 看着旧数据；又不跟着面板 15 秒的轮询节奏走，免得把上游当自家缓存打。
const QUOTA_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// 起一个低频定时器，定期补查过期的额度。
///
/// 为什么必须有它：`refresh_stale_in_background()` 是挂在 `/healthz` 上的，而 `/healthz`
/// 只有面板开着时才被调到。没人开面板的机器上，选号会一直吃启动那一刻的旧额度 ——
/// 「一个账号额度用光就自动换下一个」也就跟着失效了。
///
/// 只给多账号池起：单账号模式没有池子可换，这个定时器对它没有意义。
fn spawn_quota_refresher(pool: Arc<AccountPool>) {
    tokio::spawn(async move {
        // interval 的第一次 tick 立即返回：一启动就先看一眼有没有过期的，之后每 60 秒一次。
        let mut ticker = tokio::time::interval(QUOTA_REFRESH_INTERVAL);
        loop {
            ticker.tick().await;
            // 内部自带 TTL 节流与「只碰 stale 账号」的判断，空转几乎零成本；出错只记日志。
            pool.refresh_stale_in_background();
        }
    });
}

/// 给了 email 就固定用那一个账号（单账号模式）；否则把库里所有可用账号装成池子
/// （按额度选号 + 熔断 + 粘性）。
pub async fn build_pool(
    email: Option<&str>,
    endpoints: Option<Vec<String>>,
    log: LogFn,
) -> Result<PoolSetup, String> {
    let shared_client = upstream::http_client();
    if let Some(email) = email {
        let upstream = Upstream::init(UpstreamOptions {
            email: Some(email.to_string()),
            endpoints,
            log,
            client: Some(shared_client),
        })
        .await?;
        let masked = upstream
            .identity()
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| email.to_string());
        let pool = Pool::Single(Arc::new(SingleAccountPool::new(Arc::new(upstream))));
        return Ok(PoolSetup {
            pool,
            summary: format!("单账号模式：{masked}"),
        });
    }

    let accounts = upstream::list_accounts().await?;
    let usable: Vec<Account> = accounts
        .into_iter()
        .filter(|a| !a.disabled && !a.validation_blocked && a.refresh_token.is_some())
        .collect();
    if usable.is_empty() {
        return Err("账号库里没有可用账号（都禁用 / 待验证 / 没有 refresh_token）".to_string());
    }
    let masked: Vec<String> = usable.iter().map(|a| a.email_masked.clone()).collect();
    let pool = Arc::new(AccountPool::new(AccountPoolOptions::new(
        session_factory(endpoints, log, Some(shared_client)),
        usable,
    )));
    spawn_quota_refresher(Arc::clone(&pool));
    Ok(PoolSetup {
        pool: Pool::Multi(pool),
        summary: format!(
            "账号池：{} 个可用账号（{}）",
            masked.len(),
            masked.join("、")
        ),
    })
}

/// 按配置建 Qoder 池（`Pool::Single`）。`enabled != true` 就是 `None`（等于没配 Qoder）。
///
/// Qoder 会话只读凭据文件、不联网，凭据坏了也不拦启动 —— 真正的错误在请求/额度那两条路上如实报。
pub fn qoder_pool(cfg: &crate::config::QoderConfig, log: LogFn) -> Option<Pool> {
    if !cfg.enabled {
        return None;
    }
    let log_for_error = Arc::clone(&log);
    match crate::qoder::QoderSession::new(cfg, log) {
        Ok(session) => Some(Pool::Single(Arc::new(SingleAccountPool::new(Arc::new(
            session,
        ))))),
        Err(err) => {
            // 配置型错误不该拦住整座桥：如实说清楚，Qoder 那条路会表现为「未启用」
            (log_for_error)(format!("Qoder 上游启用失败（这次当没配）：{err}"));
            None
        }
    }
}

/// 启动预热：签 token + 拉模型表（可选，失败不拦启动 —— 第一个请求会再试）。
pub async fn warmup(pool: &Pool, log: &LogFn) -> Result<(usize, Option<String>), String> {
    let session = pool
        .any_session(None)
        .await
        .map_err(|err| err.to_string())?;
    if let Err(err) = session.load_code_assist().await {
        (log)(format!("loadCodeAssist 预热失败（不影响启动）：{err}"));
    }
    let models = session.models().await.map_err(|err| err.to_string())?;
    let count = models
        .get("models")
        .and_then(Value::as_object)
        .map(|m| m.len())
        .unwrap_or(0);
    let default = models
        .get("defaultAgentModelId")
        .and_then(Value::as_str)
        .map(str::to_string);
    (log)(format!(
        "预热完成：{count} 个模型可用，默认 {}",
        default.clone().unwrap_or_else(|| "-".to_string())
    ));
    // 额度预取只是为了选号排序，失败了不影响服务；预热的这轮不必绕过 TTL
    pool.refresh_all_in_background(false);
    Ok((count, default))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_factory_builds_with_and_without_shared_client() {
        let log: LogFn = Arc::new(|_| {});
        // 传入 Some(shared_client)
        let client = upstream::http_client();
        let _factory1 = session_factory(None, Arc::clone(&log), Some(client));
        // 传入 None 自动构造
        let _factory2 = session_factory(None, log, None);
    }
}
