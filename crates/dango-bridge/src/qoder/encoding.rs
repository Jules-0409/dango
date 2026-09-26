//! Qoder 私有 base64：标准 base64 → 换字母表 → 三段轮转 → `=` 换 `$`。
//!
//! 这是签名覆盖的「请求体字节」：COSY 的 sig 和 bodyhash 都算在**编码后**的字节上，
//! 所以不能拿明文去签。语义照抄 PoC `/tmp/qoder-poc/qoder-client.mjs` 的 `encodeBody`，
//! 以及 MIT 参考项目 `simonsmh/pi-provider-qoder/src/protocol/encoding.ts`。
//!
//! 三段轮转等价于 `std[末尾 a 段] + std[中间段] + std[开头 a 段]`，其中 `a = floor(n/3)`，
//! `n` 是 base64 字符串长度（`n < 3` 时 `a = 0`，就是整串）。

use base64::Engine;

/// 标准字母表（`Buffer.toString("base64")` 用的那套）。
const STD_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Qoder 自己的字母表（逐位对应标准字母表）。
const CUSTOM_ALPHABET: &[u8; 64] =
    b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";

/// 逐字节映射表：默认恒等，标准字母表的 64 个字符换到自定义字母表，`=` 换成 `$`。
const fn build_table() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        table[i] = i as u8;
        i += 1;
    }
    let mut j = 0usize;
    while j < 64 {
        table[STD_ALPHABET[j] as usize] = CUSTOM_ALPHABET[j];
        j += 1;
    }
    table[b'=' as usize] = b'$';
    table
}

const ENCODE_TABLE: [u8; 256] = build_table();

/// 把明文（任意字节）编成 Qoder 要的请求体字节。
///
/// 返回的是 ASCII 字节，不是 `String`：签名/bodyhash 都按字节算，中间别经过 UTF-8 反复编解码。
pub fn encode_body(plaintext: &[u8]) -> Vec<u8> {
    let std_text = base64::engine::general_purpose::STANDARD.encode(plaintext);
    let std_bytes = std_text.as_bytes();
    let n = std_bytes.len();
    let a = n / 3;

    let mut out = Vec::with_capacity(n);
    // 末尾 a 段提前
    for &byte in &std_bytes[n - a..] {
        out.push(ENCODE_TABLE[byte as usize]);
    }
    // 中间段
    for &byte in &std_bytes[a..n - a] {
        out.push(ENCODE_TABLE[byte as usize]);
    }
    // 开头 a 段挪到最后
    for &byte in &std_bytes[..a] {
        out.push(ENCODE_TABLE[byte as usize]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 期望值全部由 PoC 的 JS 编码器生成后硬编码，不是 Rust 自己算出来再自己断言。
    /// 生成脚本见任务记录：标准 base64 → 换表 → 三段轮转 → `=`→`$`。
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(text: &str) -> Vec<u8> {
        let chars: Vec<char> = text.chars().collect();
        chars
            .chunks(2)
            .map(|pair| {
                let hi = pair[0].to_digit(16).expect("十六进制高位");
                let lo = pair[1].to_digit(16).expect("十六进制低位");
                (hi * 16 + lo) as u8
            })
            .collect()
    }

    #[test]
    fn empty_body_stays_empty() {
        assert!(encode_body(b"").is_empty());
    }

    #[test]
    fn hello_matches_poc_bytes() {
        // JS: Buffer.from("hello").toString("base64") = "aGVsbG8="，轮转后 "q$FruHPH"
        let out = encode_body(b"hello");
        assert_eq!(out, b"q$FruHPH");
        assert_eq!(hex(&out), "7124467275485048");
    }

    #[test]
    fn short_inputs_exercise_the_padding_swap() {
        // "abc" → "MSK#"（无补齐），"abcd" → "$$KMD_#S"（含 $ 补齐符）
        assert_eq!(encode_body(b"abc"), b"MSK#");
        assert_eq!(encode_body(b"abcd"), b"$$KMD_#S");
        assert_eq!(hex(&encode_body(b"abcd")), "24244b4d445f2353");
    }

    #[test]
    fn chinese_json_matches_poc_bytes() {
        // 直接用那段 JSON 的字节（不能走 serde_json：它会把键按字典序重排，
        // 而期望 hex 是按 JS 的插入顺序生成的）
        let payload = r#"{"messages":[{"role":"user","content":"你好，世界"}]}"#.as_bytes();
        let expected = unhex(
            "2826515053575858595651472a535151535642452e4a6570242442794245465e4478422a476f4b4d7528517744535177424d6e2a51476d594b7444786a5e23534a4c4e594279536b722a4e4f5772442c",
        );
        assert_eq!(encode_body(payload), expected);
    }

    #[test]
    fn binary_bytes_match_poc_bytes() {
        let input = [0x00u8, 0x01, 0x02, 0xff, 0xfe, 0x80, 0x7f];
        // "AAEC//6Afw==" → "ef$$!!y___To"
        let out = encode_body(&input);
        assert_eq!(out, b"ef$$!!y___To");
        assert_eq!(hex(&out), "656624242121795f5f5f546f");
    }

    #[test]
    fn every_input_byte_is_ascii_and_no_standard_padding_survives() {
        let payload = serde_json::to_vec(&serde_json::json!({ "a": 1, "b": "x" })).expect("序列化");
        let out = encode_body(&payload);
        assert_eq!(out.len(), payload.len().div_ceil(3) * 4);
        assert!(out.iter().all(|b| b.is_ascii()));
        assert!(!out.contains(&b'='), "补齐符必须换成 $");
        assert!(
            !out.contains(&b'+') && !out.contains(&b'/'),
            "标准字母表不该漏出来"
        );
    }
}
