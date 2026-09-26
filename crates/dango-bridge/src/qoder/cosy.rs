//! COSY 签名：Qoder 网关（`gateway.qoder.com.cn`）对 `/algo/*` 请求要的那组头。
//!
//! 语义照抄 PoC `/tmp/qoder-poc/qoder-client.mjs` 的 `buildAuthHeaders`，并与 MIT 参考项目
//! `simonsmh/pi-provider-qoder/src/cosy.ts` 互证：
//!
//! ```text
//! aesKey   = uuid v4 去横线取前 16 个字节（ASCII）
//! info     = base64(AES-128-CBC(key = IV = aesKey)({uid,aid:"",name,email,security_oauth_token}))
//! cosyKey  = base64(RSA_PKCS1v15(内置 1024 位公钥)(aesKey))
//! meta     = base64({version:"v1",requestId,info,cosyVersion:"1.1.38",ideVersion:""})
//! sigPath  = pathname 去掉前导 "/algo"
//! sig      = md5_hex(meta + "\n" + cosyKey + "\n" + 秒级时间戳 + "\n" + body(编码后) + "\n" + sigPath)
//! Authorization: Bearer COSY.<meta>.<sig>
//!
//! 例外：图片上传（`/algo/api/v2/image/upload`）用同一套头，但 sig 里参与的是 **body 的
//! 十进制长度字符串**而不是字节本身（见 [`build_upload_headers`]）。
//! ```
//!
//! 任何 token / 签名都不进日志：本模块只返回头，错误信息里只提「ui d 为空 / job token 为空」这类形态。

use base64::Engine;
use md5::{Digest, Md5};
use rsa::pkcs8::DecodePublicKey;
use rsa::rand_core::OsRng;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use serde_json::json;

use super::{CLIENT_TYPE, COSY_VERSION};

/// 网关内置的 RSA 公钥（PoC 与参考项目里同一把；只用来把 aesKey 交给服务端）。
const RSA_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDA8iMH5c02LilrsERw9t6Pv5Nc
4k6Pz1EaDicBMpdpxKduSZu5OANqUq8er4GM95omAGIOPOh+Nx0spthYA2BqGz+l
6HRkPJ7S236FZz73In/KVuLnwI8JJ2CbuJap8kvheCCZpmAWpb/cPx/3Vr/J6I17
XcW+ML9FoCI6AOvOzwIDAQAB
-----END PUBLIC KEY-----";

/// 一次签名要用的身份凭据。全部是借来的引用，本结构不持有任何秘密。
pub struct CosyCredentials<'a> {
    /// 上游拿到的真实 uid（空的话网关会 105 Login expired）
    pub user_id: &'a str,
    /// job token（`jt-…`）；字段名沿用 COSY 协议里的 `security_oauth_token`
    pub auth_token: &'a str,
    pub name: &'a str,
    pub email: &'a str,
    pub machine_id: &'a str,
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn md5_hex(bytes: &[u8]) -> String {
    to_hex(&Md5::digest(bytes))
}

/// `pathname` 去前导 `/algo`（等价于 JS `new URL(url).pathname` 后再 `slice("/algo".length)`）。
fn sig_path_of(request_url: &str) -> String {
    let after_scheme = request_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(request_url);
    let path_and_query = match after_scheme.find('/') {
        Some(index) => &after_scheme[index..],
        None => "/",
    };
    let path = path_and_query.split(['?', '#']).next().unwrap_or("/");
    path.strip_prefix("/algo").unwrap_or(path).to_string()
}

/// macOS 上的官方 CLI 也把自己报成 `*_linux`（PoC 实测），所以这里按架构分、桌面系统一律走非 windows 分支。
fn machine_os() -> &'static str {
    if cfg!(target_os = "windows") {
        if cfg!(target_arch = "aarch64") {
            "aarch64_windows"
        } else {
            "x86_64_windows"
        }
    } else if cfg!(target_arch = "aarch64") {
        "aarch64_linux"
    } else {
        "x86_64_linux"
    }
}

