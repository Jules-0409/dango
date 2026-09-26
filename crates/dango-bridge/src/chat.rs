//! 两条协议的聊天入口 + 推流内核（pump）。
//!
//! 移植自 `src/bridge/server.mjs` 的 `handleMessages` / `handleChatCompletions` / `pump`。
//!
//! 账号层在 pump 里生效：**只要还没往客户端吐过一个 chunk，就允许换账号重试**；
//! 一旦吐过（流式的响应头也就写出去了），就锁死当前账号，失败如实报错。
//!
//! 一个有意为之的偏差：JS 版在客户端断开后仍会继续把上游读完（Node 里 `res.write` 不报错），
//! 这边一旦发现往客户端的通道关了，就中止上游读取 —— 省的是额度，不是代码。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::response::Response;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use crate::accounts::{Candidate, CandidateQuery, FailureInfo};
use crate::anthropic_request::{build_gemini_request, derive_session_key, BuildOptions};
use crate::anthropic_stream::{
    sse_frame, AnthropicTranslator, TranslatorOptions as AnthropicTranslatorOptions,
    TranslatorStats,
};
use crate::openai::{
    build_gemini_request_from_openai, sse_data_frame, BuildOptions as OpenAiBuildOptions,
    OpenAiTranslator, TranslatorOptions as OpenAiTranslatorOptions,
};
use crate::qoder::UpstreamKind;
use crate::sanitize::{
    fill_missing_openai_tool_results, fill_missing_tool_results, sanitize_messages,
};
use crate::server::{budget_note, json_response, map_openai_error, truncate, Bridge, MappedError};
use crate::types::{
    iso_from_millis, now_millis, GenerateRequest, ResolvedModel, StreamEvent, UpstreamError,
};

// ---------------------------------------------------------------- 出口

/// pump 往客户端写的东西。第一次一定是 `Head`（流式）或 `Json`（整包/失败），
/// 服务层拿到它才决定响应头 —— 这就是「响应头故意晚一点写」的实现方式：
/// 上游有可能直接失败（403/400/模型下线），那时候要给真正的状态码，
/// 而不是一个「200 + 空回合」。
pub enum PumpMsg {
    /// 流式：200 的 SSE 响应头（紧接着的 Body 是开场帧）
    Head(Vec<(String, String)>),
    /// 流式的正文（已经是拼好的 SSE 帧）
    Body(String),
    /// 一次性 JSON（整包成功 / 还没开流就失败）
    Json {
        status: u16,
        body: Value,
        headers: Vec<(String, String)>,
    },
}

/// 协议适配器：上游 chunk → 客户端帧。两条协议的差别全在这里。
pub trait ProtocolAdapter: Send {
    /// 这块协议要流式吗（非流式时帧都攒在适配器里，最后一次性出 JSON）
    fn is_stream(&self) -> bool;
    /// 还没吐过字节时的开场帧（Anthropic 的 message_start；OpenAI 的第一个 chunk）
    fn open_frames(&mut self) -> Vec<String>;
    /// 一个上游 chunk
    fn chunk_frames(&mut self, chunk: &Value) -> Vec<String>;
    /// 正常收尾（Anthropic 的 message_delta + message_stop；OpenAI 的 finish chunk）
    fn close_frames(&mut self) -> Vec<String>;
    /// 失败收尾的 SSE 帧（流已经开出去的时候用）
    fn fail_frames(&mut self, mapped: &MappedError) -> Vec<String>;
    /// 整包模式的响应体
    fn buffered_body(&mut self) -> Value;
    /// 失败时整包模式的响应体（两条协议外形不同）
    fn failure_body(&self, mapped: &MappedError) -> Value;
    /// 流式收尾后还要写的东西（OpenAI 的 `data: [DONE]`）
    fn tail(&self) -> &'static str {
        ""
    }
    fn stats(&self) -> TranslatorStats;
}

/// Anthropic Messages 适配器。
struct AnthropicAdapter {
    translator: AnthropicTranslator,
    stream: bool,
    /// 整包模式下攒的是**翻译后的事件**（收尾时交给 eventsToMessage 拼成完整 message）
    buffered_events: Vec<Value>,
}

impl ProtocolAdapter for AnthropicAdapter {
    fn is_stream(&self) -> bool {
        self.stream
    }

    fn open_frames(&mut self) -> Vec<String> {
        self.translator.start().iter().map(sse_frame).collect()
    }

    fn chunk_frames(&mut self, chunk: &Value) -> Vec<String> {
        let events = self.translator.push(chunk);
        if self.stream {
            events.iter().map(sse_frame).collect()
        } else {
            self.buffered_events.extend(events);
            Vec::new()
        }
    }

    fn close_frames(&mut self) -> Vec<String> {
        let events = self.translator.finish();
        if self.stream {
            events.iter().map(sse_frame).collect()
        } else {
            self.buffered_events.extend(events);
            Vec::new()
        }
    }

    fn fail_frames(&mut self, mapped: &MappedError) -> Vec<String> {
        self.translator
            .fail(&mapped.message, &mapped.kind)
            .iter()
            .map(sse_frame)
            .collect()
    }

    fn buffered_body(&mut self) -> Value {
        let events = std::mem::take(&mut self.buffered_events);
        self.translator.events_to_message(events)
    }

    fn failure_body(&self, mapped: &MappedError) -> Value {
        json!({ "type": "error", "error": { "type": mapped.kind, "message": mapped.message } })
    }

    fn stats(&self) -> TranslatorStats {
        self.translator.stats()
    }
}

/// OpenAI Chat Completions 适配器。
struct OpenAiAdapter {
    translator: OpenAiTranslator,
    stream: bool,
    /// 整包模式下攒的是**上游原始 chunk**（JS 那边就是不翻译，最后交给 toCompletion）
    buffered_chunks: Vec<Value>,
}

impl ProtocolAdapter for OpenAiAdapter {
    fn is_stream(&self) -> bool {
        self.stream
    }

    fn open_frames(&mut self) -> Vec<String> {
        self.translator.start().iter().map(sse_data_frame).collect()
    }

    fn chunk_frames(&mut self, chunk: &Value) -> Vec<String> {
        if self.stream {
            self.translator
                .push(chunk)
                .iter()
                .map(sse_data_frame)
                .collect()
        } else {
            self.buffered_chunks.push(chunk.clone());
            Vec::new()
        }
    }

    fn close_frames(&mut self) -> Vec<String> {
        if self.stream {
            self.translator
                .finish()
                .iter()
                .map(sse_data_frame)
                .collect()
        } else {
            Vec::new()
        }
    }

    fn fail_frames(&mut self, mapped: &MappedError) -> Vec<String> {
        vec![sse_data_frame(
            &json!({ "error": map_openai_error(mapped) }),
        )]
    }

