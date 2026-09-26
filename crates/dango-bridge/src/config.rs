//! 可选的配置文件：`~/.antigravity-bridge/config.json`。
//!
//! 为什么要有它：常用项（端口、默认账号、端点、日志目录、host、api_key）每次都靠命令行敲太啰嗦，
//! 尤其是 launchd / 桌面壳拉起的进程根本没有命令行可敲。文件当「默认值」用，**命令行永远优先** ——
//! 这样临时改一项（比如换端口对拍）不用去动文件。
//!
//! 一处刻意的取舍：读文件 / 解析 JSON 出任何问题都不报错、不崩，只退化成「没有配置文件」，
//! 并把原因交回给调用方去打一行日志。配置写坏不该让一座常驻的桥起不来，也不该默不作声。

use std::path::PathBuf;

use serde::Deserialize;

/// 配置文件里认识的那几项。字段全 `Option`（配 `serde(default)`）：缺哪项就交给调用方退到内建默认值，
/// 「只写一个 port」的最小配置也合法；不认识的键静默忽略（前向兼容 —— 别为一个没见过的字段就报错）。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct FileConfig {
    /// 监听端口（对应 `--port`）
    pub port: Option<u16>,
    /// 监听地址（对应 `--host`，默认 127.0.0.1）
    pub host: Option<String>,
    /// 单账号模式：只用这一个邮箱（对应 `--email`）
    pub email: Option<String>,
    /// `/v1/*` 的口令（对应 `--api-key`）
    pub api_key: Option<String>,
    /// 上游端点列表，按顺序回退（对应 `--endpoint`，但这里是数组）
    pub endpoints: Option<Vec<String>>,
    /// 请求日志目录（对应 `--log-dir`）
    pub log_dir: Option<String>,
    /// 请求日志里记不记正文（`false` 等于 `--no-log-bodies`；不写就是记）
    pub log_bodies: Option<bool>,
    /// 正文截断上限（字符数，0 表示不截断全部保留；默认 0 全部保留）
    pub body_limit: Option<usize>,
    /// Qoder CN 上游（可选）。不写 = 不启用，`qoder/` 前缀的请求会明确报未启用。
    pub qoder: Option<QoderConfig>,
}

/// Qoder CN 上游配置。派生 `PartialEq, Eq, Default` 是 [`FileConfig`] 的要求；
/// `#[serde(default)]` 保证「只写 `{"enabled":true}`」也合法。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct QoderConfig {
    /// 开关。false（默认）= 不建 Qoder 池
    pub enabled: bool,
    /// 凭据文件；默认 `~/.qoder-bridge/auth.json`
    pub auth_file: Option<String>,
    /// job token 拿不到时执行的刷新命令（30s 超时）；默认空 = 不做
    pub refresh_command: Option<String>,
    /// 网关地址；默认 `https://gateway.qoder.com.cn`
    pub base_url: Option<String>,
    /// 沿用的 machine_id 文件（只读）；默认 `~/.qoder-cn/.auth/machine_id`
    pub machine_id_file: Option<String>,
    /// 中和上游自带的产品人设（可选，默认 `false`）：置 `true` 时会在对话最前面垫一条中性
    /// 提示（模型当自己是「客户端配置的裸模型」，并带上桥的当前日期）；默认不开 —— 上游
    /// 自带什么就是什么，桥不额外注入任何文字。
    pub neutralize: Option<bool>,
}

/// 这份配置是从哪儿来的（给日志用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// 真读到了这份文件
    File(PathBuf),
    /// 没有任何配置文件（默认位置也不存在）—— 这是常态，不打日志
    Absent,
    /// 指定的文件读不了 / JSON 坏了：内容已经退化成空配置，但原因要交回去说清楚
    Unusable { path: PathBuf, reason: String },
}

/// 加载结果：文件里的值 + 来源描述。
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: FileConfig,
    pub source: Source,
}

/// 默认位置 `~/.antigravity-bridge/config.json`。拿不到 HOME（或 USERPROFILE）就没有默认位置。
pub fn default_path() -> Option<PathBuf> {
    Some(
        crate::oauth::home_dir()?
            .join(".antigravity-bridge")
            .join("config.json"),
    )
}

/// 按优先级挑一个值：命令行显式给的（`cli`）优先于文件里的（`file`）。
///
/// 「命令行 > 配置文件」这条规则只在这里写一次，CLI 和桌面壳共用，免得两边各写一遍、日后跑偏。
pub fn effective<T>(cli: Option<T>, file: Option<T>) -> Option<T> {
    cli.or(file)
}

/// 从 `BRIDGE_CONFIG` 指定的位置（测试 / 临时用）或默认位置加载。
///
/// 默认位置不存在就是「没有配置」（静默）；但 `BRIDGE_CONFIG` 是用户有意指的，指了一份却
/// 读不了 / 坏了会以 `Unusable` 报回去，让调用方打一行日志 —— 那种情况下沉默会害人。
///
/// 不抢 `BRIDGE_STATE_DIR` 这个名字：那是别的模块用来放状态文件的，跟这里没关系。
pub fn load() -> Loaded {
    match std::env::var_os("BRIDGE_CONFIG") {
        Some(path) => load_from(PathBuf::from(path)),
        None => match default_path() {
            Some(path) if path.is_file() => load_from(path),
            _ => Loaded {
                config: FileConfig::default(),
                source: Source::Absent,
            },
        },
    }
}

