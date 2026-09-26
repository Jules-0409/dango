//! 把「泄漏成文本」的伪工具调用认出来。
//!
//! 上游（Gemini）偶尔不按规范给 functionCall part，而是把调用写成正文里的一段方言：
//!   <call:default_api:Grep{context:5,output_mode:content,path:/x,pattern:<svg}
//! 桥原样放行的话，客户端收到的是纯文本、没有 tool_use，回合就此空转。
//! 服务层必须在往客户端吐字之前认出来，并翻成真正的 tool_use 块。
//!
//! 为什么参数要自己解析，而不是丢给 JSON.parse：这段方言是「不带引号的 JS 字面量」，
//! 键名和字符串都可以裸写，本来就不是合法 JSON。所以 parse_args 先按顶层逗号切、
//! 再按顶层冒号切，值做一次尽力而为的类型归一（能不能归成数字/布尔看形状）。
//!
//! 移植自 `src/bridge/leak-repair.mjs`（那边在 byok-proxy 的真实故障上验证过），
//! 逻辑与协议无关。行为基准是 JS 版：那几个看起来奇怪的阈值（MAX_NAME_LEN /
//! MAX_BRACE_WAIT）和三态判断（null / pending / 完整）都照搬，不要顺手改。
//!
//! 白名单（「这次请求声明过的工具才当调用」）不在这里：JS 版把它放在
//! anthropic-stream.mjs / openai.mjs 的翻译器里，对着本模块吐出的调用名字再判一次。
//! 本模块只负责「认出来」。

use serde::Serialize;
use serde_json::{Map, Value};

/// 调用标记。上游把伪调用写成 `<call:名字>{...}`。
pub const CALL_MARKER: &str = "<call:";

/// 调用名的合理上限；超过就把 "<call:" 当普通文本，避免误吞正文。
const MAX_NAME_LEN: usize = 64;
/// 见到 marker 之后等 `{` 的耐心值：正文里恰好出现 "<call:" 时不能无限扣着。
const MAX_BRACE_WAIT: usize = 80;
/// 缓冲上限默认值（对应 JS 构造函数的 `maxBuffer = 256 * 1024`）。
const DEFAULT_MAX_BUFFER: usize = 256 * 1024;

/// `scan_call` 的结果，形状对齐 JS 返回的那个对象。
///
/// JS 有三态：`null` / `{pending:true,start,end}`（还没看到 `{`）/ 完整对象。
/// Rust 用 `Option` 表示 null；用 `raw_name` / `name` / `input` / `raw` 全为 `None`
/// 表示「还没看到 `{`」那一态。`pending` 在两种「先扣着」的情况（没看到 `{`、
/// 括号没闭合）里都是 true，和 JS 消费者的 `call.pending || !call.complete` 等价。
///
/// `start` / `end` 是**字节**下标（Rust 切片必须用字节），JS 那边是 UTF-16 下标；
/// 但内部的阈值比较仍按 UTF-16 码元数走（见 `js_len`），保证「算不算调用」的判定一致。
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedCall {
    pub start: usize,
    pub end: usize,
    /// 括号配平、参数已经解析完
    pub complete: bool,
    /// 信息不足，需要更多数据（没看到 `{`，或括号还没闭合）
    pub pending: bool,
    /// 名字原文，未规范化
    pub raw_name: Option<String>,
    /// `normalize_name` 的结果
    pub name: Option<String>,
    /// 只有 `complete` 才有值（JS 里不完整时是 null）
    pub input: Option<Value>,
    /// 整段原文
    pub raw: Option<String>,
}