    fn buffered_body(&mut self) -> Value {
        let chunks = std::mem::take(&mut self.buffered_chunks);
        self.translator.to_completion(&chunks)
    }

    fn failure_body(&self, mapped: &MappedError) -> Value {
        json!({ "error": map_openai_error(mapped) })
    }

    fn tail(&self) -> &'static str {
        // OpenAI 的流要用 [DONE] 收尾，Anthropic 没有这个东西
        "data: [DONE]\n\n"
    }

    fn stats(&self) -> TranslatorStats {
        self.translator.stats()
    }
}

// ---------------------------------------------------------------- pump

/// 一次尝试的记录（对齐 JS 的 `attempts[]`，日志里要给人看）。
#[derive(Debug, Clone)]
struct Attempt {
    account: String,
    why: &'static str,
    status: u16,
    reason: String,
}

impl Attempt {
    fn to_json(&self) -> Value {
        json!({ "account": self.account, "why": self.why, "status": self.status, "reason": self.reason })
    }
}

/// pump 的结局（日志与计数器要的全部信息）。
struct PumpOutcome {
    ok: bool,
    chunks: usize,
    opened: bool,
    mapped: Option<MappedError>,
    reason: Option<String>,
    account: Option<String>,
    attempts: Vec<Attempt>,
    /// 客户端提前走了（我们主动停了上游）：日志里要能看出来，不然像是上游异常
    client_gone: bool,
    /// 这一发用过「空回合 → 关思考重试」的修复（日志的 note 与 warnings 都看它）
    repaired: bool,
}

/// 所有候选都试完之后，给客户端的那个错误。
///
/// 三种口径分开，别把「连不上」说成「被限流」（客户端看到 429 会退避很久，那是误导）：
///   - 有账号级失败（401/403/429）且不止一次尝试 → 429 + 最早可重试时间；
///   - 尝试全是网络层失败（status 0）或空回合 → 502 `api_error`，并如实说是哪一种；
///   - 其余（400/404/5xx）→ 照上游那个状态码映射。
fn map_attempts(
    attempts: &[Attempt],
    last_failure: Option<&UpstreamError>,
    no_data_text: &str,
    retry_after: Option<i64>,
) -> MappedError {
    let summary = attempts
        .iter()
        .map(|a| format!("{}:{}", a.account, a.status))
        .collect::<Vec<_>>()
        .join(" → ");
    let account_blocked = last_failure.is_some()
        && attempts.len() > 1
        && attempts
            .iter()
            .all(|a| crate::accounts::is_account_failure(a.status) || a.status == 0)
        && attempts
            .iter()
            .any(|a| crate::accounts::is_account_failure(a.status));
    if account_blocked {
        let failure = last_failure.expect("account_blocked 为真就一定有 last_failure");
        return MappedError {
            http: 429,
            kind: "rate_limit_error".into(),
            message: format!(
                "所有候选账号都被上游拒了（{summary}）：{}",
                truncate(&failure.message, 200)
            ),
            retry_after_seconds: retry_after,
        };
    }
    let all_transport =
        !attempts.is_empty() && attempts.iter().all(|a| a.status == 0 || a.status == 200);
    if all_transport {
        if let Some(failure) = last_failure {
            let message = if failure.reason == "empty_turn" {
                format!("上游返回了空回合（没有任何正文，可能是思考把输出预算吃满）：{summary}")
            } else {
                format!(
                    "连不上上游（{summary}）：{}",
                    truncate(&failure.message, 200)
                )
            };
            return MappedError {
                http: 502,
                kind: "api_error".into(),
                message,
                retry_after_seconds: None,
            };
        }
    }
    if let Some(failure) = last_failure {
        return MappedError::from(failure);
    }
    MappedError {
        http: 502,
        kind: "api_error".into(),
        message: format!(
            "上游没有返回数据流（可能这个模型已下线）：{}",
            truncate(no_data_text, 300)
        ),
        retry_after_seconds: None,
    }
}

