//! 端到端：用假 CLI 顶替 cursor-agent，起真的 HTTP 服务，用真 socket 说 HTTP/1.0。
//!
//! 为什么是 HTTP/1.0：1.0 不支持 chunked，hyper 只能「发完就关连接」，这样测试里拿到的
//! 就是裸 SSE 文本，断言帧可以直接 `contains`，不用先拆 chunk 头。
//!
//! 覆盖：流式 Anthropic（思考块 + 哨兵签名 + 用量）、流式/非流式 OpenAI、参数透传
//! （`--mode ask`、`--api-key`、prompt 内容）、以及「CLI 什么都没吐」必须报错而不是装成功。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 假 CLI 会吐出来的事件（形状照抄本机实测的 stream-json）。
const EVENTS: &str = r#"{"type":"system","subtype":"init","apiKeySource":"key","model":"Auto","session_id":"e2e-session"}
{"type":"thinking","subtype":"delta","text":"先算一下：","timestamp_ms":1}
{"type":"thinking","subtype":"delta","text":"1+1 显然等于 2。","timestamp_ms":2}
{"type":"thinking","subtype":"completed","timestamp_ms":3}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"1+1 等于 2。"}]}}
{"type":"result","subtype":"success","is_error":false,"result":"1+1 等于 2。","session_id":"e2e-session","usage":{"inputTokens":100,"outputTokens":20,"cacheReadTokens":5,"cacheWriteTokens":0}}
"#;

const ACCOUNTS: &str = r#"{"accounts":[{"name":"day-1","apiKey":"sk-day-1-abcdefgh"}]}"#;

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    dir: PathBuf,
    port: u16,
    _server: ServerGuard,
}

impl Fixture {
    fn args_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("args.txt")).unwrap_or_default()
    }
}