/// 从 `start` 处扫描一个调用。
///
/// `None` = 不是调用（当普通文本）；`Some(call)` 里 `pending` 表示还看不出来。
pub fn scan_call(text: &str, start: usize) -> Option<ScannedCall> {
    // JS 用下标访问字符串不会越界/断码元；Rust 必须在切片前挡住（start 落在多字节
    // 字符中间时会 panic）。这是移植带来的差异，不是逻辑差异。
    if start > text.len() || !text.is_char_boundary(start) {
        return None;
    }
    if !text[start..].starts_with(CALL_MARKER) {
        return None;
    }

    let name_start = start + CALL_MARKER.len();
    let Some(brace_rel) = text[name_start..].find('{') else {
        // 还没看到参数括号。等太久就放弃（当普通文本），否则先扣着。
        if js_len(&text[start..]) > MAX_BRACE_WAIT {
            return None;
        }
        return Some(ScannedCall {
            start,
            end: text.len(),
            complete: false,
            pending: true,
            raw_name: None,
            name: None,
            input: None,
            raw: None,
        });
    };
    let brace_idx = name_start + brace_rel;

    // 名字长得离谱：多半不是调用，是正文。
    if js_len(&text[start..brace_idx]) > MAX_NAME_LEN + CALL_MARKER.len() {
        return None;
    }
    let raw_name = text[name_start..brace_idx].trim().to_string();
    // 名字里有空白或 `<` 说明这不是「<call:名字{」，当正文。
    if raw_name.is_empty() || raw_name.chars().any(|c| c.is_whitespace() || c == '<') {
        return None;
    }

    let end_brace = match_brace(text, brace_idx);
    let complete = end_brace.is_some();
    let (end, args_text) = match end_brace {
        Some(close) => (close + 1, &text[brace_idx + 1..close]),
        // 括号还没闭合：也可能是流还没发完，交给调用方决定是等还是当正文。
        None => (text.len(), &text[brace_idx + 1..]),
    };
    Some(ScannedCall {
        start,
        end,
        complete,
        pending: !complete,
        raw_name: Some(raw_name.clone()),
        name: normalize_name(&raw_name),
        input: if complete {
            Some(parse_args(args_text))
        } else {
            None
        },
        raw: Some(text[start..end].to_string()),
    })
}