/// 空回合的修复体：同一个请求，但把思考关掉（`thinkingBudget: 0`）。
///
/// 两处克制：客户端**明确要了思考**（预算 > 0）就不造修复体 —— 那就不是我们该偷偷改的；
/// 造出来也只是备用，只有真出现「一个可见字节都没有」的回合才会用（见 `pump`）。
fn repair_variant(inner: &Value) -> Option<Value> {
    let wanted = inner
        .get("generationConfig")
        .and_then(|g| g.get("thinkingConfig"))
        .and_then(|t| t.get("thinkingBudget"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if wanted > 0 {
        return None;
    }
    let mut repaired = inner.clone();
    repaired
        .get_mut("generationConfig")?
        .as_object_mut()?
        .insert("thinkingConfig".into(), json!({ "thinkingBudget": 0 }));
    Some(repaired)
}

fn label_of(cand: &Candidate) -> String {
    if cand.account.email_masked.is_empty() {
        cand.id.clone()
    } else {
        cand.account.email_masked.clone()
    }
}

/// 桥自己的两个响应头（排查时不用翻日志）。
fn bridge_headers(model: &str, account: Option<&str>) -> Vec<(String, String)> {
    vec![
        ("x-bridge-model".to_string(), model.to_string()),
        (
            "x-bridge-account".to_string(),
            account.unwrap_or("-").to_string(),
        ),
    ]
}

/// pump 的输入（参数一多就打包，省得每个调用点都数位置）。
struct PumpInput<'a> {
    bridge: &'a Arc<Bridge>,
    /// 这次请求走哪个上游（决定用哪个池子；`qoder/` 前缀走 Qoder）
    kind: UpstreamKind,
    protocol: &'a mut dyn ProtocolAdapter,
    inner: &'a Value,
    /// 空回合的修复体（None = 客户端要了思考，或者压根造不出来）
    repair: Option<&'a Value>,
    model: &'a str,
    session_key: &'a str,
    tx: &'a mpsc::Sender<PumpMsg>,
    /// 正文快照：调用方持有（写日志要用），pump 只负责往里塞
    capture: &'a mut BodyCapture,
    /// 请求日志的 warnings：「桥做过什么」要落进去（比如空回合重试）
    warnings: &'a mut Vec<String>,
}

/// 推流内核：上游 → 客户端的差异全部交给 `protocol`。
async fn pump(input: PumpInput<'_>) -> PumpOutcome {
    let PumpInput {
        bridge,
        kind,
        protocol,
        inner,
        repair,
        model,
        session_key,
        tx,
        capture,
        warnings,
    } = input;
    let stream = protocol.is_stream();
    let pool = bridge.pool_of(kind);
    let candidates = pool.candidates(CandidateQuery {
        model: Some(model.to_string()),
        session_key: Some(session_key.to_string()),
    });

    let mut attempts: Vec<Attempt> = Vec::new();
    let mut opened = false;
    let mut chunks = 0usize;
    let mut last_failure: Option<UpstreamError> = None;
    let mut no_data_text = String::new();
    let mut served_by: Option<String> = None;
    let mut head_sent = false;
    // 用下标而不是 `for`：空回合时要拿**同一个账号 + 关思考的请求体**再试一发。
    let mut body: &Value = inner;
    let mut repair_left = repair.is_some();
    let mut repaired = false;
    let mut index = 0usize;

    while index < candidates.len() {
        let cand = &candidates[index];
        let label = label_of(cand);
        let session = match pool.open(&cand.id).await {
            Ok(session) => session,
            Err(err) => {
                let message = err.to_string();
                pool.note_failure(
                    &cand.id,
                    FailureInfo::new(0)
                        .reason("session_init")
                        .message(&message)
                        .model(model),
                );
                attempts.push(Attempt {
                    account: label,
                    why: cand.why,
                    status: 0,
                    reason: "session_init".into(),
                });
                last_failure = Some(UpstreamError::network("session_init", message));
                index += 1;
                continue;
            }
        };

        let (event_tx, mut event_rx) = mpsc::channel(64);
        let request = GenerateRequest {
            model: model.to_string(),
            request: body.clone(),
            session_key: Some(session_key.to_string()),
            request_id: uuid::Uuid::new_v4().to_string(),
        };
        // 上游读取放进独立任务：它写 channel 的时候，这边同时往客户端推
        let producer = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.stream_generate(request, event_tx).await }
        });

        let mut failure: Option<UpstreamError> = None;
        let mut raw_text = String::new();
        let mut chunks_this_attempt = 0usize;

        while let Some(event) = event_rx.recv().await {
            match event {
                StreamEvent::Open { .. } => opened = true,
                StreamEvent::Error(err) => {
                    failure = Some(err);
                    break;
                }
                StreamEvent::NoData { raw_text: text, .. } => {
                    raw_text = text;
                    break;
                }
                StreamEvent::Chunk(chunk) => {
                    chunks_this_attempt += 1;
                    chunks += 1;
                    // 日志要的正文快照（有界）：上游给过什么就记什么
                    capture.push_chunk(&chunk);
                    if served_by.is_none() {
                        served_by = Some(label.clone());
                        if cand.why == "breaker_forced" {
                            (bridge.log)(format!(
                                "注意：候选账号都在熔断冷却里，这一发是强行试的（{label}）"
                            ));
                        }
                    }
                    // 第一块正文到了才写响应头：在那之前失败还能给真正的状态码
                    if stream && !head_sent {
                        let mut text = String::new();
                        for frame in protocol.open_frames() {
                            text.push_str(&frame);
                        }
                        let headers = bridge_headers(model, served_by.as_deref());
                        if tx.send(PumpMsg::Head(headers)).await.is_err() {
                            producer.abort();
                            return client_gone_outcome(chunks, opened, served_by, attempts);
                        }
                        let _ = tx.send(PumpMsg::Body(text)).await;
                        head_sent = true;
                    }
                    let frames = protocol.chunk_frames(&chunk);
                    if stream
                        && !frames.is_empty()
                        && tx.send(PumpMsg::Body(frames.concat())).await.is_err()
                    {
                        producer.abort();
                        return client_gone_outcome(chunks, opened, served_by, attempts);
                    }
                }
            }
        }
        // 生产任务此时已经把事件发完（channel 关了）；join 一下把 panic 收干净
        let _ = producer.await;

        // 正常收尾：上游把流读完了，没有报错也没有「200 但没数据」
        if failure.is_none() && raw_text.is_empty() {
            // 空回合：客户端一个可见字节都没拿到（整包时连块都没有，流式时连开场帧都白写）。
            // 这不能算成功 —— 客户端拿到的是「模型什么都没说」，而不是一个回答。
            if capture.is_empty() {
                attempts.push(Attempt {
                    account: label.clone(),
                    why: cand.why,
                    status: 200,
                    reason: "empty_turn".into(),
                });
                if repair_left {
                    // 修一次：同一个账号、关掉思考再发一发（客户端没要过思考才敢这么干）。
                    repair_left = false;
                    repaired = true;
                    body = repair.expect("repair_left 为真就一定有修复体");
                    capture.reset();
                    warnings.push("empty_turn_retry:thinking_off".into());
                    (bridge.log)(format!(
                        "空回合：{label} 这一发没有任何正文（chunks={chunks}），同账号关掉思考再试一发"
                    ));
                    continue; // 同一个账号，用修复体
                }
                last_failure = Some(UpstreamError::network(
                    "empty_turn",
                    "上游 200 但没有任何正文（空回合）",
                ));
                index += 1;
                continue;
            }
            if repaired {
                warnings.push("empty_turn_repaired".into());
            }
            pool.note_success(&cand.id);
            pool.remember(session_key, &cand.id);
            if served_by.is_none() {
                served_by = Some(label.clone());
            }
            attempts.push(Attempt {
                account: label,
                why: cand.why,
                status: 200,
                reason: "ok".into(),
            });
            emit_done(protocol, tx, stream, head_sent, model, served_by.as_deref()).await;
            return PumpOutcome {
                ok: true,
                chunks,
                opened,
                mapped: None,
                reason: None,
                account: served_by,
                attempts,
                client_gone: false,
                repaired,
            };
        }

        // 已经吐过字节：换号会让客户端看到两段正文，只能如实报错
        if chunks_this_attempt > 0 {
            let info = match &failure {
                Some(err) => FailureInfo::new(err.status)
                    .reason(&err.reason)
                    .message(&err.message)
                    .model(model),
                None => FailureInfo::new(200)
                    .reason("no_data")
                    .message(&raw_text)
                    .model(model),
            };
            pool.note_failure(&cand.id, info);
            let mapped = match &failure {
                Some(err) => MappedError::from(err),
                None => MappedError {
                    http: 502,
                    kind: "api_error".into(),
                    message: format!(
                        "上游没有返回数据流（可能这个模型已下线）：{}",
                        truncate(&raw_text, 300)
                    ),
                    retry_after_seconds: None,
                },
            };
            attempts.push(Attempt {
                account: label.clone(),
                why: cand.why,
                status: failure.as_ref().map(|e| e.status).unwrap_or(200),
                reason: failure
                    .as_ref()
                    .map(|e| e.reason.clone())
                    .unwrap_or_else(|| "no_data_midstream".to_string()),
            });
            let reason = failure
                .as_ref()
                .map(|e| e.reason.clone())
                .unwrap_or_else(|| "no_data".into());
            emit_fail(protocol, tx, stream, head_sent, &mapped).await;
            return PumpOutcome {
                ok: false,
                chunks,
                opened,
                reason: Some(reason),
                mapped: Some(mapped),
                account: Some(label),
                attempts,
                client_gone: false,
                repaired,
            };
        }

        if let Some(err) = failure {
            pool.note_failure(
                &cand.id,
                FailureInfo::new(err.status)
                    .reason(&err.reason)
                    .message(&err.message)
                    .model(model),
            );
            attempts.push(Attempt {
                account: label,
                why: cand.why,
                status: err.status,
                reason: err.reason.clone(),
            });
            let retryable = crate::accounts::is_retryable(err.status);
            last_failure = Some(err);
            if !retryable {
                break; // 400/404 这类换号也没用
            }
            index += 1;
            continue;
        }

        // 200 但没有数据流：模型下线的味道，与账号无关，不换号
        attempts.push(Attempt {
            account: label,
            why: cand.why,
            status: 200,
            reason: "no_data".into(),
        });
        no_data_text = raw_text;
        break;
    }

    let attempts_summary = attempts
        .iter()
        .map(|a| format!("{}:{}", a.account, a.status))
        .collect::<Vec<_>>()
        .join(" → ");

    let mapped = map_attempts(
        &attempts,
        last_failure.as_ref(),
        &no_data_text,
        pool.earliest_retry_in_seconds(Some(model)),
    );

    if let Some(failure) = &last_failure {
        (bridge.log)(format!(
            "所有候选账号都没成（{attempts_summary}）：{}",
            truncate(&failure.message, 160)
        ));
    }
    emit_fail(protocol, tx, stream, head_sent, &mapped).await;
    PumpOutcome {
        ok: false,
        chunks,
        opened,
        reason: Some(
            last_failure
                .map(|e| e.reason)
                .unwrap_or_else(|| "no_data".into()),
        ),
        mapped: Some(mapped),
        account: served_by,
        attempts,
        client_gone: false,
        repaired,
    }
}

