//! 命令行入口：`serve` 起反代，`status` 看配置和登录态，`models` 列模型。

use std::process::{ExitCode, Stdio};

use tokio::process::Command;

use cursor_bridge::cli::AgentCmd;
use cursor_bridge::config::Config;

const HELP: &str = "\
cursor-bridge — 把 Cursor CLI 包成 Anthropic / OpenAI 兼容的反代

用法：
  cursor-bridge serve              起反代（默认 127.0.0.1:8052）
  cursor-bridge status             看配置、CLI 路径、当前登录态
  cursor-bridge models             列 CLI 给的模型清单（就是 /v1/models 的来源）
  cursor-bridge service install    交给 launchd 管：开机自启、挂了自动拉起
  cursor-bridge service status     看 launchd 里的状态
  cursor-bridge service uninstall  卸载 launchd 服务
  cursor-bridge --help

环境变量全部以 CURSOR_BRIDGE_ 开头，见 README。";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") | Some("help") => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Some("status") => status().await,
        Some("models") => list_models().await,
        Some("serve") => serve().await,
        Some("service") => service(&args[1..]),
        Some(other) => {
            eprintln!("未知子命令：{other}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}

/// launchd 那套：install / uninstall / status。
fn service(args: &[String]) -> ExitCode {
    let result = match args.first().map(String::as_str) {
        Some("install") => cursor_bridge::service::install(),
        Some("uninstall") | Some("remove") => cursor_bridge::service::uninstall(),
        Some("status") | None => cursor_bridge::service::status(),
        Some(other) => {
            eprintln!("service 只认 install / status / uninstall，收到：{other}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(1)
        }
    }
}

fn print_config(cfg: &Config) {
    println!("后端：        {}", cfg.backend.as_str());
    println!("监听：        http://{}", cfg.addr());
    println!("状态目录：    {}", cfg.state_dir.display());
    println!("工作区：      {}", cfg.workspace.display());
    println!("账号清单：    {}", cfg.accounts_file.display());
    println!(
        "CLI 模式：    {}",
        cfg.mode
            .as_deref()
            .unwrap_or("（不传 --mode，用 CLI 默认）")
    );
    println!("单轮上限：    {} 秒", cfg.timeout_secs);
    match &cfg.single_api_key {
        Some(_) => println!("API key：     已配置（CURSOR_BRIDGE_API_KEY，优先于 CLI 登录态）"),
        None => println!("API key：     未配置（用 CLI 当前登录态）"),
    }
    for note in &cfg.notes {
        println!("注意：        {note}");
    }
}

async fn status() -> ExitCode {
    let cfg = Config::from_env();
    print_config(&cfg);

    let cmd = match AgentCmd::locate() {
        Ok(cmd) => cmd,
        Err(err) => {
            eprintln!("\n找不到 Cursor CLI：{err}");
            return ExitCode::from(1);
        }
    };
    println!("\nCLI：         {}", cmd.bin.display());

    // 问一下 CLI 自己的登录态；这条命令只读，不会动凭据
    match Command::new(&cmd.bin)
        .arg("status")
        .stdin(Stdio::null())
        .output()
        .await
    {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            let first = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("");
            if first.is_empty() {
                let err = String::from_utf8_lossy(&output.stderr);
                println!(
                    "登录态：      读不出来（{}）",
                    cursor_bridge::turn::compact_stderr(&err)
                );
            } else {
                println!("登录态：      {first}");
            }
        }
        Err(err) => println!("登录态：      问不出来（{err}）"),
    }

    // 账号清单存在时只说数量，不打印 key
    if cfg.accounts_file.exists() {
        match cursor_bridge::accounts::load(&cfg.accounts_file) {
            Ok(accounts) => println!(
                "日抛号：      {} 个（{}）",
                accounts.len(),
                accounts
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(err) => println!("日抛号：      清单读不出来（{err}）"),
        }
    } else {
        println!("日抛号：      没有清单（{}）", cfg.accounts_file.display());
    }
    ExitCode::SUCCESS
}

async fn list_models() -> ExitCode {
    let cmd = match AgentCmd::locate() {
        Ok(cmd) => cmd,
        Err(err) => {
            eprintln!("找不到 Cursor CLI：{err}");
            return ExitCode::from(1);
        }
    };
    match cursor_bridge::models::list(&cmd).await {
        Ok(models) => {
            for m in models {
                println!("{}\t{}", m.id, m.name);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("问 CLI 要模型清单失败：{err}");
            ExitCode::from(1)
        }
    }
}

async fn serve() -> ExitCode {
    let cfg = Config::from_env();
    match cursor_bridge::server::serve(cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("起服务失败：{err}");
            ExitCode::from(1)
        }
    }
}
