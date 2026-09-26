//! 图片上传：客户端给的 base64 图先进 Qoder 图床，拿回一个带签名的 URL，聊天请求里再用
//! `{"type":"image_url","image_url":{"url":…}}` 引用它。
//!
//! 协议（2026-09-19 在真网关上打通，CLI 1.1.38 / app 1.1.53 两个变体都验过）：
//!
//! 1. `PUT {gateway}/algo/api/v2/image/upload?request_id=<32hex>`，body 是
//!    `multipart/form-data`（一个 `file` 字段，filename `image.<ext>`）。签名和聊天同一套
//!    COSY 头，但参与 md5 的是 **body 长度的十进制字符串**（见 [`super::cosy::build_upload_headers`]）。
//! 2. 回 `{"result":{"url":"https://qoder-cn-vl-private….png?Expires=…&Signature=…"}}`：
//!    带签名的私有桶 URL，实测有效期约 30 天。
//! 3. 聊天请求里 `content` 写成 part 数组，图片那段是 **OpenAI 形状**
//!    （`{"type":"image_url","image_url":{"url":…}}`）。其余三种形状实测**都被静默忽略**
//!    （模型回「没有图片」、`prompt_tokens` 也不涨）：Anthropic 的
//!    `{"type":"image","source":{…}}`、顶层 `image_urls` / `chat_context.imageUrls` 字段、
//!    正文里的 markdown 图链。
//!
//! 路径必须带 `/algo`：不带的那条 `/api/v2/image/upload` 会被 ALB 挡成 503（网关根本没收到）。

use serde_json::Value;

use super::cosy::{self, CosyCredentials};

/// 单张图上限：超过就不传（调用方按「丢图 + 警告」处理；别把上百 MB 的 base64 再打一遍）。
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// 上传接口的 URL（`request_id` 是 32 位十六进制，和桌面端一致）。
pub fn upload_url(gateway: &str) -> String {
    format!(
        "{}/algo/api/v2/image/upload?request_id={}",
        gateway.trim_end_matches('/'),
        uuid::Uuid::new_v4().simple()
    )
}

/// mime → 图床文件名后缀（桌面端也是 `image.<ext>`）。
pub fn ext_of(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/heic" | "image/heif" => "heic",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

/// 拼 multipart body（只有一段 `file`）。`boundary` 由调用方给，方便测试钉住字节。
pub fn multipart_body(bytes: &[u8], mime_type: &str, boundary: &str) -> Vec<u8> {
    let head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"image.{}\"\r\nContent-Type: {mime_type}\r\n\r\n",
        ext_of(mime_type)
    );
    let mut body = Vec::with_capacity(head.len() + bytes.len() + boundary.len() + 8);
    body.extend_from_slice(head.as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// 从上传响应里抠 URL（`{"result":{"url":…}}`）。
pub fn url_from_response(value: &Value) -> Option<String> {
    value
        .get("result")?
        .get("url")?
        .as_str()
        .map(str::to_string)
        .filter(|url| !url.is_empty())
}

/// 上传一张图，回图床 URL。错误信息只进日志，**不带** URL / 签名。
pub async fn upload_image(
    client: &reqwest::Client,
    gateway: &str,
    creds: &CosyCredentials<'_>,
    bytes: &[u8],
    mime_type: &str,
) -> Result<String, String> {
    if bytes.is_empty() {
        return Err("图片是空的".to_string());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "图片太大（{} KB > {} KB）",
            bytes.len() / 1024,
            MAX_IMAGE_BYTES / 1024
        ));
    }
    let boundary = format!("----qoderbridge{}", uuid::Uuid::new_v4().simple());
    let body = multipart_body(bytes, mime_type, &boundary);
    let url = upload_url(gateway);
    let headers = cosy::build_upload_headers(&body, &url, creds)?;

    let mut request = client
        .put(&url)
        .header("Accept", "application/json")
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(
            "AI-CLIENT-TIMESTAMP",
            (crate::types::now_millis() / 1000).to_string(),
        )
        .body(body);
    for (name, value) in headers {
        request = request.header(name, value);
    }

    let response = request
        .send()
        .await
        .map_err(|err| format!("请求发出失败：{err}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "HTTP {} {}",
            status.as_u16(),
            crate::server::truncate(&text, 160)
        ));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| format!("响应不是 JSON：{}", crate::server::truncate(&text, 120)))?;
    url_from_response(&value).ok_or_else(|| {
        format!(
            "响应里没有 result.url：{}",
            crate::server::truncate(&text, 120)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_url_carries_the_algo_prefix_and_a_request_id() {
        let url = upload_url("https://gateway.qoder.com.cn/");
        assert!(
            url.starts_with("https://gateway.qoder.com.cn/algo/api/v2/image/upload?request_id=")
        );
        let id = url.rsplit('=').next().unwrap();
        assert_eq!(id.len(), 32, "request_id 是 32 位十六进制：{id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn multipart_body_is_the_shape_the_gateway_accepts() {
        let body = multipart_body(b"PNGBYTES", "image/png", "BOUND");
        let expected = concat!(
            "--BOUND\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"image.png\"\r\n",
            "Content-Type: image/png\r\n",
            "\r\n",
            "PNGBYTES",
            "\r\n--BOUND--\r\n",
        );
        assert_eq!(body, expected.as_bytes());
    }

    #[test]
    fn ext_follows_the_mime_type() {
        assert_eq!(ext_of("image/png"), "png");
        assert_eq!(ext_of("image/jpeg"), "jpg");
        assert_eq!(ext_of("image/webp"), "webp");
        assert_eq!(ext_of("application/octet-stream"), "bin");
    }

    #[test]
    fn url_is_read_from_result_url() {
        let value = serde_json::json!({
            "result": { "url": "https://qoder-cn-vl-private.oss-cn-beijing.aliyuncs.com/a.png?Expires=1" }
        });
        assert!(url_from_response(&value).is_some());
        assert!(url_from_response(&serde_json::json!({ "result": {} })).is_none());
        assert!(url_from_response(&serde_json::json!({ "result": { "url": "" } })).is_none());
    }
}
