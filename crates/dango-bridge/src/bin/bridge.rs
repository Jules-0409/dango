//! CLI 入口：和 JS 版 `node src/bridge/run.mjs` 同一套参数、同一份启动输出。
//!
//! ```text
//! bridge                          # 起在 127.0.0.1:8050，启动时预热
//! bridge --port=8051 --no-log     # 换端口、不落盘日志（差分测试用）
//! bridge --email=x@y.com          # 只用这一个账号（单账号模式）
//! bridge --smoke                  # 起来之后自己打一发真请求（会真的调用上游）
//! ```

use std::sync::Arc;
use std::time::Duration;

use dango_bridge::config::{self, FileConfig};
use dango_bridge::runtime;
use dango_bridge::server::{Bridge, BridgeOptions, LogFn};
use dango_bridge::signatures::shared_signatures;
use dango_bridge::types::now_millis;
use dango_bridge::v1internal::ENDPOINTS;
use serde_json::{json, Value};

/// CLI 默认版本号：跟随 crate 版本（Tauri 壳传它自己的版本）。
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 内建默认值（README 和 --help 里写的也是这两个数，别散落）。
const DEFAULT_PORT: u16 = 8050;
const DEFAULT_HOST: &str = "127.0.0.1";

struct Args {
    port: u16,
    host: String,
    email: Option<String>,
    api_key: Option<String>,
    warmup: bool,
    smoke: bool,
    log_dir: Option<String>,
    endpoints: Option<Vec<String>>,
    /// 请求日志里记不记正文（默认记；`--no-log-bodies` 或配置里 `log_bodies: false` 关掉）
    log_bodies: bool,
    /// 正文截断字符上限，0 表示不截断全部保留
    body_limit: usize,
}

/// 命令行里**显式**给出的值：没给的留 `None`，好让配置文件补上（优先级见 `resolve`）。
/// 那几个开关（--no-log / --no-warmup / --smoke / --help）配置文件里没有，单独放。
#[derive(Default)]
struct Cli {
    port: Option<u16>,
    host: Option<String>,
    email: Option<String>,
    api_key: Option<String>,
    log_dir: Option<String>,
    endpoints: Option<Vec<String>>,
    body_limit: Option<usize>,
    no_log: bool,
    no_warmup: bool,
    /// `--no-log-bodies`：明确要求「请求日志里别记正文」
    no_log_bodies: bool,
    smoke: bool,
    help: bool,
}

/// 默认日志目录 `<可执行文件所在仓库>/logs`；拿不到就关掉落盘日志（不猜）。
fn default_log_dir() -> String {
    // 服务场景（launchd / systemd）下 CWD 常常是只读的 `/`，往那儿写日志必然失败。
    // 默认跟状态文件走同一个目录；BRIDGE_STATE_DIR 也会顺带生效。
    let mut dir = runtime::state_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.push("logs");
    dir.to_string_lossy().to_string()
}

fn parse_cli(argv: Vec<String>) -> Result<Cli, String> {
    let mut cli = Cli::default();
    for item in argv {
        if item == "--no-warmup" {
            cli.no_warmup = true;
        } else if item == "--smoke" {
            cli.smoke = true;
        } else if item == "--no-log" {
            cli.no_log = true;
            // --no-log 也要压过配置文件里的 log_dir
            cli.log_dir = None;
        } else if item == "--no-log-bodies" {
            cli.no_log_bodies = true;
        } else if item == "--help" || item == "-h" {
            cli.help = true;
        } else if let Some(value) = item.strip_prefix("--port=") {
            cli.port = Some(
                value
                    .parse()
                    .map_err(|_| format!("端口不是数字：{value}"))?,
            );
        } else if let Some(value) = item.strip_prefix("--host=") {
            cli.host = Some(value.to_string());
        } else if let Some(value) = item.strip_prefix("--email=") {
            cli.email = Some(value.to_string());
        } else if let Some(value) = item.strip_prefix("--api-key=") {
            cli.api_key = Some(value.to_string());
        } else if let Some(value) = item.strip_prefix("--log-dir=") {
            cli.log_dir = Some(value.to_string());
            // 后给的 --log-dir 压过前面的 --no-log（保持原来的「后者优先」）
            cli.no_log = false;
        } else if let Some(value) = item.strip_prefix("--body-limit=") {
            cli.body_limit = Some(
                value
                    .parse()
                    .map_err(|_| format!("body-limit 不是数字：{value}"))?,
            );
        } else if let Some(value) = item.strip_prefix("--endpoint=") {
            cli.endpoints = Some(vec![value.to_string()]);
        } else {
            return Err(format!("不认识的参数：{item}"));
        }
    }
    Ok(cli)
}

