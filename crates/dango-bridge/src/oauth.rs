//! OAuth 凭据抽取 + refresh_token → access_token 交换。
//!
//! 为什么要从官方二进制里抽 client 凭据：refresh_token 与「签发它的 client_id」绑定，
//! 自己新建 OAuth 客户端无法复用存量 refresh_token（会 invalid_grant）。官方客户端的
//! 凭据硬编码在 language_server 里，我们只读、只在自己进程内使用，不进仓库、不落盘。
//!
//! 移植自 `src/oauth.mjs`，三点和 JS 版不同（都是有意的）：
//!   1. **不另起子进程**。JS 版因为 V8 + 分配器会把几十 MB 大块留下，所以扫描放在短命子进程里；
//!      Rust 版全程复用同一个 4 MB 缓冲、读一块处理一块，峰值就那 4 MB，没必要再开进程。
//!   2. 搜索用 memchr（SIMD）找字面量，再在命中的小窗口里手工校验模式，
//!      不引 regex 依赖，也不把整块字节转成字符串。
//!   3. 二进制路径**按平台**找（macOS / Windows 各一组），因为这东西要能在他那台 Windows 上跑。

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::{iso_from_millis, now_millis, Account};

// 这两条只在非 Windows 上会用到（Windows 那组路径在 candidate_binaries() 里现拼），
// 所以按平台门掉：Windows 上编 core 会报「常量没人用」，而这个仓库的 lint 口径是
// `-D warnings` —— 门掉是为了 core 在 Windows 上也编得干净。
/// 官方 Antigravity 客户端里的 language_server（凭据硬编码在它里面）。
#[cfg(not(windows))]
const OFFICIAL_BIN_MAC: &str =
    "/Applications/Antigravity.app/Contents/Resources/bin/language_server";
/// 老桥（Antigravity Tools）自己的二进制里也有一张明文 OAuth 客户端表。
#[cfg(not(windows))]
const TOOLS_BIN_MAC: &str = "/Applications/Antigravity Tools.app/Contents/MacOS/antigravity-tools";

const CLIENT_ID_SUFFIX: &[u8] = b".apps.googleusercontent.com";
const SECRET_PREFIX: &[u8] = b"GOCSPX-";
const SCAN_CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// 跨块搭桥：比最长模式（GOCSPX- + 最多 300）长，4 KB 绰绰有余。
const SCAN_CARRY_BYTES: usize = 4096;
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OauthCandidates {
    pub extracted_at: String,
    pub sources: Vec<String>,
    pub client_ids: Vec<String>,
    pub secrets: Vec<String>,
}