/// AES-128-CBC（PKCS7 补齐，key = IV）加密，返回密文字节。
fn aes_encrypt_cbc(plaintext: &[u8], key: &[u8]) -> Result<Vec<u8>, String> {
    use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    let mut buf = vec![0u8; plaintext.len() + 16];
    buf[..plaintext.len()].copy_from_slice(plaintext);
    let cipher = Aes128CbcEnc::new_from_slices(key, key)
        .map_err(|_| "AES key 必须是 16 字节".to_string())?;
    let out = cipher
        .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
        .map_err(|_| "AES-CBC 补齐失败".to_string())?;
    Ok(out.to_vec())
}

/// 签名规范串的 md5（十六进制小写）。抽出来是为了单测能钉死拼接顺序。
pub(crate) fn signature(
    meta_b64: &str,
    cosy_key: &str,
    timestamp: &str,
    body: &[u8],
    sig_path: &str,
) -> String {
    let mut hasher = Md5::new();
    hasher.update(meta_b64.as_bytes());
    hasher.update(b"\n");
    hasher.update(cosy_key.as_bytes());
    hasher.update(b"\n");
    hasher.update(timestamp.as_bytes());
    hasher.update(b"\n");
    hasher.update(body);
    hasher.update(b"\n");
    hasher.update(sig_path.as_bytes());
    to_hex(&hasher.finalize())
}

/// 造一整套 COSY 头。`body` 必须是**已编码**的请求体字节；GET 传空切片。
///
/// 返回 `Vec<(name, value)>` 而不是 `HeaderMap`：调用方要原样拼进 reqwest，且测试要按顺序比对。
pub fn build_auth_headers(
    body: &[u8],
    request_url: &str,
    creds: &CosyCredentials<'_>,
) -> Result<Vec<(String, String)>, String> {
    build_headers_with_sig_body(body, body, request_url, creds)
}

/// 上传接口（`/algo/api/v2/image/upload`）的 COSY 头：和聊天同一套，**唯一区别是参与 md5 的
/// 是 body 的十进制长度字符串**，不是字节本身。
///
/// 为什么：实测（2026-09-19，真网关，CLI 1.1.38 / app 1.1.53 两个变体都一样）——按字节签回
/// `403 {"code":"101","message":"Signature invalid"}`，按长度串签回 200。多半是流式上传不在
/// 服务端缓冲 body，只能按声明的长度验签。`Cosy-Bodylength` / `Cosy-Bodyhash` 仍旧报真实 body
/// （长度/hash 报真值或报长度串本身都试过，服务端只认长度串那一处）。
pub fn build_upload_headers(
    body: &[u8],
    request_url: &str,
    creds: &CosyCredentials<'_>,
) -> Result<Vec<(String, String)>, String> {
    let len = body.len().to_string();
    build_headers_with_sig_body(len.as_bytes(), body, request_url, creds)
}