/// 三处值按优先级合起来：命令行显式值 > 配置文件 > 内建默认。
fn resolve(cli: Cli, file: &FileConfig) -> Args {
    let log_dir = if cli.no_log {
        None
    } else {
        // 文件里给了就用它，都没有才落到默认目录
        match config::effective(cli.log_dir, file.log_dir.clone()) {
            Some(dir) => Some(dir),
            None => Some(default_log_dir()),
        }
    };
    Args {
        port: config::effective(cli.port, file.port).unwrap_or(DEFAULT_PORT),
        host: config::effective(cli.host, file.host.clone())
            .unwrap_or_else(|| DEFAULT_HOST.to_string()),
        email: config::effective(cli.email, file.email.clone()),
        api_key: config::effective(cli.api_key, file.api_key.clone()),
        warmup: !cli.no_warmup,
        smoke: cli.smoke,
        log_dir,
        endpoints: config::effective(cli.endpoints, file.endpoints.clone()),
        // 正文默认记（面板的「请求详情」靠它）；--no-log-bodies 一票否决，
        // 否则听配置文件的 `log_bodies`，再没有就默认开。
        log_bodies: !cli.no_log_bodies && file.log_bodies.unwrap_or(true),
        // 默认 0（全部保留不截断）
        body_limit: config::effective(cli.body_limit, file.body_limit).unwrap_or(0),
    }
}

fn help_text() -> String {
    [
        "用法：bridge [选项]  /  bridge login",
        "  login              添加账号：浏览器里走一遍 Google 授权，账号落进账号库",
        "  --port=8050        监听端口（默认 8050）",
        "  --host=127.0.0.1   监听地址（默认只监听本机）",
        "  --email=…          只用这一个账号（默认用账号池：库里所有可用账号，按额度选号 + 熔断）",
        "  --api-key=…        给 /v1/* 加一道口令（x-api-key 或 Bearer）",
        "  --no-warmup        启动时不预热（第一个请求来了再签 token）",
        "  --no-log           不写 logs/requests.jsonl",
        "  --no-log-bodies    请求日志里不记正文（默认记正文，全部保留；面板的「请求详情」要靠它）",
        "  --body-limit=<N>   正文截断字符数（默认 0 不截断全部保留）",
        "  --log-dir=<路径>   日志目录（默认 <状态目录>/logs，即 ~/.antigravity-bridge/logs）",
        "  --endpoint=<url>   只用一个端点（默认按 sandbox → daily → prod 回退）",
        "  --smoke            起来之后打一发真请求自测并打印结果",
        "",
        "配置文件（可选）：",
        "  默认读 ~/.antigravity-bridge/config.json；BRIDGE_CONFIG=<路径> 可换一份（测试 / 临时用）",
        "  字段：port / host / email / api_key / endpoints（字符串数组）/ log_dir / log_bodies（真假）",
        "  优先级：命令行显式给的 > 配置文件 > 内建默认；文件缺失、字段缺失、JSON 坏都退化成默认值",
        "  例：{\"port\": 8051, \"email\": \"x@y.com\", \"log_dir\": \"/tmp/bridge-logs\"}",
    ]
    .join("\n")
}

fn log_line(log: &LogFn, message: impl Into<String>) {
    log(format!("[{}] {}", clock(), message.into()));
}

/// `HH:MM:SS`（日志前缀，和 JS 的 `toISOString().slice(11,19)` 对齐）
fn clock() -> String {
    let ms = now_millis();
    let seconds = ms / 1000;
    let day_seconds = seconds.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        day_seconds / 3600,
        (day_seconds % 3600) / 60,
        day_seconds % 60
    )
}

