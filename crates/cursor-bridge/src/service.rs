//! `cursor-bridge service …`：把桥交给 launchd 管（开机自启、挂了自动拉起）。
//!
//! 为什么做进桥里而不是另给一个 plist 文件：plist 里最容易错的两件事 ——
//! **launchd 的 PATH 很干净**（不写死 `CURSOR_BRIDGE_AGENT_BIN` 就找不到 cursor-agent），
//! 以及**别把 API key 写进 plist**（明文躺在磁盘上）。这两条由代码保证，用户不用记。
//!
//! 只支持 macOS（launchd）。Linux 的 systemd 没做，别假装支持。

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::home_dir;
use crate::config::{Backend, Config, DEFAULT_AGENT_ENDPOINT, DEFAULT_API_ENDPOINT};
use crate::error::{Error, Result};

pub const LABEL: &str = "local.cursor-bridge";

fn plist_path() -> Result<PathBuf> {
    let home = home_dir().ok_or_else(|| Error::Service {
        message: "取不到 HOME，找不到 ~/Library/LaunchAgents".into(),
    })?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn log_dir() -> Result<PathBuf> {
    let home = home_dir().ok_or_else(|| Error::Service {
        message: "取不到 HOME，找不到 ~/Library/Logs".into(),
    })?;
    Ok(home.join("Library/Logs/cursor-bridge"))
}

/// plist 里的 XML 文本转义（路径里可能有 `&` 这类字符）。
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// 生成 plist。纯函数，方便直接断言内容。
///
/// `agent_bin`：CLI 路径，**必须写进环境变量**（launchd 不继承你的 PATH）。
/// 只把「和默认值不一样」的配置写进去，减少将来默认值变化带来的偏差。
pub fn plist_body(exe: &Path, cfg: &Config, agent_bin: Option<&Path>, dir: &Path) -> String {
    let mut env = String::new();
    let mut add = |key: &str, value: &str| {
        env.push_str(&format!(
            "        <key>{}</key>\n        <string>{}</string>\n",
            xml_escape(key),
            xml_escape(value)
        ));
    };
    if let Some(bin) = agent_bin {
        add("CURSOR_BRIDGE_AGENT_BIN", &bin.display().to_string());
    }
    if cfg.port != 8052 {
        add("CURSOR_BRIDGE_PORT", &cfg.port.to_string());
    }
    if cfg.backend != Backend::Cli {
        add("CURSOR_BRIDGE_BACKEND", cfg.backend.as_str());
    }
    if cfg.api_endpoint != DEFAULT_API_ENDPOINT {
        add("CURSOR_BRIDGE_API_ENDPOINT", &cfg.api_endpoint);
    }
    if cfg.agent_endpoint != DEFAULT_AGENT_ENDPOINT {
        add("CURSOR_BRIDGE_AGENT_ENDPOINT", &cfg.agent_endpoint);
    }
    if let Some(mode) = cfg.mode.as_deref() {
        if mode != "ask" {
            add("CURSOR_BRIDGE_MODE", mode);
        }
    }
    if let Some(state_dir) = cfg.state_dir.to_str() {
        if Some(PathBuf::from(state_dir)) != home_dir().map(|h| h.join(".cursor-bridge")) {
            add("CURSOR_BRIDGE_STATE_DIR", state_dir);
        }
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>serve</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{dir}</string>
    <key>EnvironmentVariables</key>
    <dict>
{env}    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{err}</string>
</dict>
</plist>
"#,
        label = LABEL,
        exe = xml_escape(&exe.display().to_string()),
        dir = xml_escape(&dir.display().to_string()),
        out = xml_escape(&dir.join("out.log").display().to_string()),
        err = xml_escape(&dir.join("err.log").display().to_string()),
        env = env,
    )
}

fn uid() -> Result<String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| Error::Service {
            message: format!("跑 id -u 失败：{e}"),
        })?;
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uid.is_empty() {
        return Err(Error::Service {
            message: "读不到当前用户 uid".into(),
        });
    }
    Ok(uid)
}

fn launchctl(args: &[&str]) -> Result<String> {
    let out = Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|e| Error::Service {
            message: format!("跑 launchctl 失败：{e}"),
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        return Err(Error::Service {
            message: format!(
                "launchctl {} 失败：{}",
                args.join(" "),
                if stderr.is_empty() { stdout } else { stderr }
            ),
        });
    }
    Ok(if stdout.is_empty() { stderr } else { stdout })
}

fn bootout(label: &str) -> Result<()> {
    let uid = uid()?;
    // 没加载过会报错，这里当正常（幂等）
    let _ = launchctl(&["bootout", &format!("gui/{uid}/{label}")]);
    Ok(())
}