fn write_fake_agent(dir: &Path, script_body: &str) {
    let path = dir.join("fake-agent");
    let mut file = std::fs::File::create(&path).unwrap();
    write!(file, "#!/bin/bash\n{script_body}").unwrap();
    drop(file);
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

async fn start(name: &str, script_body: &str) -> Fixture {
    // `free_port()` 先占再放会有 TOCTOU 竞争（并行测试/残留进程把端口抢走
    // → server bind 失败 → healthz 永远不通）。起服务失败就换个端口重试。
    let mut last_log = String::new();
    for attempt in 0..5 {
        let dir = std::env::temp_dir().join(format!(
            "cursor-bridge-e2e-{}-{}-{attempt}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_fake_agent(&dir, script_body);
        std::fs::write(dir.join("events.jsonl"), EVENTS).unwrap();
        std::fs::write(dir.join("accounts.json"), ACCOUNTS).unwrap();

        let port = free_port();
        let log = std::fs::File::create(dir.join("server.log")).unwrap();
        let server = Command::new(env!("CARGO_BIN_EXE_cursor-bridge"))
            .arg("serve")
            .env("CURSOR_BRIDGE_AGENT_BIN", dir.join("fake-agent"))
            .env("CURSOR_BRIDGE_PORT", port.to_string())
            .env("CURSOR_BRIDGE_STATE_DIR", &dir)
            .env("CURSOR_BRIDGE_ACCOUNTS", dir.join("accounts.json"))
            .env("CURSOR_BRIDGE_WORKSPACE", dir.join("workspace"))
            .env("CURSOR_BRIDGE_KEEPALIVE_SECS", "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("起服务失败");

        let fixture = Fixture {
            dir,
            port,
            _server: ServerGuard(server),
        };

        // 等它准备好：最多 15 秒
        for _ in 0..150 {
            if let Ok((status, _)) = raw_http(port, "GET", "/healthz", None).await {
                if status == 200 {
                    return fixture;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        last_log = std::fs::read_to_string(fixture.dir.join("server.log")).unwrap_or_default();
        // 起不来多半是端口被抢——换端口重试；最后一次才带着日志炸
    }
    panic!("服务没起来（重试 5 次仍失败），日志：\n{last_log}");
}

/// 极简 HTTP 客户端：请求 + 读完整个响应（1.0 → 服务端发完就关）。
async fn raw_http(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> std::io::Result<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await?;
    let (head, body) = match raw.split_once("\r\n\r\n") {
        Some(parts) => parts,
        None => (raw.as_str(), ""),
    };
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    Ok((status, body.to_string()))
}

/// OK 的假 CLI：先记下收到的参数，再把事件原样吐出来。
const FAKE_OK: &str = r#"printf '%s\n' "$@" > "$(dirname "$0")/args.txt"
cat "$(dirname "$0")/events.jsonl"
"#;

/// 什么都不吐、直接报错的假 CLI（模拟没登录）。
const FAKE_DEAD: &str = r#"printf '%s\n' "$@" > "$(dirname "$0")/args.txt"
echo "未登录，请先 agent login" >&2
exit 3
"#;

#[tokio::test]
async fn anthropic_streaming_carries_thinking_with_sentinel_and_usage() {
    let fx = start("anthropic-stream", FAKE_OK).await;

    let (status, _) = raw_http(fx.port, "GET", "/healthz", None).await.unwrap();
    assert_eq!(status, 200);

    let request = r#"{
        "model": "gpt-5.3-codex",
        "stream": true,
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "messages": [{"role": "user", "content": "你好，1+1 等于几？"}]
    }"#;
    let (status, body) = raw_http(fx.port, "POST", "/v1/messages", Some(request))
        .await
        .unwrap();
    assert_eq!(status, 200, "响应体：{body}");
    assert!(
        body.contains("event: message_start") && body.contains("event: message_stop"),
        "SSE 帧不全：{body}"
    );
    assert!(
        body.contains(r#""type":"thinking""#)
            && body.contains(r#""signature":"skip_thought_signature_validator""#),
        "思考块或哨兵签名缺失：{body}"
    );
    assert!(
        body.contains(r#""thinking":"先算一下：""#),
        "思考增量缺失：{body}"
    );
    assert!(
        body.contains(r#""text":"1+1 等于 2。""#),
        "正文缺失：{body}"
    );
    assert!(
        body.contains(r#""usage":{"cache_creation_input_tokens":0"#)
            && body.contains(r#""input_tokens":100"#)
            && body.contains(r#""output_tokens":20"#),
        "用量没折对：{body}"
    );
    // 内容块顺序：思考在前，正文在后
    let thinking_at = body.find(r#""type":"thinking""#).unwrap();
    let text_at = body.find(r#""content_block":{"text"#).unwrap();
    assert!(thinking_at < text_at, "正文块跑到思考块前面了");

    // CLI 真的收到了该收的参数和 prompt
    let args = fx.args_log();
    assert!(args.contains("--print"), "缺少 --print：{args}");
    assert!(args.contains("stream-json"), "缺少 stream-json：{args}");
    assert!(
        args.contains("--stream-partial-output"),
        "缺少增量开关：{args}"
    );
    assert!(args.contains("--mode\nask"), "应当以只读模式跑：{args}");
    assert!(
        args.contains("--api-key\nsk-day-1-abcdefgh"),
        "账号池的 key 没传下去：{args}"
    );
    assert!(args.contains("--workspace"), "缺少 --workspace：{args}");
    assert!(
        args.contains("你好，1+1 等于几？"),
        "prompt 里没有用户的话：{args}"
    );
}

#[tokio::test]
async fn openai_endpoints_stream_and_block() {
    let fx = start("openai", FAKE_OK).await;

    let streaming = r#"{
        "model": "auto",
        "stream": true,
        "reasoning_effort": "high",
        "stream_options": {"include_usage": true},
        "messages": [
            {"role": "system", "content": "你要简洁"},
            {"role": "user", "content": "算一下 1+1"}
        ]
    }"#;
    let (status, body) = raw_http(fx.port, "POST", "/v1/chat/completions", Some(streaming))
        .await
        .unwrap();
    assert_eq!(status, 200, "响应体：{body}");
    assert!(
        body.contains(r#""reasoning_content":"先算一下：""#),
        "思考没映射：{body}"
    );
    assert!(
        body.contains(r#""content":"1+1 等于 2。""#),
        "正文缺失：{body}"
    );
    assert!(
        body.contains(r#""finish_reason":"stop""#),
        "缺 finish_reason：{body}"
    );
    assert!(
        body.contains(r#""prompt_tokens":100"#),
        "用量块缺失：{body}"
    );
    assert!(
        body.trim_end().ends_with("data: [DONE]"),
        "结尾应是 [DONE]：{body}"
    );
    // system 消息要变成 <system> 段，而不是混进用户话里
    let args = fx.args_log();
    assert!(
        args.contains("<system>"),
        "system 提示没拼进 prompt：{args}"
    );

    let blocking = r#"{
        "model": "auto",
        "stream": false,
        "messages": [{"role": "user", "content": "算一下 1+1"}]
    }"#;
    let (status, body) = raw_http(fx.port, "POST", "/v1/chat/completions", Some(blocking))
        .await
        .unwrap();
    assert_eq!(status, 200, "响应体：{body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("应是 JSON");
    assert_eq!(parsed["object"], "chat.completion");
    assert_eq!(parsed["choices"][0]["message"]["content"], "1+1 等于 2。");
    assert_eq!(
        parsed["choices"][0]["message"]["reasoning_content"],
        "先算一下：1+1 显然等于 2。"
    );
    assert_eq!(parsed["usage"]["completion_tokens"], 20);
}

#[tokio::test]
async fn anthropic_blocking_returns_thinking_block_and_text() {
    let fx = start("anthropic-blocking", FAKE_OK).await;
    let request = r#"{
        "model": "cursor/auto",
        "thinking": {"type": "enabled"},
        "messages": [{"role": "user", "content": "算一下 1+1"}]
    }"#;
    let (status, body) = raw_http(fx.port, "POST", "/v1/messages", Some(request))
        .await
        .unwrap();
    assert_eq!(status, 200, "响应体：{body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("应是 JSON");
    assert_eq!(parsed["type"], "message");
    assert_eq!(parsed["content"][0]["type"], "thinking");
    assert_eq!(
        parsed["content"][0]["signature"],
        "skip_thought_signature_validator"
    );
    assert_eq!(parsed["content"][1]["type"], "text");
    assert_eq!(parsed["content"][1]["text"], "1+1 等于 2。");
    assert_eq!(parsed["usage"]["input_tokens"], 100);
    // cursor/ 前缀是给客户端标明来源用的，不能原样丢给 CLI
    let args = fx.args_log();
    assert!(!args.contains("cursor/auto"), "cursor/ 前缀没剥掉：{args}");
}

#[tokio::test]
async fn a_cli_that_says_nothing_is_an_error_not_an_empty_success() {
    let fx = start("dead-cli", FAKE_DEAD).await;

    // 非流式：必须 502，并且把 CLI 的 stderr 带出来
    let request = r#"{"model":"auto","messages":[{"role":"user","content":"在吗"}]}"#;
    let (status, body) = raw_http(fx.port, "POST", "/v1/messages", Some(request))
        .await
        .unwrap();
    assert_eq!(status, 502, "空回合不该算成功：{body}");
    assert!(body.contains("未登录"), "没把原因带出来：{body}");

    // 流式：给出 error 事件后收尾，不能装成正常结束
    let request = r#"{"model":"auto","stream":true,"messages":[{"role":"user","content":"在吗"}]}"#;
    let (status, body) = raw_http(fx.port, "POST", "/v1/messages", Some(request))
        .await
        .unwrap();
    assert_eq!(status, 200, "流式已经开始就没法改状态码了：{body}");
    assert!(body.contains("event: error"), "缺 error 事件：{body}");
    assert!(body.contains("未登录"), "error 事件里没原因：{body}");
    assert!(
        !body.contains(r#""stop_reason":"end_turn""#),
        "出错不能报成正常收尾：{body}"
    );
}

#[tokio::test]
async fn models_endpoint_lists_what_the_cli_says() {
    let script = r#"if [ "$1" = "--list-models" ]; then
  printf 'Available models\n\nauto - Auto (current, default)\ngpt-5.3-codex - Codex 5.3\n'
  exit 0
fi
printf '%s\n' "$@" > "$(dirname "$0")/args.txt"
cat "$(dirname "$0")/events.jsonl"
"#;
    let fx = start("models", script).await;
    let (status, body) = raw_http(fx.port, "GET", "/v1/models", None).await.unwrap();
    assert_eq!(status, 200, "响应体：{body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let ids: Vec<&str> = parsed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(ids, ["auto", "gpt-5.3-codex"]);
}