fn client_gone_outcome(
    chunks: usize,
    opened: bool,
    served_by: Option<String>,
    attempts: Vec<Attempt>,
) -> PumpOutcome {
    PumpOutcome {
        ok: false,
        chunks,
        opened,
        mapped: None,
        reason: Some("client_gone".into()),
        account: served_by,
        attempts,
        client_gone: true,
        repaired: false,
    }
}

/// 正常收尾：流式写完收尾帧 + 尾巴；整包时一次性给 JSON（带头）。
async fn emit_done(
    protocol: &mut dyn ProtocolAdapter,
    tx: &mpsc::Sender<PumpMsg>,
    stream: bool,
    head_sent: bool,
    model: &str,
    account: Option<&str>,
) {
    if !stream {
        // 收尾事件（message_delta 里的 stop_reason/usage、message_stop）也要攒进去，
        // 否则整包 message 的 stop_reason 会变成 null —— JS 的 onDone 就是先 finish 再 eventsToMessage
        protocol.close_frames();
        let _ = tx
            .send(PumpMsg::Json {
                status: 200,
                body: protocol.buffered_body(),
                headers: bridge_headers(model, account),
            })
            .await;
        return;
    }

    let mut text = String::new();
    if !head_sent {
        // 一个 chunk 都没有也可能正常收尾（空回合）：头也得补上，不然客户端等到的是断流
        for frame in protocol.open_frames() {
            text.push_str(&frame);
        }
        let _ = tx.send(PumpMsg::Head(bridge_headers(model, account))).await;
    }
    for frame in protocol.close_frames() {
        text.push_str(&frame);
    }
    text.push_str(protocol.tail());
    if !text.is_empty() {
        let _ = tx.send(PumpMsg::Body(text)).await;
    }
}

/// 失败收尾：流已经开出去就发错误帧，没开出去就给真正的状态码 + JSON。
async fn emit_fail(
    protocol: &mut dyn ProtocolAdapter,
    tx: &mpsc::Sender<PumpMsg>,
    stream: bool,
    head_sent: bool,
    mapped: &MappedError,
) {
    if stream && head_sent {
        for frame in protocol.fail_frames(mapped) {
            let _ = tx.send(PumpMsg::Body(frame)).await;
        }
        let _ = tx.send(PumpMsg::Body(protocol.tail().to_string())).await;
        return;
    }
    let headers = mapped
        .retry_after_seconds
        .map(|s| vec![("retry-after".to_string(), s.to_string())])
        .unwrap_or_default();
    let _ = tx
        .send(PumpMsg::Json {
            status: mapped.http,
            body: protocol.failure_body(mapped),
            headers,
        })
        .await;
}

// ---------------------------------------------------------------- 请求日志

/// 日志条目的短 id：面板按它去 `/logs/entry?id=` 取这一条的正文。
/// `at` 只到秒，单靠它认条目迟早撞车，所以带一个进程内自增序号。
static LOG_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_log_id() -> String {
    format!(
        "{:x}-{:x}",
        now_millis(),
        LOG_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// 正文快照：limit 为 0 时全部保留（不截断）；limit > 0 时只留前 `limit` 个字符。
/// 另外记住原本多大、截断过没有。面板上「残的就说残的」。
#[derive(Debug, Clone, Default)]
struct BodyCapture {
    text: String,
    /// 上游一共给了多少字符（含被截掉的）
    chars: usize,
    /// 已经塞进 text 的字符数（省得每次 push 都重新数一遍）
    kept: usize,
    truncated: bool,
    /// 0 表示不截断
    limit: usize,
}

impl BodyCapture {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            ..Default::default()
        }
    }

    fn push(&mut self, piece: &str) {
        let len = piece.chars().count();
        self.chars += len;
        if self.limit == 0 {
            self.text.push_str(piece);
            self.kept += len;
            return;
        }
        if self.kept >= self.limit {
            self.truncated = true;
            return;
        }
        let room = self.limit - self.kept;
        if len <= room {
            self.text.push_str(piece);
            self.kept += len;
        } else {
            // 按字符切，不切断多字节
            self.text.extend(piece.chars().take(room));
            self.kept = self.limit;
            self.truncated = true;
        }
    }

    /// 一个上游 chunk（v1internal 的形状）里的正文与工具调用，按客户端看到的样子攒起来。
    /// 换号重试不影响它：只有真拿到过的 chunk 才会进来，而「拿到过就不会再换号」。
    fn push_chunk(&mut self, chunk: &Value) {
        let parts = chunk
            .get("response")
            .and_then(|r| r.get("candidates"))
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array);
        let Some(parts) = parts else { return };
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                // 思考 part 也带 text，但它不是「回复正文」—— 标出来，别让看日志的人
                // 以为模型把思考当答案说了（`thought: true` 是上游给思考打的标）。
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    self.push("\n[思考] ");
                    self.push(text);
                    self.push("\n");
                } else {
                    self.push(text);
                }
            }
            if let Some(call) = part.get("functionCall") {
                let name = call.get("name").and_then(Value::as_str).unwrap_or("?");
                let args = call.get("args").cloned().unwrap_or(Value::Null);
                self.push(&format!("\n[工具调用] {name}({args})\n"));
            }
        }
    }

    /// 客户端一个可见字节都没拿到（正文、工具调用、思考标记全没有）。
    /// 空回合的判定就看它 —— 「上游回了 200」不等于「客户端拿到了东西」。
    fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// 空回合改成关思考和重试时用：日志该记的是最后那一发给客户端的东西，
    /// 不是几次尝试的混合体。
    fn reset(&mut self) {
        let limit = self.limit;
        *self = Self::new(limit);
    }

    /// 一个字都没有就交 `None`：没正文的请求不该在日志里凭空多出一个 "response" 字段。
    fn to_json(&self) -> Option<Value> {
        if self.chars == 0 {
            return None;
        }
        Some(json!({
            "text": self.text,
            "chars": self.chars,
            "truncated": self.truncated,
        }))
    }
}