pub fn install() -> Result<()> {
    let cfg = Config::from_env();
    let exe = std::env::current_exe().map_err(Error::Io)?;
    let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let agent_bin = crate::cli::AgentCmd::locate().ok().map(|c| c.bin);

    if cfg.single_api_key.is_some() {
        println!(
            "提醒： CURSOR_BRIDGE_API_KEY 不会写进 plist（明文落盘不好）。\
             launchd 起的桥读不到它 —— 要日抛号请写 {}",
            cfg.accounts_file.display()
        );
    }
    if agent_bin.is_none() && cfg.backend == Backend::Cli {
        println!("警告： 找不到 cursor-agent，launchd 里的桥会起不来（CLI 后端必须要它）");
    }

    let plist = plist_path()?;
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    let logs = log_dir()?;
    std::fs::create_dir_all(&logs).map_err(Error::Io)?;

    let body = plist_body(&exe, &cfg, agent_bin.as_deref(), &dir);
    let changed = std::fs::read_to_string(&plist)
        .map(|old| old != body)
        .unwrap_or(true);
    std::fs::write(&plist, &body).map_err(Error::Io)?;
    println!("写好了：  {}", plist.display());
    if changed {
        println!("内容有变化，重载服务");
    }

    bootout(LABEL)?;
    let uid = uid()?;
    launchctl(&["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])?;
    println!("已加载：  {LABEL}");
    println!("日志：    {}/out.log（错误在 err.log）", logs.display());
    println!("\n验证：    curl -s http://127.0.0.1:{}/healthz", cfg.port);
    println!("关掉：    cursor-bridge service uninstall");
    println!("注意：    如果之前是手动 nohup 起的，先停掉那条，否则两个进程抢同一个端口");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    bootout(LABEL)?;
    let plist = plist_path()?;
    if plist.exists() {
        std::fs::remove_file(&plist).map_err(Error::Io)?;
        println!("删掉了：  {}", plist.display());
    } else {
        println!("没有 plist：{}", plist.display());
    }
    println!("已卸载：  {LABEL}");
    Ok(())
}

pub fn status() -> Result<()> {
    let plist = plist_path()?;
    println!("plist：     {}", plist.display());
    println!("plist 在？  {}", if plist.exists() { "在" } else { "不在" });
    let uid = uid()?;
    match launchctl(&["print", &format!("gui/{uid}/{LABEL}")]) {
        Ok(text) => {
            // 只挑几行有用的
            for line in text.lines() {
                let t = line.trim();
                if t.starts_with("state =")
                    || t.starts_with("pid =")
                    || t.starts_with("last exit code")
                    || t.starts_with("path =")
                {
                    println!("  {t}");
                }
            }
        }
        Err(err) => println!("未加载：    {err}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn escapes_xml_specials() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        assert_eq!(xml_escape("/Users/someone/code"), "/Users/someone/code");
    }

    #[test]
    fn plist_carries_label_exe_and_agent_bin() {
        let cfg = Config {
            port: 8055,
            ..Config::default()
        };
        let body = plist_body(
            Path::new("/Users/j/code/bridge/cursor-bridge/target/release/cursor-bridge"),
            &cfg,
            Some(Path::new("/Users/j/.local/bin/cursor-agent")),
            Path::new("/Users/j/code/bridge/cursor-bridge"),
        );
        assert!(body.contains("<string>local.cursor-bridge</string>"));
        assert!(body.contains("<string>serve</string>"));
        assert!(body.contains("/target/release/cursor-bridge</string>"));
        // launchd 的 PATH 很干净，CLI 路径必须写死
        assert!(body.contains("<key>CURSOR_BRIDGE_AGENT_BIN</key>"));
        assert!(body.contains("/Users/j/.local/bin/cursor-agent"));
        // 非默认端口要带上
        assert!(body.contains("<key>CURSOR_BRIDGE_PORT</key>"));
        assert!(body.contains("<string>8055</string>"));
        // 默认值不写（将来默认变了也不会被旧 plist 钉住）
        assert!(!body.contains("CURSOR_BRIDGE_BACKEND"));
        assert!(!body.contains("CURSOR_BRIDGE_MODE"));
        assert!(!body.contains("CURSOR_BRIDGE_STATE_DIR"));
        assert!(body.contains("RunAtLoad"));
        assert!(body.contains("KeepAlive"));
        assert!(body.contains("out.log"));
    }

    #[test]
    fn plist_never_contains_api_key() {
        let cfg = Config {
            single_api_key: Some("not-a-real-key".into()),
            ..Config::default()
        };
        let body = plist_body(Path::new("/x/cursor-bridge"), &cfg, None, Path::new("/x"));
        assert!(!body.contains("not-a-real-key"));
        assert!(!body.contains("CURSOR_BRIDGE_API_KEY"));
    }

    #[test]
    fn native_backend_is_recorded() {
        let cfg = Config {
            backend: Backend::Native,
            agent_endpoint: "http://127.0.0.1:8060".into(),
            ..Config::default()
        };
        let body = plist_body(Path::new("/x/cursor-bridge"), &cfg, None, Path::new("/x"));
        assert!(body.contains("<key>CURSOR_BRIDGE_BACKEND</key>"));
        assert!(body.contains("<string>native</string>"));
        assert!(body.contains("CURSOR_BRIDGE_AGENT_ENDPOINT"));
        assert!(body.contains("http://127.0.0.1:8060"));
    }
}