#[tokio::main]
async fn main() {
    // 子命令先行：`login` 走一趟 OAuth 回环添加账号，不起服务。
    if std::env::args().nth(1).as_deref() == Some("login") {
        let client = reqwest::Client::new();
        match dango_bridge::login::run(&client).await {
            Ok(message) => println!("{message}"),
            Err(message) => {
                eprintln!("登录失败：{message}");
                std::process::exit(1);
            }
        }
        return;
    }

    let loaded = config::load();
    let cli = match parse_cli(std::env::args().skip(1).collect()) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    if cli.help {
        println!("{}", help_text());
        return;
    }
    let args = resolve(cli, &loaded.config);

    let log: LogFn = Arc::new(|message: String| eprintln!("{message}"));
    // 配置从哪来、坏没坏，先说清楚：平时没有配置文件就不打这一行，保持原来的启动输出
    match &loaded.source {
        config::Source::File(path) => {
            log_line(
                &log,
                format!("配置：{}（port={}）", path.display(), args.port),
            );
        }
        config::Source::Unusable { path, reason } => log_line(
            &log,
            format!(
                "配置文件忽略，按命令行默认值继续：{}（{reason}）",
                path.display()
            ),
        ),
        config::Source::Absent => {}
    }
    let endpoints = args.endpoints.clone();
    let endpoint_list = endpoints
        .clone()
        .unwrap_or_else(|| ENDPOINTS.iter().map(|e| e.to_string()).collect());

    let setup = match runtime::build_pool(args.email.as_deref(), endpoints, Arc::clone(&log)).await
    {
        Ok(setup) => setup,
        Err(message) => {
            log_line(&log, message);
            std::process::exit(1);
        }
    };
    log_line(&log, setup.summary.clone());

    // Qoder 上游（可选）：配置里 qoder.enabled=true 才建；它只读凭据文件、不联网
    let qoder = runtime::qoder_pool(
        &loaded.config.qoder.clone().unwrap_or_default(),
        Arc::clone(&log),
    );
    if qoder.is_some() {
        log_line(&log, "Qoder 上游已启用（模型带 qoder/ 前缀）");
    }

    let bridge = Arc::new(Bridge::new(BridgeOptions {
        pool: setup.pool,
        qoder,
        store: shared_signatures(),
        log_dir: args.log_dir.clone().map(std::path::PathBuf::from),
        state_dir: runtime::state_dir(),
        log_bodies: args.log_bodies,
        body_limit: args.body_limit,
        api_key: args.api_key.clone(),
        allow_restart: None,
        log: Arc::clone(&log),
        version: VERSION.to_string(),
    }));

    let addr = match dango_bridge::server::serve(Arc::clone(&bridge), &args.host, args.port).await {
        Ok(addr) => addr,
        Err(err) => {
            log_line(
                &log,
                format!("起不来（{}:{}）：{err}", args.host, args.port),
            );
            std::process::exit(1);
        }
    };

    log_line(&log, format!("桥已就绪：http://{addr}"));
    log_line(&log, format!("  Anthropic Base URL : http://{addr}"));
    log_line(
        &log,
        format!("  自用面板           : http://{addr}/（额度、计数、最近请求、重启按钮）"),
    );
    log_line(
        &log,
        format!("  健康检查           : http://{addr}/healthz（含账号池状态）"),
    );
    log_line(
        &log,
        format!("  额度               : http://{addr}/quota?all=1"),
    );
    log_line(
        &log,
        format!("  模型表             : http://{addr}/v1/models"),
    );
    log_line(
        &log,
        format!("  最近请求           : http://{addr}/logs/recent"),
    );
    log_line(
        &log,
        format!(
            "  日志目录           : {}",
            args.log_dir
                .clone()
                .unwrap_or_else(|| "<未启用>".to_string())
        ),
    );
    let hosts: Vec<String> = endpoint_list
        .iter()
        .filter_map(|e| {
            e.split("//")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
                .map(str::to_string)
        })
        .collect();
    log_line(
        &log,
        format!("  上游端点           : {}", hosts.join(" → ")),
    );
    log_line(&log, format!("  进程 {}，Rust 版", std::process::id()));

    if args.warmup {
        let t0 = now_millis();
        if let Err(err) = runtime::warmup(bridge.pool(), &log).await {
            log_line(
                &log,
                format!(
                    "预热失败（服务照常启动，第一个请求会再试）：{}",
                    dango_bridge::server::truncate(&err, 200)
                ),
            );
        }
        // 顺口报一下内存：热完之后这个进程到底占多大，是这套东西最常被问到的数
        log_line(
            &log,
            format!(
                "预热用时 {}ms{}",
                now_millis() - t0,
                dango_bridge::server::memory_note()
            ),
        );
    } else {
        log_line(&log, "跳过预热：access_token 会在第一个请求到来时再签");
    }

    if args.smoke {
        log_line(&log, "== 自测：用 Anthropic 协议打自己一发真请求 ==");
        let t0 = now_millis();
        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{addr}/v1/messages"))
            .json(&json!({
                "model": "gemini-3.6-flash-high",
                "max_tokens": 64,
                "messages": [{ "role": "user", "content": "只回答两个字：收到" }],
            }))
            .send()
            .await;
        match res {
            Ok(response) => {
                let status = response.status();
                let headers_model = response
                    .headers()
                    .get("x-bridge-model")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("?")
                    .to_string();
                let headers_account = response
                    .headers()
                    .get("x-bridge-account")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("?")
                    .to_string();
                let body = response.text().await.unwrap_or_default();
                let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let text = message
                    .get("content")
                    .and_then(Value::as_array)
                    .and_then(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.get("text").and_then(Value::as_str))
                            .next()
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                log_line(&log, format!("HTTP {status}，{}ms", now_millis() - t0));
                log_line(&log, format!("  正文：{text:?}"));
                log_line(
                    &log,
                    format!("  stop_reason={:?}", message.get("stop_reason")),
                );
                log_line(
                    &log,
                    format!(
                        "  usage={}",
                        message
                            .get("usage")
                            .map(|u| u.to_string())
                            .unwrap_or_default()
                    ),
                );
                log_line(&log, format!("  响应头：x-bridge-model={headers_model} x-bridge-account={headers_account}"));
            }
            Err(err) => log_line(&log, format!("自测请求失败：{err}")),
        }
    }

    // Ctrl-C 之后把在飞的请求放一放再退（和 JS 版一样：关监听就行）
    let _ = tokio::signal::ctrl_c().await;
    log_line(&log, "收到 SIGINT，退出");
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 命令行显式给的压过文件里的；命令行没给的仍用文件里的（而不是内建默认）。
    #[test]
    fn command_line_overrides_file() {
        let file = FileConfig {
            port: Some(8050),
            host: Some("0.0.0.0".to_string()),
            log_dir: Some("/tmp/from-file".to_string()),
            ..FileConfig::default()
        };
        let cli = parse_cli(vec![
            "--port=8051".to_string(),
            "--log-dir=/tmp/from-cli".to_string(),
        ])
        .expect("解析命令行");
        let args = resolve(cli, &file);
        assert_eq!(args.port, 8051);
        assert_eq!(args.log_dir.as_deref(), Some("/tmp/from-cli"));
        // 命令行没给 host → 用文件里的 0.0.0.0，而不是内建 127.0.0.1
        assert_eq!(args.host, "0.0.0.0");
    }

    /// `--no-log` 是命令行的显式决定，要压过文件里的 log_dir。
    #[test]
    fn no_log_beats_file_log_dir() {
        let file = FileConfig {
            log_dir: Some("/tmp/from-file".to_string()),
            ..FileConfig::default()
        };
        let cli = parse_cli(vec!["--no-log".to_string()]).expect("解析命令行");
        let args = resolve(cli, &file);
        assert!(args.log_dir.is_none());
    }

    /// 命令行和文件都没给 → 落到内建默认（端口 8050、只监听本机、默认预热、默认日志目录）。
    #[test]
    fn file_and_cli_empty_falls_back_to_builtin_defaults() {
        let args = resolve(
            parse_cli(Vec::new()).expect("解析空命令行"),
            &FileConfig::default(),
        );
        assert_eq!(args.port, DEFAULT_PORT);
        assert_eq!(args.host, DEFAULT_HOST);
        assert!(args.warmup);
        assert!(args.log_dir.is_some());
    }

    /// 正文默认记（面板的「请求详情」要靠它）；`--no-log-bodies` 和文件里的 `log_bodies: false` 都能关。
    #[test]
    fn log_bodies_defaults_on_and_can_be_turned_off() {
        let empty = || parse_cli(Vec::new()).expect("解析空命令行");
        let default = resolve(empty(), &FileConfig::default());
        assert!(default.log_bodies, "不写就是记");

        let by_flag = resolve(
            parse_cli(vec!["--no-log-bodies".to_string()]).expect("解析命令行"),
            &FileConfig::default(),
        );
        assert!(!by_flag.log_bodies, "--no-log-bodies 要能关");

        let file = FileConfig {
            log_bodies: Some(false),
            ..FileConfig::default()
        };
        let by_file = resolve(empty(), &file);
        assert!(!by_file.log_bodies, "文件里关掉也算关");
    }
}