/// 客户端请求的快照（先红掉图片那种巨型 base64，再截断；limit 0 表示不截断）。
///
/// 深拷贝一次是刻意的：不红掉 `data`，一张图就能把预算全吃掉，正文一个字都留不下。
fn request_snapshot(body: &Value, limit: usize) -> Value {
    let mut value = body.clone();
    redact_blobs(&mut value);
    let text = serde_json::to_string(&value).unwrap_or_default();
    let bytes = text.len();
    let truncated = if limit == 0 {
        false
    } else {
        text.chars().count() > limit
    };
    let kept: String = if truncated {
        text.chars().take(limit).collect()
    } else {
        text
    };
    json!({ "json": kept, "bytes": bytes, "truncated": truncated })
}

/// 递归找「一看就是二进制」的字符串（Anthropic 的图片来源字段叫 `data`），换成一句说明。
fn redact_blobs(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if key == "data" {
                    if let Value::String(s) = item {
                        let chars = s.chars().count();
                        if chars > 256 {
                            *item = json!(format!("<{chars} 字符的二进制数据，没记>"));
                            continue;
                        }
                    }
                }
                redact_blobs(item);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_blobs(item);
            }
        }
        _ => {}
    }
}

/// 两个入口共用的日志上下文。
struct LogCtx {
    kind: &'static str,
    requested_model: Option<String>,
    model: String,
    stream: bool,
    session_key: String,
    warnings: Vec<String>,
    tool_signature_hits: usize,
    tool_signature_misses: usize,
    started_at: i64,
    /// 客户端请求的快照（`--no-log-bodies` 时为 `Value::Null`，避免无谓的深拷贝和序列化）
    request: Value,
}

/// 翻译器统计摊成 JS 那边的形状，好复用（有测试的）`budget_note`。
fn stats_as_js(stats: &TranslatorStats) -> Value {
    json!({
        "finishReason": stats.finish_reason,
        "usage": {
            "thoughts_token_count": stats.thoughts_tokens,
            "output_tokens": stats.output_tokens,
        },
    })
}