/// `default_api:Grep` → `Grep`。
///
/// JS 版总能给出字符串（最差是空串）；这里空名字（例如 `":"`）返回 `None`。
/// 调用方按 `unwrap_or_default()` 取用时拿到的是空串，交付出去的行为和 JS 一致。
pub fn normalize_name(raw: &str) -> Option<String> {
    let name = match raw.rfind(':') {
        Some(i) => &raw[i + 1..],
        None => raw,
    };
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 「不带引号的 JS 字面量」风格参数 → 对象。
pub fn parse_args(args_text: &str) -> Value {
    let s = args_text.trim();
    let mut out = Map::new();
    if s.is_empty() {
        return Value::Object(out);
    }
    for part in split_top_level(s) {
        let piece = part.trim();
        if piece.is_empty() {
            continue;
        }
        let Some(colon) = find_top_level_colon(piece) else {
            continue;
        };
        let key = unquote(piece[..colon].trim());
        if key.is_empty() {
            continue;
        }
        out.insert(key, coerce_value(piece[colon + 1..].trim()));
    }
    Value::Object(out)
}

/// 流式过滤器：把「还没确认是不是调用」的尾巴先扣住，确认了就把调用交给调用方。
///
/// 关键点是不能漏字也不能吞字：`<call:` 可能被切在两个 chunk 之间（"…<ca" + "ll:…"），
/// 所以每次都要把「可能是 marker 前缀」的尾巴留在缓冲区里，下一次再判断。
#[derive(Debug)]
pub struct LeakFilter {
    buf: String,
    inside: bool,
    max_buffer: usize,
}

impl Default for LeakFilter {
    fn default() -> Self {
        Self::with_max_buffer(DEFAULT_MAX_BUFFER)
    }
}

/// 过滤器吐出的一件事。和 JS 里 `feed()` 返回的 `{text, calls}` 对应：
/// 正文合并成一条 `Text`（排在前面），每个修复好的调用一条 `ToolCall`。
///
/// 序列化形状（给别的模块当契约用）：
///   `{"type":"text","text":"…"}`
///   `{"type":"tool_call","name":"Grep","input":{…},"raw":"<call:…>"}`
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LeakEvent {
    Text {
        text: String,
    },
    ToolCall {
        name: String,
        input: Value,
        raw: String,
    },
}

impl LeakFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_buffer(max_buffer: usize) -> Self {
        Self {
            buf: String::new(),
            inside: false,
            max_buffer,
        }
    }

    /// 喂一段文本，返回这一轮能确定的事件（正文先出，调用跟在后面，和 JS 一致）。
    pub fn push(&mut self, chunk: &str) -> Vec<LeakEvent> {
        if !chunk.is_empty() {
            self.buf.push_str(chunk);
        }
        // JS 把正文累积成一个字符串、调用收集成数组再一起返回；这里也这样攒，
        // 最后先出一条 Text、再依次出 ToolCall，顺序和 JS 消费者看到的一样。
        let mut text = String::new();
        let mut calls: Vec<LeakEvent> = Vec::new();
        loop {
            if !self.inside {
                match self.buf.find(CALL_MARKER) {
                    None => {
                        // 结尾可能是 marker 的半截（"<ca"），留到下次再判断。
                        let keep = partial_marker_tail(&self.buf);
                        let cut = self.buf.len() - keep;
                        if cut > 0 {
                            text.push_str(&self.buf[..cut]);
                        }
                        if keep > 0 {
                            self.buf.drain(..cut);
                        } else {
                            self.buf.clear();
                        }
                        break;
                    }
                    Some(idx) => {
                        if idx > 0 {
                            text.push_str(&self.buf[..idx]);
                            self.buf.drain(..idx);
                        }
                        self.inside = true;
                        continue;
                    }
                }
            }

            // 两种「还不能下结论」：参数还没开始（pending），或者参数还没收完（!complete）。
            // 后者在流结束时由 finish() 当正文放出来 —— 截断的输出不该被猜成调用。
            match scan_call(&self.buf, 0) {
                Some(call) if call.pending || !call.complete => {
                    if js_len(&self.buf) > self.max_buffer {
                        text.push_str(&self.buf);
                        self.buf.clear();
                        self.inside = false;
                    }
                    break;
                }
                None => {
                    // 只是正文里恰好出现了 "<call:"，原样吐出去，继续看后面。
                    text.push_str(CALL_MARKER);
                    self.buf.drain(..CALL_MARKER.len());
                    self.inside = false;
                    continue;
                }
                Some(call) => {
                    calls.push(LeakEvent::ToolCall {
                        name: call.name.unwrap_or_default(),
                        input: call.input.unwrap_or(Value::Null),
                        raw: call.raw.unwrap_or_default(),
                    });
                    self.buf.drain(..call.end);
                    self.inside = false;
                }
            }
        }

        let mut events = Vec::with_capacity(calls.len() + 1);
        if !text.is_empty() {
            events.push(LeakEvent::Text { text });
        }
        events.append(&mut calls);
        events
    }

    /// 流结束时把扣住的东西全放出来（不完整的调用也当正文，不猜）。
    ///
    /// 返回空表示没有残留；JS 的 `flush()` 返回空串，消费者也是这么判的。
    pub fn finish(&mut self) -> Vec<LeakEvent> {
        let rest = std::mem::take(&mut self.buf);
        self.inside = false;
        if rest.is_empty() {
            Vec::new()
        } else {
            vec![LeakEvent::Text { text: rest }]
        }
    }
}

/// JS 的 `String.prototype.length` 数的是 UTF-16 码元，不是字节也不是码点。
/// 阈值判定照它来，含非 ASCII 的尾巴才不会和 JS 版判得不一样。
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// buf 结尾有几位是 marker 的前缀（"<ca" 这种），那几位要留到下次再判断。
///
/// marker 全是 ASCII，所以「码元数」和「字节数」在这里是一回事，直接用字节。
fn partial_marker_tail(s: &str) -> usize {
    let max = (CALL_MARKER.len() - 1).min(s.len());
    for n in (1..=max).rev() {
        if s.ends_with(&CALL_MARKER[..n]) {
            return n;
        }
    }
    0
}