/// 从一个明确路径加载。单测直接调它，就不必去碰用户真实的 `~/.antigravity-bridge`。
pub fn load_from(path: PathBuf) -> Loaded {
    match std::fs::read_to_string(&path) {
        Ok(text) => match parse(&text) {
            Ok(config) => Loaded {
                config,
                source: Source::File(path),
            },
            Err(err) => Loaded {
                config: FileConfig::default(),
                source: Source::Unusable {
                    path,
                    reason: format!("JSON 解析失败：{err}"),
                },
            },
        },
        Err(err) => Loaded {
            config: FileConfig::default(),
            source: Source::Unusable {
                path,
                reason: format!("读不了：{err}"),
            },
        },
    }
}

/// 纯函数：把 JSON 文本变成配置。和文件系统无关，单测用。
pub fn parse(text: &str) -> Result<FileConfig, serde_json::Error> {
    serde_json::from_str(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 每个测试用独立临时目录下的一个不存在的文件路径；测完把整个目录删掉。
    /// 名字带 pid + 纳秒，避免并行测试之间撞车。**绝不碰真实的 `~/.antigravity-bridge`。**
    fn tmp_config(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间早于 UNIX 纪元")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "bridge-config-test-{}-{tag}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir.join("config.json")
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn missing_file_degrades_to_empty_config() {
        let path = tmp_config("missing");
        let loaded = load_from(path.clone());
        // 退化成空配置：调用方拿到全 None，于是全走内建默认值
        assert_eq!(loaded.config, FileConfig::default());
        match loaded.source {
            Source::Unusable { path: p, reason } => {
                assert_eq!(p, path);
                assert!(reason.contains("读不了"), "缺文件要说明读不了：{reason}");
            }
            other => panic!("缺文件应是 Unusable，得到 {other:?}"),
        }
        cleanup(&path);
    }

    #[test]
    fn broken_json_degrades_and_explains() {
        let path = tmp_config("broken");
        std::fs::write(&path, "{ 这不是 JSON }").expect("写坏配置");
        let loaded = load_from(path.clone());
        assert_eq!(loaded.config, FileConfig::default());
        match loaded.source {
            Source::Unusable { path: p, reason } => {
                assert_eq!(p, path);
                // 必须能说清「哪儿坏了」，不能静默
                assert!(reason.contains("JSON"), "坏 JSON 要说明解析失败：{reason}");
            }
            other => panic!("坏 JSON 应是 Unusable，得到 {other:?}"),
        }
        cleanup(&path);
    }

    #[test]
    fn partial_fields_only_set_what_is_present() {
        let cfg = parse(r#"{"port": 8051, "log_dir": "/tmp/bridge-logs"}"#).expect("解析部分字段");
        assert_eq!(cfg.port, Some(8051));
        assert_eq!(cfg.log_dir.as_deref(), Some("/tmp/bridge-logs"));
        // 没写的字段保持 None，交给调用方退到默认值
        assert_eq!(cfg.host, None);
        assert_eq!(cfg.email, None);
        assert_eq!(cfg.api_key, None);
        assert_eq!(cfg.endpoints, None);
        assert_eq!(cfg.log_bodies, None);
    }

    #[test]
    fn unknown_keys_are_ignored_for_forward_compat() {
        let cfg = parse(r#"{"port": 8051, "future_knob": true}"#).expect("未知键不该报错");
        assert_eq!(cfg.port, Some(8051));
    }

    #[test]
    fn all_fields_round_trip() {
        let cfg = parse(
            r#"{
                "port": 8051,
                "host": "0.0.0.0",
                "email": "x@y.com",
                "api_key": "secret",
                "endpoints": ["https://a/v1internal", "https://b/v1internal"],
                "log_dir": "/tmp/logs",
                "log_bodies": false
            }"#,
        )
        .expect("解析全字段");
        assert_eq!(cfg.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(cfg.email.as_deref(), Some("x@y.com"));
        assert_eq!(cfg.api_key.as_deref(), Some("secret"));
        assert_eq!(cfg.endpoints.as_ref().map(Vec::len), Some(2));
        assert_eq!(cfg.log_dir.as_deref(), Some("/tmp/logs"));
        assert_eq!(cfg.log_bodies, Some(false));
    }

    #[test]
    fn qoder_block_parses_and_is_optional() {
        let cfg = parse(
            r#"{"qoder": {"enabled": true, "auth_file": "/tmp/a.json", "refresh_command": "refresh-me", "machine_id_file": "/tmp/mid", "neutralize": true}}"#,
        )
        .expect("解析 qoder");
        let qoder = cfg.qoder.expect("有 qoder");
        assert!(qoder.enabled);
        assert_eq!(qoder.auth_file.as_deref(), Some("/tmp/a.json"));
        assert_eq!(qoder.refresh_command.as_deref(), Some("refresh-me"));
        assert_eq!(qoder.machine_id_file.as_deref(), Some("/tmp/mid"));
        assert_eq!(qoder.neutralize, Some(true));
        assert_eq!(qoder.base_url, None);
        // 不写 qoder 就是 None（不启用）
        assert!(parse(r#"{"port": 8051}"#).expect("解析").qoder.is_none());
        // qoder 块里的未知键静默忽略；只写 enabled 也合法
        let minimal = parse(r#"{"qoder": {"enabled": true, "future_knob": 1}}"#).expect("解析");
        let minimal = minimal.qoder.expect("有 qoder");
        assert!(minimal.enabled);
        assert_eq!(minimal.auth_file, None);
        // 不写 neutralize 就是 None，由 QoderSession 按「默认关（原样透传）」处理
        assert_eq!(minimal.neutralize, None);
    }

    #[test]
    fn command_line_beats_the_file() {
        // 命令行显式给了就用它；没给才落到文件里的值；两边都没有就是 None。
        assert_eq!(effective(Some(8051u16), Some(8050)), Some(8051));
        assert_eq!(effective(None, Some(8050u16)), Some(8050));
        assert_eq!(effective::<u16>(None, None), None);
    }
}