async fn finish_log(
    bridge: &Arc<Bridge>,
    ctx: LogCtx,
    outcome: &PumpOutcome,
    stats: &TranslatorStats,
    capture: &BodyCapture,
) {
    let elapsed = now_millis() - ctx.started_at;
    let stream = ctx.stream;
    let js_stats = stats_as_js(stats);
    // 空回合是「这一发客户端什么都没拿到」的直接证据，优先于预算口径的 note
    let note = if outcome.repaired && outcome.ok {
        Some("empty_turn_repaired".to_string())
    } else if outcome.reason.as_deref() == Some("empty_turn") {
        Some("empty_turn".to_string())
    } else {
        budget_note(&js_stats)
    };
    let attempts: Vec<Value> = outcome.attempts.iter().map(Attempt::to_json).collect();

    let mut entry = Map::new();
    // 面板按 id 去 /logs/entry?id= 取这一条的正文
    entry.insert("id".into(), json!(next_log_id()));
    entry.insert("at".into(), json!(iso_from_millis(now_millis())));
    entry.insert("kind".into(), json!(ctx.kind));
    entry.insert("model".into(), json!(ctx.model));
    entry.insert("account".into(), json!(outcome.account));
    entry.insert("attempts".into(), json!(attempts));
    entry.insert("requestedModel".into(), json!(ctx.requested_model));
    entry.insert("stream".into(), json!(ctx.stream));
    entry.insert("session".into(), json!(ctx.session_key));
    entry.insert("chunks".into(), json!(outcome.chunks));
    entry.insert("ok".into(), json!(outcome.ok));
    entry.insert("upstreamOpened".into(), json!(outcome.opened));
    if ctx.kind == "chat.completions" {
        entry.insert(
            "feedBack".into(),
            json!(if outcome.chunks == 0 {
                Some("zero_chunks")
            } else {
                None
            }),
        );
    }
    entry.insert("stopReason".into(), json!(stats.finish_reason));
    // 思考/正文各花了多少：排查「正文被思考挤空」时一眼就能对上（面板详情里直接摆出来）
    if let Some(usage) = js_stats.get("usage") {
        entry.insert(
            "thoughtsTokens".into(),
            usage
                .get("thoughts_token_count")
                .cloned()
                .unwrap_or(Value::Null),
        );
        entry.insert(
            "outputTokens".into(),
            usage.get("output_tokens").cloned().unwrap_or(Value::Null),
        );
    }
    // 完整用量（输入含缓存命中部分；输出含思考），给 dango 的 token 账本按账号记账。
    entry.insert("inputTokens".into(), json!(stats.input_tokens));
    entry.insert("cachedTokens".into(), json!(stats.cached_tokens));
    entry.insert("note".into(), json!(note));
    entry.insert("toolCalls".into(), json!(stats.saw_tool_use));
    entry.insert("leakedCalls".into(), json!(stats.leaked_calls));
    entry.insert("signatureHits".into(), json!(ctx.tool_signature_hits));
    entry.insert("signatureMisses".into(), json!(ctx.tool_signature_misses));
    entry.insert("warnings".into(), json!(ctx.warnings));
    entry.insert("elapsedMs".into(), json!(elapsed));
    match (&outcome.mapped, outcome.client_gone) {
        (Some(mapped), _) => {
            entry.insert(
                "error".into(),
                json!({
                    "http": mapped.http,
                    "type": mapped.kind,
                    "message": mapped.message,
                    "reason": outcome.reason,
                }),
            );
        }
        (None, true) => {
            entry.insert("error".into(), json!({ "reason": "client_gone" }));
        }
        _ => {}
    }
    // 正文（有界）：默认记；关掉的话留一个明确的记号，免得翻日志的人以为是自己丢了包。
    if bridge.log_bodies {
        entry.insert("request".into(), ctx.request);
        if let Some(response) = capture.to_json() {
            entry.insert("response".into(), response);
        }
    } else {
        entry.insert("bodies".into(), json!("off"));
    }
    bridge.write_log(&Value::Object(entry)).await;

    // 计数器和日志口径对齐 JS：成功才分「流式/整包」，失败记一次错误
    if outcome.ok {
        if stream {
            bridge.counters.streamed();
        } else {
            bridge.counters.buffered();
        }
    } else if outcome.mapped.is_some() {
        bridge.counters.errors();
    }
    bridge
        .counters
        .leaks(stats.leaked_calls as u64, stats.leaks_ignored as u64);
    // 换号次数看**不同账号**，不是尝试次数：空回合重试是同一个账号的第二发，
    // 记成「换号」会撒谎。
    let accounts_tried = outcome
        .attempts
        .iter()
        .map(|a| a.account.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if accounts_tried > 1 {
        bridge.counters.account_switch();
    }

    let switches = if accounts_tried > 1 {
        format!("（换号 {} 次）", accounts_tried - 1)
    } else {
        String::new()
    };
    let account = outcome.account.as_deref().unwrap_or("-");
    let note_text = note.clone().map(|n| format!(" [{n}]")).unwrap_or_default();
    let head = match &outcome.mapped {
        Some(mapped) => format!("← 失败 {} {}", mapped.http, mapped.kind),
        None if outcome.client_gone => "← 客户端断开".to_string(),
        None => "← 200".to_string(),
    };
    if ctx.kind == "chat.completions" {
        (bridge.log)(format!(
            "{head} [openai] {} {} chunks={} 工具={} 账号={account}{switches} {elapsed}ms{note_text}{}",
            ctx.model,
            if ctx.stream { "流式" } else { "整包" },
            outcome.chunks,
            if stats.saw_tool_use { "有" } else { "无" },
            match &outcome.mapped {
                Some(mapped) => format!("：{}", truncate(&mapped.message, 160)),
                None => String::new(),
            },
        ));
    } else {
        (bridge.log)(format!(
            "{head} {} {} chunks={} 工具={} 账号={account}{switches} 泄漏修复={} 忽略={} 签名命中={}/缺={} {elapsed}ms{note_text}{}",
            ctx.model,
            if ctx.stream { "流式" } else { "整包" },
            outcome.chunks,
            if stats.saw_tool_use { "有" } else { "无" },
            stats.leaked_calls,
            stats.leaks_ignored,
            ctx.tool_signature_hits,
            ctx.tool_signature_misses,
            match &outcome.mapped {
                Some(mapped) => format!("：{}", truncate(&mapped.message, 160)),
                None => String::new(),
            },
        ));
    }
}

// ---------------------------------------------------------------- 两个入口

/// 起 pump、把第一条消息转成响应。返回时要么流已经在推，要么整包 JSON 已经好了。
async fn run_protocol(
    bridge: Arc<Bridge>,
    mut protocol: Box<dyn ProtocolAdapter>,
    inner: Value,
    // 空回合的修复体（None = 客户端要了思考，或者造不出来）
    repair: Option<Value>,
    model: String,
    session_key: String,
    mut ctx: LogCtx,
) -> Response {
    // 上游由解析后的模型 id 决定（Qoder 的解析结果保留 `qoder/` 前缀，Antigravity 不带），
    // 这样 run_protocol 的入参不会再多一个。
    let kind = bridge.upstream_for(&model);
    let (tx, mut rx) = mpsc::channel::<PumpMsg>(64);
    let task_bridge = Arc::clone(&bridge);
    let task_model = model.clone();
    let task_session_key = session_key.clone();
    tokio::spawn(async move {
        // 正文快照由这里持有：pump 往里写，写日志时读
        let mut capture = BodyCapture::new(task_bridge.body_limit);
        let outcome = pump(PumpInput {
            bridge: &task_bridge,
            kind,
            protocol: &mut *protocol,
            inner: &inner,
            repair: repair.as_ref(),
            model: &task_model,
            session_key: &task_session_key,
            tx: &tx,
            capture: &mut capture,
            warnings: &mut ctx.warnings,
        })
        .await;
        let stats = protocol.stats();
        finish_log(&task_bridge, ctx, &outcome, &stats, &capture).await;
        // tx 关掉，body 流自然收尾
        drop(tx);
    });

    match rx.recv().await {
        Some(PumpMsg::Head(headers)) => {
            let mut builder = Response::builder()
                .status(200)
                .header("content-type", "text/event-stream; charset=utf-8")
                .header("cache-control", "no-cache, no-transform")
                .header("connection", "keep-alive");
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
            builder
                .body(body_from_messages(rx))
                .unwrap_or_else(|_| Response::new(Body::empty()))
        }
        Some(PumpMsg::Json {
            status,
            body,
            headers,
        }) => {
            let extra: Vec<(&str, String)> = headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone()))
                .collect();
            json_response(status, &body, &extra)
        }
        _ => json_response(
            502,
            &json!({ "type": "error", "error": { "type": "api_error", "message": "桥内部错误：没拿到任何上游事件" } }),
            &[],
        ),
    }
}

/// 把 pump 的正文消息变成一个 HTTP body 流（收到 Head 之后就调它）。
fn body_from_messages(rx: mpsc::Receiver<PumpMsg>) -> Body {
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        let mut collected = String::new();
        loop {
            match rx.recv().await {
                Some(PumpMsg::Body(text)) => {
                    collected.push_str(&text);
                    break;
                }
                Some(PumpMsg::Head(_)) => continue,
                Some(PumpMsg::Json { .. }) => {
                    if collected.is_empty() {
                        return None;
                    }
                    break;
                }
                None => {
                    if collected.is_empty() {
                        return None;
                    }
                    break;
                }
            }
        }
        Some((
            Ok::<Bytes, std::convert::Infallible>(Bytes::from(collected)),
            rx,
        ))
    });
    Body::from_stream(stream)
}