/// 一个账号的 refresh_token 交换成功后的结果（access_token 只在内存里）。
#[derive(Debug, Clone)]
pub struct Exchange {
    pub access_token: String,
    pub expires_in: u64,
    pub client_id: String,
    pub client_secret: String,
    /// 试过的组合：只记指纹和错误码，不记凭据原文
    pub attempts: Vec<CredentialAttempt>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialAttempt {
    pub client_id: String,
    pub secret: String,
    pub ok: bool,
    pub code: Option<String>,
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// 老桥（Antigravity Tools）的账号库位置：`~/.antigravity_tools/`（macOS 与 Windows 都是这个约定，
/// Windows 上就是 `%USERPROFILE%\.antigravity_tools`）。现在只当**迁移来源**读一次，桥不再往里写。
pub fn legacy_accounts_root() -> Option<PathBuf> {
    Some(home_dir()?.join(".antigravity_tools"))
}

/// 桥自己的账号库目录 + 它是不是「默认位置」。
///
/// 只有默认位置（`~/.antigravity-bridge`）才谈得上从老目录迁移：`BRIDGE_STATE_DIR` 是测试 / 临时用的，
/// 那边绝不去碰用户真实的 `~/.antigravity_tools`（否则跑个测试就把真账号读进来了）。
fn own_accounts_root() -> Option<(PathBuf, bool)> {
    match std::env::var_os("BRIDGE_STATE_DIR") {
        Some(dir) => Some((PathBuf::from(dir), false)),
        None => Some((home_dir()?.join(".antigravity-bridge"), true)),
    }
}

/// 迁移痕迹（进程内只记一次，给 `/healthz` 报一句实话用）。
static MIGRATED_FROM: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// 迁移说明：这次进程真迁移过就返回一句人话，没迁过返回 `None`。
pub fn migration_note() -> Option<String> {
    MIGRATED_FROM.get().cloned()
}

/// 账号库落在哪：自己的目录优先；自己这边还没有账号库、老目录里有，就把老的那份**拷**过来
/// （只拷贝、不动原件、一次性），之后一律只认自己的 —— 老工具被删掉也不影响桥。
pub fn accounts_root() -> Option<PathBuf> {
    let (own, is_default) = own_accounts_root()?;
    let legacy = if is_default {
        legacy_accounts_root()
    } else {
        None
    };
    Some(resolve_accounts_root(&own, legacy.as_deref()))
}

/// 选目录的纯逻辑（单测直接调，不碰环境变量也不碰真实家目录）。
fn resolve_accounts_root(own: &Path, legacy: Option<&Path>) -> PathBuf {
    if own.join("accounts.json").is_file() {
        return own.to_path_buf();
    }
    if let Some(legacy) = legacy {
        if legacy.join("accounts.json").is_file() {
            match migrate_accounts(own, legacy) {
                Ok(count) => {
                    let _ = MIGRATED_FROM.set(format!("{}（{count} 个账号）", legacy.display()));
                    return own.to_path_buf();
                }
                Err(err) => {
                    // 拷不过来就先只读老目录：宁可读旧的，也不能让桥没账号
                    eprintln!("账号迁移失败，先继续读老目录：{err}");
                    return legacy.to_path_buf();
                }
            }
        }
    }
    own.to_path_buf()
}

/// 把老账号库拷进自己的目录：`accounts.json` 索引 + `accounts/*.json` 每个账号一份。
/// 返回拷了几个账号文件。
fn migrate_accounts(own: &Path, legacy: &Path) -> Result<usize, String> {
    let own_accounts = own.join("accounts");
    std::fs::create_dir_all(&own_accounts)
        .map_err(|e| format!("建目录 {} 失败：{e}", own_accounts.display()))?;
    copy_owner_only(&legacy.join("accounts.json"), &own.join("accounts.json"))?;
    let mut copied = 0;
    if let Ok(entries) = std::fs::read_dir(legacy.join("accounts")) {
        for entry in entries.flatten() {
            let src = entry.path();
            if src.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = src.file_name() else {
                continue;
            };
            copy_owner_only(&src, &own_accounts.join(name))?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// 拷一份到指定位置，权限收紧到 0600（账号文件里有 refresh_token，老文件是 0644，太敞了）。
fn copy_owner_only(src: &Path, dst: &Path) -> Result<(), String> {
    let bytes = std::fs::read(src).map_err(|e| format!("读 {} 失败：{e}", src.display()))?;
    std::fs::write(dst, &bytes).map_err(|e| format!("写 {} 失败：{e}", dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 要扫的二进制，按「官方 → 老桥」的顺序（账号的 refresh_token 是其中某一对签发的，逐个试比猜快）。
///
/// 路径按平台来；另外支持环境变量覆盖（`ANTIGRAVITY_OFFICIAL_BIN` / `ANTIGRAVITY_TOOLS_BIN`），
/// 装机位置和猜测不一致时不用改代码。Windows 上的默认位置还没在真机核对过，
/// 所以找不到时会把「找过哪些路径」一起报出来，方便按实际情况补一条。
pub fn candidate_binaries() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };

    if let Some(p) = std::env::var_os("ANTIGRAVITY_OFFICIAL_BIN") {
        push(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("ANTIGRAVITY_TOOLS_BIN") {
        push(PathBuf::from(p));
    }

    #[cfg(target_os = "macos")]
    {
        push(PathBuf::from(OFFICIAL_BIN_MAC));
        push(PathBuf::from(TOOLS_BIN_MAC));
    }

    #[cfg(windows)]
    {
        // Windows：官方客户端装在 %LOCALAPPDATA%\Programs\Antigravity\…
        // （Electron 风格布局：resources/bin/language_server.exe）
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            for rel in [
                r"Programs\Antigravity\resources\bin\language_server.exe",
                r"Antigravity\resources\bin\language_server.exe",
                r"Programs\antigravity\resources\bin\language_server.exe",
            ] {
                push(local.join(rel));
            }
        }
        // 老桥（Antigravity Tools）在 Windows 上通常是免安装的 exe，常见位置也试着找一下
        if let Some(home) = home_dir() {
            for rel in [
                r"AppData\Local\Antigravity Tools\antigravity-tools.exe",
                r"AppData\Local\AntigravityTools\antigravity-tools.exe",
                r"Antigravity Tools\antigravity-tools.exe",
            ] {
                push(home.join(rel));
            }
        }
    }

    // 没被 cfg 覆盖的平台（比如开发机上跑 Linux）也不至于完全瞎：把 macOS 的路径留着，
    // 反正 `exists()` 会过滤掉不存在的，最后报错信息里能看到「找过哪些」。
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        push(PathBuf::from(OFFICIAL_BIN_MAC));
        push(PathBuf::from(TOOLS_BIN_MAC));
    }

    out
}

/// 存在且是文件的那些候选（扫描只扫这些）。
pub fn existing_binaries() -> Vec<PathBuf> {
    candidate_binaries()
        .into_iter()
        .filter(|p| p.is_file())
        .collect()
}

/// 日志/展示用的短指纹（和 JS 版一致：sha256 前 16 位十六进制）。
pub fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// 邮箱掩码：`omarcuto123321@gmail.com` → `om************@gmail.com`。
pub fn mask_email(email: &str) -> String {
    let Some((name, domain)) = email.split_once('@') else {
        return "<unknown>".to_string();
    };
    let head: String = name.chars().take(2).collect();
    let stars = "*".repeat(name.chars().count().saturating_sub(2).max(1));
    format!("{head}{stars}@{domain}")
}

/// 扫一批二进制，抽出 client_id / secret 候选。
///
/// 内存：全程只有一个 `chunk_bytes` 大小的缓冲（外加 4 KB 搭桥），读一块处理一块 ——
/// JS 版在这里踩过坑（整块读进内存，服务 RSS 冲到 612 MB），Rust 版不许再犯。
pub fn extract_candidates_from_binary(paths: &[PathBuf]) -> Result<OauthCandidates, String> {
    extract_candidates_with_chunk(paths, SCAN_CHUNK_BYTES)
}

pub fn extract_candidates_with_chunk(
    paths: &[PathBuf],
    chunk_bytes: usize,
) -> Result<OauthCandidates, String> {
    let mut client_ids: Vec<String> = Vec::new();
    let mut secrets: Vec<String> = Vec::new();
    let mut sources = Vec::new();

    for path in paths {
        if !path.is_file() {
            continue;
        }
        let (ids, secs) = scan_file(path, chunk_bytes)?;
        for id in ids {
            if !client_ids.contains(&id) {
                client_ids.push(id);
            }
        }
        for s in secs {
            if !secrets.contains(&s) {
                secrets.push(s);
            }
        }
        sources.push(path.display().to_string());
    }

    if client_ids.is_empty() {
        let looked: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        return Err(format!(
            "二进制里没抽到 client_id（找过这些路径：{}）。装机位置不一样的话，用 ANTIGRAVITY_OFFICIAL_BIN / ANTIGRAVITY_TOOLS_BIN 指一下。",
            looked.join("、")
        ));
    }

    Ok(OauthCandidates {
        extracted_at: iso_from_millis(now_millis()),
        sources,
        client_ids,
        secrets,
    })
}

/// 扫一个文件：返回（client_id 列表, secret 列表），顺序按出现先后。
fn scan_file(path: &Path, chunk_bytes: usize) -> Result<(Vec<String>, Vec<String>), String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("打不开 {}：{e}", path.display()))?;
    let mut buf = vec![0u8; chunk_bytes];
    // carry + 本块：一次分配，循环复用（这是内存友好的关键）
    let mut hay = vec![0u8; chunk_bytes + SCAN_CARRY_BYTES];

    let mut ids = Vec::new();
    let mut secrets = Vec::new();
    let mut carry_len = 0usize;

    loop {
        let read = file
            .read(&mut buf)
            .map_err(|e| format!("读 {} 出错：{e}", path.display()))?;
        if read == 0 {
            break;
        }
        hay[carry_len..carry_len + read].copy_from_slice(&buf[..read]);
        let hay_len = carry_len + read;

        scan_window(&hay[..hay_len], false, &mut ids, &mut secrets);

        // 留尾巴给下一块搭桥：跨块的模式（比如后缀被切开）下一轮才认得出来
        carry_len = hay_len.min(SCAN_CARRY_BYTES);
        let start = hay_len - carry_len;
        hay.copy_within(start..hay_len, 0);
    }

    // 收尾：文件末尾没有「下一块」了，最后这点尾巴里被跳过的命中（怕截断而跳的）这里补扫
    if carry_len > 0 {
        let tail = hay[..carry_len].to_vec();
        scan_window(&tail, true, &mut ids, &mut secrets);
    }

    Ok((ids, secrets))
}

/// 在一段字节里找两类模式。
///
/// `eof = false` 时，靠近末尾的命中可能被截断（读到一半），跳过它们留给下一轮；
/// `eof = true` 表示这段就是文件结尾，命中多长算多长。
fn scan_window(hay: &[u8], eof: bool, ids: &mut Vec<String>, secrets: &mut Vec<String>) {
    // ---- client_id：`<10~14 位数字>-<20~40 位小写字母数字>.apps.googleusercontent.com`
    let mut from = 0usize;
    while let Some(rel) = memchr::memmem::find(&hay[from..], CLIENT_ID_SUFFIX) {
        let dot = from + rel;
        // 模式向左最多 55 字节（14 数字 + 1 短横 + 40 字母数字）
        let lo = dot.saturating_sub(80);
        if let Some(offset) = client_id_start(&hay[lo..dot]) {
            // 注意：client_id 是**整串**（含 .apps.googleusercontent.com 后缀）——
            // 换 token 时要拿它去比 client_id，少了后缀 Google 只会回 unauthorized_client。
            let end = dot + CLIENT_ID_SUFFIX.len();
            if let Ok(client_id) = std::str::from_utf8(&hay[lo + offset..end]) {
                let client_id = client_id.to_string();
                if !ids.contains(&client_id) {
                    ids.push(client_id);
                }
            }
        }
        from = dot + CLIENT_ID_SUFFIX.len();
    }

    // ---- secret：`GOCSPX-` + `[A-Za-z0-9_-]{10,300}`（相邻字符串常量没有分隔符，会多吃一点）
    let mut from = 0usize;
    while let Some(rel) = memchr::memmem::find(&hay[from..], SECRET_PREFIX) {
        let start = from + rel;
        let mut end = start + SECRET_PREFIX.len();
        while end < hay.len() && end - start < 7 + 300 && is_secret_byte(hay[end]) {
            end += 1;
        }
        // 只有「被窗口末尾切断」才算可能不完整，留给下一轮（搭桥区会补齐）；
        // 撞到 300 上限的说明模式本身就吃完了（正则的 {10,300} 到这儿就是终点），不是截断。
        let hit_cap = end - start >= 7 + 300;
        if !eof && !hit_cap && end == hay.len() {
            from = start + 1;
            continue;
        }
        let run = &hay[start..end];
        // 二进制里相邻的字符串常量没有分隔符，贪婪匹配会把后面的标识符吞进来
        // （例如 GOCSPX-… 后面紧跟着 ANTIGRAVITY_OAUTH_CLIENTS）。Google 的 secret 是
        // GOCSPX- + 28 字符 = 35；保险起见把常见长度都放进候选，换 token 时逐个试，谁成功算谁。
        for len in [35usize, 42, 47, run.len()] {
            if run.len() >= len && len > SECRET_PREFIX.len() {
                let candidate = String::from_utf8_lossy(&run[..len]).to_string();
                if !secrets.contains(&candidate) {
                    secrets.push(candidate);
                }
            }
        }
        from = start + 1;
    }
}

/// 从「后缀左边的窗口」里找出 local part 的起始下标：`\d{10,14}-[a-z0-9]{20,40}`。
/// 从窗口最左边开始试，第一个成立且**恰好顶到窗口末尾**的就算命中 ——
/// 和 JS 那个正则的扫描顺序一致（正则要求 local part 紧接着 `.apps...`）。
fn client_id_start(window: &[u8]) -> Option<usize> {
    for start in 0..window.len() {
        let rest = &window[start..];
        // 数字段：10~14 位
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if !(10..=14).contains(&digits) {
            continue;
        }
        if rest.get(digits) != Some(&b'-') {
            continue;
        }
        let tail = &rest[digits + 1..];
        let alnum = tail
            .iter()
            .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            .count();
        if !(20..=40).contains(&alnum) {
            continue;
        }
        // 模式必须正好用完窗口（窗口的最后一字节就是后缀前面那个字节），
        // 否则就是更长的串，不是我们要的 client_id
        if alnum == tail.len() {
            return Some(start);
        }
    }
    None
}

fn is_secret_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// 扫一次候选（进程内只扫一次；失败不缓存，下次还能重试）。
pub async fn load_oauth_candidates() -> Result<OauthCandidates, String> {
    // 分块扫是阻塞 IO（一百多 MB），丢到 blocking 线程池里，别占住 async 执行器
    tokio::task::spawn_blocking(|| {
        let paths = existing_binaries();
        extract_candidates_from_binary(&paths)
    })
    .await
    .map_err(|e| format!("扫描任务失败：{e}"))?
}

/// 读老桥的账号库（只读）。返回的 `Account` 里带 refresh_token，调用方不要打印。
pub fn load_bridge_accounts() -> Result<(Vec<Account>, Option<String>), String> {
    let root = accounts_root().ok_or_else(|| "找不到用户目录".to_string())?;
    let index_path = root.join("accounts.json");
    let index: serde_json::Value = match std::fs::read_to_string(&index_path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
        Err(_) => serde_json::Value::Null,
    };
    let current_id = index
        .get("current_account_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let dir = root.join("accounts");
    let mut accounts = Vec::new();
    let entries =
        std::fs::read_dir(&dir).map_err(|e| format!("读不到账号目录 {}：{e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // 单个账号文件坏了不影响其它账号
        let Ok(raw) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let id = raw
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let email = raw
            .get("email")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let token = raw.get("token").cloned().unwrap_or(serde_json::Value::Null);
        accounts.push(Account {
            id: id.clone(),
            email_masked: mask_email(email.as_deref().unwrap_or_default()),
            email,
            project: token
                .get("project_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            refresh_token: token
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            disabled: raw
                .get("disabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            validation_blocked: raw
                .get("validation_blocked")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            is_current: current_id.as_deref() == Some(id.as_str()) && !id.is_empty(),
        });
    }
    Ok((accounts, current_id))
}

/// 挑一个账号：给了邮箱就找它（大小写不敏感），否则用账号库里标记为当前的，再否则第一个可用的。
pub fn pick_account(accounts: &[Account], email: Option<&str>) -> Result<Account, String> {
    let usable: Vec<&Account> = accounts
        .iter()
        .filter(|a| a.refresh_token.is_some())
        .collect();
    if usable.is_empty() {
        return Err("桥的账号文件里没有可用 refresh_token".to_string());
    }
    if let Some(want) = email {
        let want_lower = want.to_lowercase();
        let hit = usable
            .iter()
            .find(|a| a.email.as_deref().map(|e| e.to_lowercase()) == Some(want_lower.clone()));
        return match hit {
            Some(a) => Ok((*a).clone()),
            None => Err(format!("账号列表里没有 {}", mask_email(want))),
        };
    }
    let current = usable.iter().find(|a| a.is_current).copied();
    Ok(current.unwrap_or(usable[0]).clone())
}

/// 一次 refresh_token 交换。失败时只带出 Google 的错误码，不带任何 token 内容。
pub async fn refresh_access_token(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<(String, u64), (String, String)> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("client_id", client_id.to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret.to_string()));
    }

    let res = client
        .post(TOKEN_URL)
        .form(&form)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| ("network_error".to_string(), e.to_string()))?;

    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);

    if !(200..300).contains(&status) || json.get("access_token").is_none() {
        let code = json
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("http_{status}"));
        let description = json
            .get("error_description")
            .and_then(|v| v.as_str())
            .map(|d| d.chars().take(120).collect::<String>())
            .unwrap_or_default();
        return Err((code, description));
    }

    Ok((
        json["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        json.get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600),
    ))
}

/// 按「client_id × secret 候选」逐个试，第一个成功的就是当初签发这个 refresh_token 的那一对。
/// 试过的组合只记录指纹和错误码。
pub async fn exchange_with_candidates(
    client: &reqwest::Client,
    refresh_token: &str,
    candidates: &OauthCandidates,
) -> Result<Exchange, String> {
    let mut attempts: Vec<CredentialAttempt> = Vec::new();
    let mut secrets: Vec<String> = candidates.secrets.clone();
    secrets.push(String::new()); // 有的客户端根本没 secret

    for client_id in &candidates.client_ids {
        for secret in &secrets {
            match refresh_access_token(client, client_id, secret, refresh_token).await {
                Ok((access_token, expires_in)) => {
                    attempts.push(CredentialAttempt {
                        client_id: fingerprint(client_id),
                        secret: fingerprint(secret),
                        ok: true,
                        code: None,
                    });
                    return Ok(Exchange {
                        access_token,
                        expires_in,
                        client_id: client_id.clone(),
                        client_secret: secret.clone(),
                        attempts,
                    });
                }
                Err((code, _)) => attempts.push(CredentialAttempt {
                    client_id: fingerprint(client_id),
                    secret: fingerprint(secret),
                    ok: false,
                    code: Some(code),
                }),
            }
        }
    }

    let codes: Vec<String> = attempts
        .iter()
        .map(|a| format!("{}:{}", a.secret, a.code.clone().unwrap_or_default()))
        .collect();
    Err(format!(
        "所有 client 凭据组合都换不到 token（{}）",
        codes.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join("antigravity-core-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        path
    }

    const CLIENT_ID: &str =
        "123456789012-abcdefghijklmnopqrstuvwx0123456789.apps.googleusercontent.com";
    const SECRET: &str = "GOCSPX-abcdefghijklmnopqrstuvwxyz";

    fn build_file(size: usize, placements: &[(usize, &str)]) -> Vec<u8> {
        let mut buf = vec![b'A'; size];
        for (offset, text) in placements {
            buf[*offset..*offset + text.len()].copy_from_slice(text.as_bytes());
        }
        buf
    }

    #[test]
    fn mask_email_masks_most_of_the_local_part() {
        assert_eq!(
            mask_email("omarcuto123321@gmail.com"),
            "om************@gmail.com"
        );
        assert_eq!(mask_email("ab@x.com"), "ab*@x.com");
        // 没有 @ 的字符串：JS 的 split('@') 拿不到两段，会走 domain = undefined 的分支
        assert_eq!(mask_email("没有at符号的字符串"), "<unknown>");
    }

    #[test]
    fn fingerprint_is_stable_and_short() {
        assert_eq!(fingerprint("abc").len(), 16);
        assert_eq!(fingerprint("abc"), fingerprint("abc"));
        assert_ne!(fingerprint("abc"), fingerprint("abd"));
    }

    #[test]
    fn scan_finds_client_id_and_secret_across_chunk_boundary() {
        // 块小到 4 KB，让 client_id 横跨两块边界，secret 放在文件最末尾
        let chunk = 4096;
        let data = build_file(
            chunk + 64 * 1024,
            &[(chunk - 20, CLIENT_ID), (chunk + 32 * 1024, SECRET)],
        );
        let path = write_temp("fake-bin-1.bin", &data);

        let found = extract_candidates_with_chunk(std::slice::from_ref(&path), chunk).unwrap();
        assert_eq!(found.client_ids, vec![CLIENT_ID.to_string()]);
        // 填充字节是 'A'，会被 GOCSPX- 的贪婪匹配吞掉一部分，所以只要求「有个候选用真 secret 开头」
        assert!(
            found.secrets.iter().any(|s| s.starts_with(SECRET)),
            "secret 候选要抽到：{:?}",
            found.secrets
        );
        assert_eq!(found.sources.len(), 1);
    }

    #[test]
    fn scan_reports_end_of_file_secret_through_scan_window() {
        let mut data = vec![b'B'; 128];
        data.extend_from_slice(SECRET.as_bytes());
        let mut ids = Vec::new();
        let mut secrets = Vec::new();
        scan_window(&data, true, &mut ids, &mut secrets);
        assert!(secrets.iter().any(|s| s.starts_with(SECRET)), "{secrets:?}");
    }

    #[test]
    fn scan_rejects_bad_client_id_shapes() {
        let mut ids = Vec::new();
        let mut secrets = Vec::new();
        // 数字段只有 9 位 → 不算
        let bad = "123456789-abcdefghijklmnopqrstuvwx.apps.googleusercontent.com";
        scan_window(bad.as_bytes(), true, &mut ids, &mut secrets);
        assert!(ids.is_empty(), "{ids:?}");
        // 字母数字段只有 10 位（要求 20~40）→ 不算
        let bad2 = "123456789012-abcdefghij.apps.googleusercontent.com";
        scan_window(bad2.as_bytes(), true, &mut ids, &mut secrets);
        assert!(ids.is_empty(), "{ids:?}");
        // 字母数字段 41 位（超了）→ 不算
        let bad3 = format!(
            "{}-abcdefghijklmnopqrstuvwxyzabcdefghijklmno.apps.googleusercontent.com",
            "123456789012"
        );
        scan_window(bad3.as_bytes(), true, &mut ids, &mut secrets);
        assert!(ids.is_empty(), "{ids:?}");
        // 正常形状 → 抽到，而且是**整串**（含后缀）
        scan_window(CLIENT_ID.as_bytes(), true, &mut ids, &mut secrets);
        assert_eq!(ids, vec![CLIENT_ID.to_string()]);
        assert!(ids[0].ends_with(".apps.googleusercontent.com"));
    }

    #[test]
    fn scan_errors_when_nothing_found() {
        let path = write_temp("junk.bin", b"nothing to see here");
        let err = extract_candidates_with_chunk(&[path], 4096).unwrap_err();
        assert!(err.contains("没抽到 client_id"), "{err}");
        // 错误信息里要能看到找过哪些路径（Windows 上装机位置不同时靠这个补路径）
        assert!(err.contains("junk.bin"), "{err}");
    }

    #[test]
    fn candidate_binaries_prefers_env_override() {
        std::env::set_var("ANTIGRAVITY_OFFICIAL_BIN", "/tmp/自定义/language_server");
        let list = candidate_binaries();
        std::env::remove_var("ANTIGRAVITY_OFFICIAL_BIN");
        assert_eq!(list[0].display().to_string(), "/tmp/自定义/language_server");
    }

    #[test]
    fn pick_account_prefers_current_then_email() {
        let accounts = vec![
            Account {
                id: "a".into(),
                email: Some("a@x.com".into()),
                email_masked: mask_email("a@x.com"),
                refresh_token: Some("r".into()),
                ..Default::default()
            },
            Account {
                id: "b".into(),
                email: Some("b@x.com".into()),
                email_masked: mask_email("b@x.com"),
                refresh_token: Some("r".into()),
                is_current: true,
                ..Default::default()
            },
            Account {
                id: "c".into(),
                email: Some("c@x.com".into()),
                email_masked: mask_email("c@x.com"),
                refresh_token: None,
                ..Default::default()
            },
        ];
        assert_eq!(pick_account(&accounts, None).unwrap().id, "b");
        assert_eq!(pick_account(&accounts, Some("A@X.COM")).unwrap().id, "a");
        // 没有 refresh_token 的账号不算可用
        assert!(pick_account(&accounts, Some("c@x.com")).is_err());
        assert!(pick_account(&accounts, Some("nobody@x.com")).is_err());
    }

    /// 只用一次的临时目录（单测不碰真实家目录）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agb-oauth-{tag}-{}-{}",
            std::process::id(),
            now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一份老格式的账号库（索引 + 账号文件）。
    fn write_accounts(root: &Path, id: &str, email: &str) {
        std::fs::create_dir_all(root.join("accounts")).unwrap();
        std::fs::write(
            root.join("accounts.json"),
            format!(r#"{{"accounts":[{{"id":"{id}"}}],"current_account_id":"{id}"}}"#),
        )
        .unwrap();
        std::fs::write(
            root.join("accounts").join(format!("{id}.json")),
            format!(r#"{{"id":"{id}","email":"{email}","token":{{"refresh_token":"rt-{id}"}}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn accounts_live_in_our_own_dir_once_we_have_them() {
        let own = temp_dir("own");
        let legacy = temp_dir("legacy");
        write_accounts(&own, "mine", "me@example.com");
        write_accounts(&legacy, "old", "old@example.com");
        assert_eq!(
            resolve_accounts_root(&own, Some(&legacy)),
            own,
            "自己这边已经有账号库，就该用自己的"
        );
    }

    #[test]
    fn legacy_accounts_are_copied_once_and_originals_stay_put() {
        let own = temp_dir("migrate-own");
        let legacy = temp_dir("migrate-legacy");
        write_accounts(&legacy, "old", "old@example.com");

        assert_eq!(
            resolve_accounts_root(&own, Some(&legacy)),
            own,
            "迁完就用自己这份"
        );
        assert!(own.join("accounts.json").is_file(), "索引要拷过去");
        let copied = own.join("accounts").join("old.json");
        assert!(copied.is_file(), "账号文件要拷过去");
        assert!(legacy.join("accounts.json").is_file(), "老目录只读、不动");

        // 第二次来：自己的已经有了，不再碰老目录（老工具删掉也照样起）
        std::fs::remove_dir_all(&legacy).unwrap();
        assert_eq!(resolve_accounts_root(&own, Some(&legacy)), own);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&copied).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "账号文件里有 refresh_token，只能自己可读");
        }
    }

    #[test]
    fn missing_legacy_accounts_leaves_our_own_dir_empty_but_chosen() {
        let own = temp_dir("empty-own");
        let legacy = temp_dir("empty-legacy");
        assert_eq!(resolve_accounts_root(&own, Some(&legacy)), own);
        assert_eq!(resolve_accounts_root(&own, None), own);
        assert!(!own.join("accounts.json").exists(), "没东西可迁就不写文件");
    }
}