/// 括号配平；认得引号里的内容（pattern 里带 `}` 很常见，不能算成闭合）。
/// 返回配对括号的**字节**下标；不配对返回 `None`。
fn match_brace(text: &str, open_idx: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut quote: Option<char> = None;
    let mut iter = text[open_idx..].char_indices();
    while let Some((off, ch)) = iter.next() {
        let idx = open_idx + off;
        match quote {
            Some(q) => {
                if ch == '\\' {
                    // 反斜杠转义：连同下一个字符一起跳过（JS 的 `if (ch === "\\") i++`）
                    iter.next();
                } else if ch == q {
                    quote = None;
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                } else if ch == '{' || ch == '[' || ch == '(' {
                    depth += 1;
                } else if ch == '}' || ch == ']' || ch == ')' {
                    depth -= 1;
                    if depth == 0 {
                        return Some(idx);
                    }
                    if depth < 0 {
                        return None;
                    }
                }
            }
        }
    }
    None
}

/// 按顶层（括号外、引号外）的逗号切分。
fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth: i32 = 0;
    let mut quote: Option<char> = None;
    let mut last = 0usize;
    let mut iter = s.char_indices();
    while let Some((i, ch)) = iter.next() {
        match quote {
            Some(q) => {
                if ch == '\\' {
                    iter.next();
                } else if ch == q {
                    quote = None;
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                } else if ch == '{' || ch == '[' || ch == '(' {
                    depth += 1;
                } else if ch == '}' || ch == ']' || ch == ')' {
                    depth -= 1;
                } else if ch == ',' && depth == 0 {
                    parts.push(&s[last..i]);
                    last = i + 1;
                }
            }
        }
    }
    parts.push(&s[last..]);
    parts
}

/// 找顶层（括号外、引号外）的冒号。
fn find_top_level_colon(s: &str) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut quote: Option<char> = None;
    let mut iter = s.char_indices();
    while let Some((i, ch)) = iter.next() {
        match quote {
            Some(q) => {
                if ch == '\\' {
                    iter.next();
                } else if ch == q {
                    quote = None;
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                } else if ch == '{' || ch == '[' || ch == '(' {
                    depth += 1;
                } else if ch == '}' || ch == ']' || ch == ')' {
                    depth -= 1;
                } else if ch == ':' && depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}

/// 去掉一层引号。双引号走 JSON 解析（能吃转义），单引号只消 `\'`。
fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    let first = bytes.first().copied();
    let last = bytes.last().copied();
    let quoted = bytes.len() >= 2
        && ((first == Some(b'"') && last == Some(b'"'))
            || (first == Some(b'\'') && last == Some(b'\'')));
    if !quoted {
        return s.to_string();
    }
    let inner = &s[1..s.len() - 1];
    if first == Some(b'"') {
        match serde_json::from_str::<Value>(s) {
            Ok(Value::String(text)) => text,
            // JSON 不认（比如没闭合的转义）：退回去掉引号的原样
            _ => inner.to_string(),
        }
    } else {
        inner.replace("\\'", "'")
    }
}

/// 方言里的裸值 → JSON 值。
fn coerce_value(v: &str) -> Value {
    if v.is_empty() {
        return Value::String(String::new());
    }
    match v {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" | "undefined" => return Value::Null,
        _ => {}
    }
    if is_int_literal(v) {
        return int_value(v);
    }
    if is_decimal_literal(v) {
        return num_value(v);
    }
    let bytes = v.as_bytes();
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
        return Value::String(unquote(v));
    }
    if first == b'{' || first == b'[' {
        if let Ok(parsed) = serde_json::from_str::<Value>(v) {
            return parsed;
        }
        // 方言里的嵌套对象/数组也是「不带引号的 JS 字面量」，递归解析一次。
        if first == b'{' {
            return parse_args(strip_wrap(v, b'}'));
        }
        let inner = strip_wrap(v, b']');
        let arr = split_top_level(inner)
            .into_iter()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(coerce_value)
            .collect();
        return Value::Array(arr);
    }
    Value::String(v.to_string())
}