/// 这次请求声明了哪些工具：泄漏修复用它当白名单（没声明过的名字不当调用）。
/// 空列表和 JS 的空 Set 一样「不设白名单」，所以返回 `None`。
fn declared_names(inner: &Value) -> Option<Vec<String>> {
    let declarations = inner
        .get("tools")
        .and_then(Value::as_array)
        .and_then(|tools| tools.first())
        .and_then(|tool| tool.get("functionDeclarations"))
        .and_then(Value::as_array)?;
    let names: Vec<String> = declarations
        .iter()
        .filter_map(|decl| decl.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

/// 模型替换后的展示名（日志与 message_start 的 model 字段用同一个）。
fn model_label(resolved: &ResolvedModel) -> String {
    match &resolved.substituted_from {
        Some(from) => format!("{} (桥梁自 {})", resolved.model, from),
        None => resolved.model.clone(),
    }
}

/// 把上游模型解析失败如实报出去（不编一个默认模型顶上）。
fn no_models_response() -> Response {
    json_response(
        502,
        &json!({
            "type": "error",
            "error": { "type": "api_error", "message": "拿不到上游模型表（所有账号都没成）" },
        }),
        &[],
    )
}

pub async fn handle_messages(bridge: Arc<Bridge>, mut body: Value) -> Response {
    let started_at = now_millis();
    let mut warnings: Vec<String> = Vec::new();

    let removed = sanitize_messages(&mut body);
    if removed > 0 {
        bridge.counters.sanitized();
        warnings.push(format!("sanitized:{removed}"));
        (bridge.log)(format!("请求修复：删掉 {removed} 条孤儿 tool 消息"));
    }
    let filled = fill_missing_tool_results(&mut body);
    if filled > 0 {
        bridge.counters.filled();
        warnings.push(format!("filled_tool_results:{filled}"));
        (bridge.log)(format!("请求修复：补了 {filled} 条占位 tool_result"));
    }

    let session_key = derive_session_key(&body);
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let kind = bridge.upstream_for(&requested);
    if kind == UpstreamKind::Qoder && bridge.qoder().is_none() {
        return json_response(
            502,
            &json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": "Qoder 上游未启用（模型带 qoder/ 前缀，但配置里没有开启 qoder）",
                },
            }),
            &[],
        );
    }
    let Some(resolved) = bridge.pool_of(kind).resolve(&requested).await else {
        return no_models_response();
    };
    if let Some(from) = &resolved.substituted_from {
        (bridge.log)(format!(
            "模型替换（{}）：客户端要 {from}，上游用 {}",
            resolved.reason, resolved.model
        ));
    }
    let limits = crate::server::model_limits_for(&bridge, &resolved.model).await;
    let built = build_gemini_request(
        &body,
        BuildOptions {
            signatures: Some(bridge.store()),
            session_key: &session_key,
            limits: limits.as_ref(),
        },
    );
    bridge.counters.signatures(
        built.stats.tool_signature_hits as u64,
        built.stats.tool_signature_misses as u64,
    );
    warnings.extend(built.warnings.iter().cloned());

    // Anthropic 的规矩：不写 stream 就是整包 JSON，只有 stream:true 才走 SSE。
    // 老桥也是这个行为（不带 stream 的 curl 拿到的是 JSON），所以这里跟协议走。
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let translator = AnthropicTranslator::new(AnthropicTranslatorOptions {
        model: model_label(&resolved),
        signatures: Arc::clone(bridge.store()),
        session_key: session_key.clone(),
        message_id: String::new(),
        declared_tools: declared_names(&built.inner),
    });
    let protocol: Box<dyn ProtocolAdapter> = Box::new(AnthropicAdapter {
        translator,
        stream,
        buffered_events: Vec::new(),
    });
    let request = if bridge.log_bodies {
        request_snapshot(&body, bridge.body_limit)
    } else {
        Value::Null
    };
    let ctx = LogCtx {
        kind: "messages",
        requested_model: Some(requested),
        model: resolved.model.clone(),
        stream,
        session_key: session_key.clone(),
        warnings,
        tool_signature_hits: built.stats.tool_signature_hits,
        tool_signature_misses: built.stats.tool_signature_misses,
        started_at,
        request,
    };
    run_protocol(
        bridge,
        protocol,
        built.inner.clone(),
        repair_variant(&built.inner),
        resolved.model,
        session_key,
        ctx,
    )
    .await
}