fn build_headers_with_sig_body(
    sig_body: &[u8],
    body: &[u8],
    request_url: &str,
    creds: &CosyCredentials<'_>,
) -> Result<Vec<(String, String)>, String> {
    if creds.user_id.is_empty() {
        return Err("cosy: uid 为空（网关会报 105 Login expired）".to_string());
    }
    if creds.auth_token.is_empty() {
        return Err("cosy: job token 为空".to_string());
    }

    let uuid = uuid::Uuid::new_v4().simple().to_string();
    let aes_key = &uuid[..16];

    let info = json!({
        "uid": creds.user_id,
        "aid": "",
        "name": creds.name,
        "email": creds.email,
        "security_oauth_token": creds.auth_token,
    });
    let ciphertext = aes_encrypt_cbc(
        serde_json::to_string(&info)
            .map_err(|e| format!("序列化 info 失败：{e}"))?
            .as_bytes(),
        aes_key.as_bytes(),
    )?;
    let info_b64 = base64::engine::general_purpose::STANDARD.encode(ciphertext);

    let public_key = RsaPublicKey::from_public_key_pem(RSA_PUBLIC_KEY)
        .map_err(|e| format!("解析公钥失败：{e}"))?;
    let encrypted_key = public_key
        .encrypt(&mut OsRng, Pkcs1v15Encrypt, aes_key.as_bytes())
        .map_err(|e| format!("RSA 加密 aesKey 失败：{e}"))?;
    let cosy_key = base64::engine::general_purpose::STANDARD.encode(encrypted_key);

    let meta = json!({
        "version": "v1",
        "requestId": uuid::Uuid::new_v4().to_string(),
        "info": info_b64,
        "cosyVersion": COSY_VERSION,
        "ideVersion": "",
    });
    let meta_b64 = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_vec(&meta).map_err(|e| format!("序列化 meta 失败：{e}"))?);

    let timestamp = (crate::types::now_millis() / 1000).to_string();
    let sig_path = sig_path_of(request_url);
    let sig = signature(&meta_b64, &cosy_key, &timestamp, sig_body, &sig_path);

    let machine_id = if creds.machine_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        creds.machine_id.to_string()
    };
    let body_hash = md5_hex(body);

    Ok(vec![
        (
            "Authorization".to_string(),
            format!("Bearer COSY.{meta_b64}.{sig}"),
        ),
        ("Cosy-Key".to_string(), cosy_key),
        ("Cosy-User".to_string(), creds.user_id.to_string()),
        ("Cosy-Date".to_string(), timestamp),
        ("Cosy-Version".to_string(), COSY_VERSION.to_string()),
        ("Cosy-Machineid".to_string(), machine_id.clone()),
        ("Cosy-Machinetoken".to_string(), machine_id),
        ("Cosy-Machinetype".to_string(), "5".to_string()),
        ("Cosy-Machineos".to_string(), machine_os().to_string()),
        ("Cosy-Clienttype".to_string(), CLIENT_TYPE.to_string()),
        ("Cosy-Clientip".to_string(), "127.0.0.1".to_string()),
        ("Cosy-Bodyhash".to_string(), body_hash),
        ("Cosy-Bodylength".to_string(), body.len().to_string()),
        ("Cosy-Sigpath".to_string(), sig_path),
        ("Cosy-Data-Policy".to_string(), "disagree".to_string()),
        ("Cosy-Organization-Id".to_string(), String::new()),
        ("Cosy-Organization-Tags".to_string(), String::new()),
        ("Login-Version".to_string(), "v2".to_string()),
        ("X-Request-Id".to_string(), uuid::Uuid::new_v4().to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sig_path_strips_leading_algo_and_query() {
        assert_eq!(
            sig_path_of("https://gateway.qoder.com.cn/algo/api/v2/model/list?Encode=1"),
            "/api/v2/model/list"
        );
        assert_eq!(
            sig_path_of("https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1"),
            "/api/v2/service/pro/sse/agent_chat_generation"
        );
        // 不带 /algo 的路径原样保留
        assert_eq!(
            sig_path_of("https://openapi.qoder.com.cn/api/v1/userinfo"),
            "/api/v1/userinfo"
        );
        // 只有 host、没有路径
        assert_eq!(sig_path_of("https://gateway.qoder.com.cn"), "/");
    }

    #[test]
    fn upload_headers_sign_the_length_string() {
        let creds = CosyCredentials {
            user_id: "u-1",
            auth_token: "jt-x",
            name: "n",
            email: "e",
            machine_id: "m-1",
        };
        let url = "https://gateway.qoder.com.cn/algo/api/v2/image/upload?request_id=abc";
        let body = b"0123456789"; // 长度串就是 "10"
        let headers = build_upload_headers(body, url, &creds).unwrap();
        let get = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        // 报给服务端的长度/hash 仍是真实 body
        assert_eq!(get("Cosy-Bodylength"), "10");
        assert_eq!(get("Cosy-Bodyhash"), md5_hex(body));
        // 签名必须等于"用 '10' 当 body"重算出来的那个
        let auth = get("Authorization");
        let mut parts = auth.trim_start_matches("Bearer COSY.").rsplitn(2, '.');
        let sig = parts.next().unwrap().to_string();
        let meta_b64 = parts.next().unwrap().to_string();
        assert_eq!(
            sig,
            signature(
                &meta_b64,
                &get("Cosy-Key"),
                &get("Cosy-Date"),
                b"10",
                "/api/v2/image/upload"
            )
        );
        // 用字节本身签是另一个值（正是线上被回 101 的那种）
        assert_ne!(
            sig,
            signature(
                &meta_b64,
                &get("Cosy-Key"),
                &get("Cosy-Date"),
                body,
                "/api/v2/image/upload"
            )
        );
    }

    #[test]
    fn signature_concatenation_order_is_pinned() {
        // 期望值由 PoC 的 JS（md5(meta+"\n"+cosyKey+"\n"+ts+"\n"+body+"\n"+path)）生成后硬编码
        assert_eq!(
            signature(
                "TUVUQQ==",
                "Q09TWUtFWQ==",
                "1700000000",
                b"BODY",
                "/api/v2/model/list"
            ),
            "8c9ef630aa67a81a805bedb4ffe76360"
        );
        // 空 body（GET 的 model/list / userinfo 走这条）
        assert_eq!(
            signature("AA==", "BB==", "1", b"", "/api/v1/userinfo"),
            "a7136b56e7e7bdee35e54d08ab527435"
        );
        // 二进制 body（含 0x00/0xff），拼接必须按字节而不是字符串
        assert_eq!(
            signature(
                "bWV0YQ==",
                "Y29zeQ==",
                "1700000001",
                &[0u8, 255, 16, 32],
                "/api/v2/service/pro/sse/agent_chat_generation"
            ),
            "d3bc993bb746582239dfea490e2a0ecd"
        );
    }

    #[test]
    fn build_headers_shape_and_no_token_in_headers() {
        let creds = CosyCredentials {
            user_id: "uid-123",
            auth_token: "jt-super-secret",
            name: "名字",
            email: "a@b.com",
            machine_id: "machine-1",
        };
        let headers = build_auth_headers(
            b"",
            "https://gateway.qoder.com.cn/algo/api/v2/model/list?Encode=1",
            &creds,
        )
        .expect("签名");

        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("Cosy-User"), Some("uid-123"));
        assert_eq!(get("Cosy-Clienttype"), Some("5"));
        assert_eq!(get("Cosy-Sigpath"), Some("/api/v2/model/list"));
        assert_eq!(get("Cosy-Bodylength"), Some("0"));
        assert_eq!(get("Cosy-Bodyhash"), Some(md5_hex(b"").as_str()));
        assert_eq!(get("Login-Version"), Some("v2"));
        assert_eq!(get("Cosy-Data-Policy"), Some("disagree"));
        let authorization = get("Authorization").expect("Authorization");
        assert!(authorization.starts_with("Bearer COSY."), "签名头形状不对");
        // Bearer COSY.<meta>.<sig>：两个点
        assert_eq!(authorization.matches('.').count(), 2);
        // job token 只应出现在加密的 info 段里，绝不能以明文出现在任何头值上
        assert!(
            !headers.iter().any(|(_, v)| v.contains("jt-super-secret")),
            "token 不许出现在头里"
        );
        // meta（Authorization 的第二段）不该是明文 JSON
        let meta_b64 = authorization.split('.').nth(1).unwrap();
        assert!(!meta_b64.contains("uid-123"));
    }

    #[test]
    fn empty_uid_or_token_is_a_clear_error() {
        let no_uid = CosyCredentials {
            user_id: "",
            auth_token: "jt",
            name: "",
            email: "",
            machine_id: "m",
        };
        assert!(build_auth_headers(b"", "https://x/algo/y", &no_uid)
            .unwrap_err()
            .contains("uid"));
        let no_token = CosyCredentials {
            user_id: "u",
            auth_token: "",
            name: "",
            email: "",
            machine_id: "m",
        };
        assert!(build_auth_headers(b"", "https://x/algo/y", &no_token)
            .unwrap_err()
            .contains("job token"));
    }

    #[test]
    fn rsa_key_parses_and_aes_roundtrips_through_length() {
        // 公钥能解析 + RSA 密文长度等于模长（1024 位 = 128 字节），防止把 PEM 抄坏
        let key = RsaPublicKey::from_public_key_pem(RSA_PUBLIC_KEY).expect("公钥");
        use rsa::traits::PublicKeyParts;
        assert_eq!(key.size(), 128);
        // AES-CBC PKCS7：16 字节明文 → 32 字节密文
        let out = aes_encrypt_cbc(b"0123456789abcdef", b"0123456789abcdef").expect("加密");
        assert_eq!(out.len(), 32);
        // 空明文 → 也有一个补齐块
        assert_eq!(aes_encrypt_cbc(b"", b"0123456789abcdef").unwrap().len(), 16);
    }
}