/// 去掉最外层的一对包裹字符；结尾没有闭合符时就只去掉开头那一个。
/// 对应 JS 的 `v.slice(1, v.endsWith("}") ? -1 : undefined)`。
fn strip_wrap(v: &str, close: u8) -> &str {
    let end = if v.as_bytes().last() == Some(&close) {
        v.len() - 1
    } else {
        v.len()
    };
    &v[1..end]
}

/// `/^-?\d+$/`（JS 的 `\d` 就是 ASCII 数字）。
fn is_int_literal(v: &str) -> bool {
    let digits = v.strip_prefix('-').unwrap_or(v);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// `/^-?\d+\.\d+$/`：正好一个小数点，两边都是十进制数字。
fn is_decimal_literal(v: &str) -> bool {
    let digits = v.strip_prefix('-').unwrap_or(v);
    let Some((int_part, frac_part)) = digits.split_once('.') else {
        return false;
    };
    !int_part.is_empty()
        && !frac_part.is_empty()
        && int_part.bytes().all(|b| b.is_ascii_digit())
        && frac_part.bytes().all(|b| b.is_ascii_digit())
}

/// JS 的数字都是 f64，但大于 2^53 的整数会丢精度；这里先按整数解析。
/// 参数里出现的都是下标/计数这种小整数，够用且输出不带小数点（JS 也不会带）。
fn int_value(v: &str) -> Value {
    if let Ok(n) = v.parse::<i64>() {
        return Value::from(n);
    }
    if let Ok(n) = v.parse::<u64>() {
        return Value::from(n);
    }
    num_value(v)
}

/// `Number(v)` 之后再序列化：JS 会把 `1.0` 写成 `1`，serde_json 会写成 `1.0`，
/// 所以整数值退回整数类型，别让输出的 JSON 和 JS 版对不上。
fn num_value(v: &str) -> Value {
    match v.parse::<f64>() {
        Ok(f) => from_js_number(f),
        Err(_) => Value::String(v.to_string()),
    }
}

fn from_js_number(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 {
        return Value::from(f as i64);
    }
    Value::from(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `test/quota-and-leak-guard.test.mjs` 里的原文。
    const LEAK_TEXT: &str = "<call:default_api:Grep{pattern:hello}";

    fn texts(events: &[LeakEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                LeakEvent::Text { text } => Some(text.as_str()),
                LeakEvent::ToolCall { .. } => None,
            })
            .collect()
    }

    /// 服务层翻译器对过滤器输出的处理，照抄 JS 的 anthropic-stream.mjs / openai.mjs：
    /// 有白名单而且名字不在里面 → 只回显原文（不算调用）；否则先回显原文再补一个真调用。
    /// 返回 (回显的正文, 修补出的调用, leakedCalls, leaksIgnored)。
    fn apply_whitelist(
        events: &[LeakEvent],
        declared: Option<&[&str]>,
    ) -> (String, Vec<LeakEvent>, usize, usize) {
        let mut text = String::new();
        let mut repaired = Vec::new();
        let (mut leaked_calls, mut leaks_ignored) = (0usize, 0usize);
        for ev in events {
            match ev {
                LeakEvent::Text { text: t } => text.push_str(t),
                LeakEvent::ToolCall { name, input, raw } => {
                    if let Some(allowed) = declared {
                        if !allowed.is_empty() && !allowed.contains(&name.as_str()) {
                            leaks_ignored += 1;
                            text.push_str(raw);
                            continue;
                        }
                    }
                    leaked_calls += 1;
                    text.push_str(raw);
                    repaired.push(LeakEvent::ToolCall {
                        name: name.clone(),
                        input: input.clone(),
                        raw: raw.clone(),
                    });
                }
            }
        }
        (text, repaired, leaked_calls, leaks_ignored)
    }

    fn run_filter(
        chunk: &str,
        declared: Option<&[&str]>,
    ) -> (String, Vec<LeakEvent>, usize, usize) {
        let mut filter = LeakFilter::new();
        let mut events = filter.push(chunk);
        events.extend(filter.finish());
        apply_whitelist(&events, declared)
    }

    // ---------------- scan_call

    #[test]
    fn scan_call_reads_name_and_args() {
        let call = scan_call(LEAK_TEXT, 0).expect("recognized");
        assert_eq!(call.start, 0);
        assert_eq!(call.end, LEAK_TEXT.len());
        assert!(call.complete);
        assert!(!call.pending);
        assert_eq!(call.raw_name.as_deref(), Some("default_api:Grep"));
        assert_eq!(call.name.as_deref(), Some("Grep"));
        assert_eq!(call.input, Some(json!({ "pattern": "hello" })));
        assert_eq!(call.raw.as_deref(), Some(LEAK_TEXT));
    }

    #[test]
    fn scan_call_ignores_plain_text() {
        assert!(scan_call("hello", 0).is_none());
        assert!(scan_call("<cal", 0).is_none());
        // 不是从 start 处开始就不算
        assert!(scan_call("x<call:f{}", 0).is_none());
        assert!(scan_call("x<call:f{}", 1).is_some());
        // start 落在多字节字符中间：宁可当正文，也不能 panic
        assert!(scan_call("中<call:f{}", 1).is_none());
    }

    #[test]
    fn scan_call_holds_until_the_brace_shows_up() {
        // 对应 JS 的 {pending:true}：名字都还没开始，需要更多数据
        let call = scan_call("<call:Gre", 0).expect("pending");
        assert!(call.pending);
        assert!(!call.complete);
        assert_eq!(call.start, 0);
        assert_eq!(call.end, 9);
        assert_eq!(call.raw_name, None);
        assert_eq!(call.name, None);
        assert_eq!(call.input, None);
        assert_eq!(call.raw, None);
    }

    #[test]
    fn scan_call_gives_up_when_the_marker_is_just_text() {
        // 没有 `{`、又已经等过 MAX_BRACE_WAIT：当普通文本
        let no_brace = format!("<call:{}", "G".repeat(MAX_BRACE_WAIT + 1));
        assert!(scan_call(&no_brace, 0).is_none());
        // 名字太长也是普通文本
        let long_name = format!("<call:{}", "G".repeat(MAX_NAME_LEN + 1));
        assert!(scan_call(&format!("{long_name}{{}}"), 0).is_none());
    }

    #[test]
    fn scan_call_rejects_bad_names() {
        assert!(scan_call("<call:{}", 0).is_none(), "空名字");
        assert!(scan_call("<call:a b{}", 0).is_none(), "名字里有空白");
        assert!(scan_call("<call:<x{}", 0).is_none(), "名字里有 '<'");
    }

    #[test]
    fn scan_call_reports_an_unclosed_brace_as_pending() {
        let text = "<call:default_api:Grep{pattern:hello";
        let call = scan_call(text, 0).expect("recognized");
        assert!(call.pending);
        assert!(!call.complete);
        assert_eq!(call.end, text.len());
        assert_eq!(call.name.as_deref(), Some("Grep"));
        assert_eq!(call.input, None);
        assert_eq!(call.raw.as_deref(), Some(text));
    }

    #[test]
    fn scan_call_matches_braces_inside_quotes() {
        // pattern 里带 `}` 很常见，不能算成参数结束
        let text = r#"<call:f{p:"a}b"}"#;
        let call = scan_call(text, 0).expect("recognized");
        assert!(call.complete);
        assert_eq!(call.input, Some(json!({ "p": "a}b" })));
        assert_eq!(call.raw.as_deref(), Some(text));

        // 转义引号：反斜杠之后那个字符不算引号
        let escaped = r#"<call:f{p:"a\"b"}"#;
        let call = scan_call(escaped, 0).expect("recognized");
        assert!(call.complete);
        assert_eq!(call.input, Some(json!({ "p": "a\"b" })));
    }

    // ---------------- normalize_name / parse_args

    #[test]
    fn normalize_name_strips_the_namespace() {
        assert_eq!(normalize_name("default_api:Grep").as_deref(), Some("Grep"));
        assert_eq!(normalize_name("Grep").as_deref(), Some("Grep"));
        // JS 对 ":" 给出空串；这里用 None 表示「没有可用名字」，
        // 调用方 unwrap_or_default() 之后拿到的还是空串。
        assert_eq!(normalize_name(":"), None);
    }

    #[test]
    fn parse_args_matches_the_js_dialect() {
        // 探针实测的形态：键不带引号、值是裸字面量、pattern 里还有 `<`
        let input = parse_args("context:5,output_mode:content,path:/x,pattern:<svg");
        assert_eq!(
            input,
            json!({ "context": 5, "output_mode": "content", "path": "/x", "pattern": "<svg" })
        );
        assert_eq!(parse_args(""), json!({}));
        assert_eq!(parse_args("   "), json!({}));
        // 没有顶层冒号的片段直接丢（JS 的 `if (c === -1) continue`）
        assert_eq!(parse_args("garbage"), json!({}));
    }

    #[test]
    fn parse_args_coerces_values_like_js() {
        let input = parse_args(
            r#"a:true,b:false,c:null,d:undefined,e:"quoted,comma",f:-3,g:2.5,h:1.0,i:'single',j:{k:1},l:[1,2]"#,
        );
        assert_eq!(
            input,
            json!({
                "a": true, "b": false, "c": null, "d": null,
                "e": "quoted,comma", "f": -3, "g": 2.5, "h": 1, "i": "single",
                "j": {"k": 1}, "l": [1, 2],
            })
        );
    }

    #[test]
    fn parse_args_recovers_from_broken_json_values() {
        // `{"x":1` 不是合法 JSON：递归回退到方言解析器（JS 的 catch 分支）
        assert_eq!(parse_args(r#"a:{"x":1"#), json!({ "a": { "x": 1 } }));
        assert_eq!(parse_args("a:[1,2"), json!({ "a": [1, 2] }));
        // 合法 JSON 优先走 JSON 解析
        assert_eq!(parse_args(r#"a:{"x":1}"#), json!({ "a": { "x": 1 } }));
        // 引号没闭合、JSON 也不是合法值：原样当字符串
        assert_eq!(
            parse_args(r#"a:"unterminated"#),
            json!({ "a": "\"unterminated" })
        );
    }

    // ---------------- LeakFilter

    #[test]
    fn filter_repairs_a_declared_tool() {
        // 对应 JS：「声明过这个工具时：泄漏文本被修成 tool_use」
        let (text, repaired, leaked, ignored) = run_filter(LEAK_TEXT, Some(&["Grep", "Read"]));
        assert_eq!(repaired.len(), 1);
        assert_eq!(leaked, 1);
        assert_eq!(ignored, 0);
        // 先如实回显原文，再补一个真调用（客户端两边都看得到）
        assert_eq!(text, LEAK_TEXT);
        match &repaired[0] {
            LeakEvent::ToolCall { name, input, raw } => {
                assert_eq!(name, "Grep");
                assert_eq!(input, &json!({ "pattern": "hello" }));
                assert_eq!(raw, LEAK_TEXT);
            }
            other => panic!("expected a repaired tool call, got {other:?}"),
        }
    }

    #[test]
    fn filter_leaves_an_undeclared_tool_as_text() {
        // 对应 JS：「没声明过这个工具时：只当正文，不造 tool_use」
        let (text, repaired, leaked, ignored) = run_filter(LEAK_TEXT, Some(&["Read"]));
        assert!(repaired.is_empty());
        assert_eq!(leaked, 0);
        assert_eq!(ignored, 1);
        assert_eq!(text, LEAK_TEXT);
    }

    #[test]
    fn filter_repairs_when_the_caller_gave_no_whitelist() {
        // 对应 JS：「调用方没给白名单时，行为跟以前一样（照修）」
        let (text, repaired, leaked, ignored) = run_filter(LEAK_TEXT, None);
        assert_eq!(repaired.len(), 1);
        assert_eq!(leaked, 1);
        assert_eq!(ignored, 0);
        assert_eq!(text, LEAK_TEXT);
    }

    #[test]
    fn text_around_a_call_comes_out_first_and_joined() {
        // JS 的返回是 {text, calls}：正文合成一条（排在前面），调用排在后面
        let mut filter = LeakFilter::new();
        let events = filter.push("before <call:f{a:1} after");
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            LeakEvent::Text {
                text: "before  after".to_string()
            }
        );
        match &events[1] {
            LeakEvent::ToolCall { name, input, raw } => {
                assert_eq!(name, "f");
                assert_eq!(input, &json!({ "a": 1 }));
                assert_eq!(raw, "<call:f{a:1}");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        // 收尾没有残留
        assert!(filter.finish().is_empty());
    }

    #[test]
    fn marker_with_a_bad_name_round_trips_as_text() {
        let mut filter = LeakFilter::new();
        let mut events = filter.push("<call:a b{x}");
        events.extend(filter.finish());
        assert_eq!(texts(&events), "<call:a b{x}");
        assert!(!events
            .iter()
            .any(|e| matches!(e, LeakEvent::ToolCall { .. })));
    }

    #[test]
    fn marker_split_across_chunks_is_not_emitted_early() {
        // 半截 marker 必须扣住：不能先吐 "…<ca" 再吐 "ll:…"
        let mut filter = LeakFilter::new();
        assert_eq!(
            filter.push("hi <ca"),
            vec![LeakEvent::Text {
                text: "hi ".to_string()
            }]
        );

        let out = filter.push("ll:default_api:Grep{pattern:hello}");
        assert_eq!(out.len(), 1);
        match &out[0] {
            LeakEvent::ToolCall { name, input, .. } => {
                assert_eq!(name, "Grep");
                assert_eq!(input, &json!({ "pattern": "hello" }));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        assert!(filter.finish().is_empty());
    }

    #[test]
    fn split_marker_that_never_becomes_a_call_is_released() {
        let mut filter = LeakFilter::new();
        assert!(filter.push("<ca").is_empty());
        assert!(filter.push("ll:no args yet").is_empty());
        // 攒过 MAX_BRACE_WAIT 还没见到 `{`：放弃，当正文整段放出来
        let filler = "x".repeat(MAX_BRACE_WAIT);
        let expected = format!("<call:no args yet{filler}");
        assert_eq!(
            filter.push(&filler),
            vec![LeakEvent::Text { text: expected }]
        );
    }

    #[test]
    fn finish_releases_an_unfinished_call_as_text() {
        let mut filter = LeakFilter::new();
        let text = "<call:default_api:Grep{pattern:hello";
        // 括号没闭合：不猜成调用，先扣着
        assert!(filter.push(text).is_empty());
        assert_eq!(
            filter.finish(),
            vec![LeakEvent::Text {
                text: text.to_string()
            }]
        );
        // 收尾之后缓冲区是空的
        assert!(filter.finish().is_empty());
    }

    #[test]
    fn oversized_pending_call_is_flushed_as_text() {
        // maxBuffer 是安全阀：迟迟不闭合的 "<call:" 不能无限占内存
        let mut filter = LeakFilter::with_max_buffer(16);
        let buf = "<call:aaaaaaaaaaaa";
        assert!(buf.len() > 16);
        assert_eq!(
            filter.push(buf),
            vec![LeakEvent::Text {
                text: buf.to_string()
            }]
        );
        assert!(filter.finish().is_empty());
    }

    #[test]
    fn leak_event_serializes_with_a_type_tag() {
        // 事件形状是别的模块要消费的契约，钉死它
        let mut filter = LeakFilter::new();
        let events = filter.push("hi <call:Grep{pattern:x}");
        assert_eq!(
            serde_json::to_value(&events[0]).unwrap(),
            json!({ "type": "text", "text": "hi " })
        );
        assert_eq!(
            serde_json::to_value(&events[1]).unwrap(),
            json!({ "type": "tool_call", "name": "Grep", "input": { "pattern": "x" }, "raw": "<call:Grep{pattern:x}" })
        );
    }
}