pub async fn handle_chat_completions(bridge: Arc<Bridge>, mut body: Value) -> Response {
    let started_at = now_millis();
    let mut warnings: Vec<String> = Vec::new();

    let removed = sanitize_messages(&mut body);
    if removed > 0 {
        bridge.counters.sanitized();
        warnings.push(format!("sanitized:{removed}"));
        (bridge.log)(format!("请求修复：删掉 {removed} 条孤儿 tool 消息"));
    }
    let filled = fill_missing_openai_tool_results(&mut body);
    if filled > 0 {
        bridge.counters.filled();
        warnings.push(format!("filled_tool_results:{filled}"));
        (bridge.log)(format!("请求修复：补了 {filled} 条占位 tool 结果"));
    }

    let session_key = derive_session_key(&body);
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let kind = bridge.upstream_for(&requested);
    if kind == UpstreamKind::Qoder && bridge.qoder().is_none() {
        return json_response(
            502,
            &json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": "Qoder 上游未启用（模型带 qoder/ 前缀，但配置里没有开启 qoder）",
                },
            }),
            &[],
        );
    }
    let Some(resolved) = bridge.pool_of(kind).resolve(&requested).await else {
        return no_models_response();
    };
    if let Some(from) = &resolved.substituted_from {
        (bridge.log)(format!(
            "模型替换（{}）：客户端要 {from}，上游用 {}",
            resolved.reason, resolved.model
        ));
    }
    let limits = crate::server::model_limits_for(&bridge, &resolved.model).await;
    let built = build_gemini_request_from_openai(
        &body,
        OpenAiBuildOptions {
            signatures: Some(bridge.store()),
            session_key: &session_key,
            limits: limits.as_ref(),
        },
    );
    bridge.counters.signatures(
        built.stats.tool_signature_hits as u64,
        built.stats.tool_signature_misses as u64,
    );
    warnings.extend(built.warnings.iter().cloned());

    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let include_usage = body
        .get("stream_options")
        .and_then(|opts| opts.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let translator = OpenAiTranslator::new(OpenAiTranslatorOptions {
        model: model_label(&resolved),
        signatures: Arc::clone(bridge.store()),
        session_key: session_key.clone(),
        include_usage,
        declared_tools: declared_names(&built.inner),
    });
    let protocol: Box<dyn ProtocolAdapter> = Box::new(OpenAiAdapter {
        translator,
        stream,
        buffered_chunks: Vec::new(),
    });
    let request = if bridge.log_bodies {
        request_snapshot(&body, bridge.body_limit)
    } else {
        Value::Null
    };
    let ctx = LogCtx {
        kind: "chat.completions",
        requested_model: Some(requested),
        model: resolved.model.clone(),
        stream,
        session_key: session_key.clone(),
        warnings,
        tool_signature_hits: built.stats.tool_signature_hits,
        tool_signature_misses: built.stats.tool_signature_misses,
        started_at,
        request,
    };
    run_protocol(
        bridge,
        protocol,
        built.inner.clone(),
        repair_variant(&built.inner),
        resolved.model,
        session_key,
        ctx,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_names_reads_the_first_tool_block() {
        let inner = json!({
            "tools": [{ "functionDeclarations": [{ "name": "Read" }, { "name": "Grep" }] }],
        });
        assert_eq!(
            declared_names(&inner),
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
        // 没声明工具 = 不设白名单（和 JS 的空 Set 一个意思）
        assert_eq!(declared_names(&json!({})), None);
        assert_eq!(declared_names(&json!({ "tools": [] })), None);
        assert_eq!(
            declared_names(&json!({ "tools": [{ "functionDeclarations": [] }] })),
            None
        );
    }

    #[test]
    fn stats_become_the_js_shape_for_budget_note() {
        let stats = TranslatorStats {
            finish_reason: Some("MAX_TOKENS".into()),
            thoughts_tokens: Some(900),
            output_tokens: Some(1000),
            ..Default::default()
        };
        assert_eq!(
            budget_note(&stats_as_js(&stats)).as_deref(),
            Some("thoughts_ate_max_tokens")
        );
        let empty = TranslatorStats::default();
        assert_eq!(budget_note(&stats_as_js(&empty)), None);
    }

    fn attempt(status: u16, account: &str) -> Attempt {
        Attempt {
            account: account.to_string(),
            why: "quota",
            status,
            reason: "x".into(),
        }
    }

    #[test]
    fn repair_variant_is_only_built_when_the_client_did_not_ask_for_thinking() {
        let plain = json!({ "generationConfig": { "maxOutputTokens": 4096 } });
        let repaired = repair_variant(&plain).expect("没提思考就该造得出修复体");
        assert_eq!(
            repaired["generationConfig"]["thinkingConfig"],
            json!({ "thinkingBudget": 0 })
        );
        // 客户端明确要了思考：不造（不许偷偷把它关掉）
        assert!(repair_variant(&json!({
            "generationConfig": { "thinkingConfig": { "thinkingBudget": 2048 } }
        }))
        .is_none());
        // 客户端自己就关着思考：形状本来就一样，造出来无害
        assert!(repair_variant(&json!({
            "generationConfig": { "thinkingConfig": { "thinkingBudget": 0 } }
        }))
        .is_some());
        // 没有 generationConfig：不 panic，也造不出来
        assert!(repair_variant(&json!({})).is_none());
    }

    #[test]
    fn attempts_map_to_an_honest_error_shape() {
        // 两个账号都被限流：429 + 最早可重试时间
        let mapped = map_attempts(
            &[attempt(429, "a@x"), attempt(429, "b@x")],
            Some(&UpstreamError::http(429, "RESOURCE_EXHAUSTED", "quota")),
            "",
            Some(42),
        );
        assert_eq!(mapped.http, 429);
        assert_eq!(mapped.kind, "rate_limit_error");
        assert_eq!(mapped.retry_after_seconds, Some(42));

        // 两个账号都连不上：502 api_error，不能说成限流
        let mapped = map_attempts(
            &[attempt(0, "a@x"), attempt(0, "b@x")],
            Some(&UpstreamError::network(
                "fetch failed",
                "connection refused",
            )),
            "",
            Some(42),
        );
        assert_eq!(mapped.http, 502);
        assert_eq!(mapped.kind, "api_error");
        assert!(mapped.message.contains("连不上上游"), "{}", mapped.message);
        assert_eq!(mapped.retry_after_seconds, None);

        // 账号级 + 网络混着：还是 429（确实有账号被上游拒了）
        let mapped = map_attempts(
            &[attempt(429, "a@x"), attempt(0, "b@x")],
            Some(&UpstreamError::network("fetch failed", "boom")),
            "",
            Some(7),
        );
        assert_eq!(mapped.http, 429);
        assert_eq!(mapped.retry_after_seconds, Some(7));

        // 空回合：502 + 说清是「没正文」而不是「没额度」
        let mapped = map_attempts(
            &[attempt(200, "a@x")],
            Some(&UpstreamError::network(
                "empty_turn",
                "上游 200 但没有任何正文（空回合）",
            )),
            "",
            None,
        );
        assert_eq!(mapped.http, 502);
        assert_eq!(mapped.kind, "api_error");
        assert!(mapped.message.contains("空回合"), "{}", mapped.message);

        // 400 这种换号也没用的：照上游状态码出去
        let mapped = map_attempts(
            &[attempt(400, "a@x")],
            Some(&UpstreamError::http(400, "INVALID_ARGUMENT", "bad schema")),
            "",
            None,
        );
        assert_eq!(mapped.http, 400);
        assert_eq!(mapped.kind, "invalid_request_error");
    }

    #[test]
    fn model_label_marks_substitutions() {
        let resolved = ResolvedModel {
            model: "gemini-3.6-flash-high".into(),
            substituted_from: Some("claude-sonnet-4-5".into()),
            reason: "family_fallback".into(),
        };
        assert_eq!(
            model_label(&resolved),
            "gemini-3.6-flash-high (桥梁自 claude-sonnet-4-5)"
        );
        let plain = ResolvedModel {
            substituted_from: None,
            ..resolved
        };
        assert_eq!(model_label(&plain), "gemini-3.6-flash-high");
    }

    #[test]
    fn attempts_serialize_like_the_js_log() {
        let attempt = Attempt {
            account: "a@b.com".into(),
            why: "sticky",
            status: 429,
            reason: "RATE_LIMIT".into(),
        };
        let value = attempt.to_json();
        assert_eq!(value["account"], "a@b.com");
        assert_eq!(value["why"], "sticky");
        assert_eq!(value["status"], 429);
        assert_eq!(value["reason"], "RATE_LIMIT");
    }

    #[test]
    fn request_snapshot_redacts_blobs_and_respects_limit() {
        let big_blob = "A".repeat(300);
        let body = json!({
            "model": "claude-3-5",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": { "type": "base64", "data": big_blob }
                }]
            }]
        });
        let snap = request_snapshot(&body, 0);
        assert!(!snap["json"].as_str().unwrap().contains(&"A".repeat(300)));
        assert!(snap["json"]
            .as_str()
            .unwrap()
            .contains("字符的二进制数据，没记"));
        assert_eq!(snap["truncated"], false);

        let snap_truncated = request_snapshot(&body, 20);
        assert_eq!(snap_truncated["truncated"], true);
        assert_eq!(snap_truncated["json"].as_str().unwrap().chars().count(), 20);
    }
}
