use crate::probes::credentials::load_devin_token;
use crate::probes::models::DevinQuota;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE,
};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// Minimal Protobuf encoder / decoder
fn encode_varint(mut val: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    while val > 127 {
        buf.push(((val & 0x7f) | 0x80) as u8);
        val >>= 7;
    }
    buf.push(val as u8);
    buf
}

fn encode_tag(field: u32, wire: u8) -> Vec<u8> {
    encode_varint(((field as u64) << 3) | (wire as u64))
}

fn encode_string(field: u32, val: &str) -> Vec<u8> {
    let b = val.as_bytes();
    let mut out = encode_tag(field, 2);
    out.extend(encode_varint(b.len() as u64));
    out.extend_from_slice(b);
    out
}

fn encode_message(field: u32, inner: &[u8]) -> Vec<u8> {
    let mut out = encode_tag(field, 2);
    out.extend(encode_varint(inner.len() as u64));
    out.extend_from_slice(inner);
    out
}

fn decode_varint(buf: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    while pos < buf.len() {
        let byte = buf[pos];
        pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        shift += 7;
        if (byte & 0x80) == 0 {
            return Some((result, pos));
        }
        if shift > 64 {
            return None;
        }
    }
    None
}

#[derive(Debug, Clone)]
enum PbValue<'a> {
    Varint(u64),
    LengthDelimited(&'a [u8]),
}

#[derive(Debug, Clone)]
struct PbField<'a> {
    field: u32,
    value: PbValue<'a>,
}

fn parse_proto(buf: &[u8]) -> Vec<PbField<'_>> {
    let mut fields = Vec::new();
    let mut pos = 0;

    while pos < buf.len() {
        let (tag, next_pos) = match decode_varint(buf, pos) {
            Some(res) => res,
            None => break,
        };
        pos = next_pos;

        let field = (tag >> 3) as u32;
        let wire = (tag & 0x07) as u8;
        if field == 0 {
            break;
        }

        match wire {
            0 => {
                if let Some((v, p)) = decode_varint(buf, pos) {
                    pos = p;
                    fields.push(PbField {
                        field,
                        value: PbValue::Varint(v),
                    });
                } else {
                    break;
                }
            }
            2 => {
                if let Some((len, p)) = decode_varint(buf, pos) {
                    let len = len as usize;
                    if p + len <= buf.len() {
                        fields.push(PbField {
                            field,
                            value: PbValue::LengthDelimited(&buf[p..p + len]),
                        });
                        pos = p + len;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            5 => {
                if pos + 4 <= buf.len() {
                    fields.push(PbField {
                        field,
                        value: PbValue::LengthDelimited(&buf[pos..pos + 4]),
                    });
                    pos += 4;
                } else {
                    break;
                }
            }
            1 => {
                if pos + 8 <= buf.len() {
                    fields.push(PbField {
                        field,
                        value: PbValue::LengthDelimited(&buf[pos..pos + 8]),
                    });
                    pos += 8;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }

    fields
}

fn get_varint(fields: &[PbField], target_field: u32) -> Option<u64> {
    fields.iter().find_map(|f| {
        if f.field == target_field {
            if let PbValue::Varint(v) = f.value {
                return Some(v);
            }
        }
        None
    })
}

fn get_sub(fields: &[PbField], target_field: u32) -> Option<Vec<u8>> {
    fields.iter().find_map(|f| {
        if f.field == target_field {
            if let PbValue::LengthDelimited(slice) = f.value {
                return Some(slice.to_vec());
            }
        }
        None
    })
}

fn get_string(fields: &[PbField], target_field: u32) -> Option<String> {
    get_sub(fields, target_field).and_then(|bytes| String::from_utf8(bytes).ok())
}

fn get_timestamp(fields: &[PbField], target_field: u32) -> Option<i64> {
    let sub = get_sub(fields, target_field)?;
    let parsed = parse_proto(&sub);
    get_varint(&parsed, 1).map(|v| v as i64)
}

pub async fn fetch_devin_live(
    client: &reqwest::Client,
    token: &str,
    source: &str,
) -> Result<DevinQuota, String> {
    let mut metadata = Vec::new();
    // 客户端身份如实填写。原来这里硬编码自称 Devin 自家的 "chisel" 客户端、
    // 版本 "0.0.0-dev"、平台 "windows"（在 macOS 上也照发）。
    // 2026-09-17 实测：服务端不校验这些字段，如实填写与谎报解析出的字段和数值完全一致。
    metadata.extend(encode_string(1, "quota-panel"));
    metadata.extend(encode_string(2, env!("CARGO_PKG_VERSION")));
    metadata.extend(encode_string(3, token));
    metadata.extend(encode_string(4, "en"));
    metadata.extend(encode_string(5, std::env::consts::OS));
    metadata.extend(encode_string(7, env!("CARGO_PKG_VERSION")));

    let body = encode_message(1, &metadata);

    let mut headers = HeaderMap::new();
    let auth_val = format!("Basic {}-{}", token, token);
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&auth_val).map_err(|e| e.to_string())?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/proto"));
    headers.insert("connect-protocol-version", HeaderValue::from_static("1"));
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string()).unwrap(),
    );

    // 网络错误 / 5xx / 429 自动重试（最多 3 次尝试，300ms、900ms 指数退避）。
    // RequestBuilder 是一次性的，重试时要用 headers / body 的克隆重新构造。
    let send_result = crate::probes::http::send_with_retry(|| {
        client
            .post("https://server.codeium.com/exa.seat_management_pb.SeatManagementService/GetUserStatus")
            .headers(headers.clone())
            .body(body.clone())
    })
    .await;

    let resp = send_result.map_err(|e| format!("devin.request_failed: {}", e))?;

    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err("devin.auth_expired".into());
    }
    if !status.is_success() {
        return Err(format!("devin.http_status: {}", status));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("devin.body_read_failed: {}", e))?;

    let top = parse_proto(&bytes);
    let user_status_bytes = get_sub(&top, 1).unwrap_or_default();
    let user_status = parse_proto(&user_status_bytes);

    let email = get_string(&user_status, 7);
    let user_id = get_string(&user_status, 36);
    let team_id = get_string(&user_status, 5);

    let plan_status_bytes = get_sub(&user_status, 13).unwrap_or_default();
    let plan_status = parse_proto(&plan_status_bytes);

    let plan_info_bytes = get_sub(&plan_status, 1).unwrap_or_default();
    let plan_info = parse_proto(&plan_info_bytes);
    let plan_name = get_string(&plan_info, 2);

    let daily_remaining_percent = get_varint(&plan_status, 14).map(|v| v as f64);
    let weekly_remaining_percent = get_varint(&plan_status, 15).map(|v| v as f64);
    let overage_balance_micros = get_varint(&plan_status, 16).map(|v| v as i64);
    let daily_reset_at_unix = get_varint(&plan_status, 17).map(|v| v as i64);
    let weekly_reset_at_unix = get_varint(&plan_status, 18).map(|v| v as i64);
    let acu_consumed = get_varint(&plan_status, 19).map(|v| v as i64);
    let acu_limit = get_varint(&plan_status, 20).map(|v| v as i64);

    let plan_start_unix = get_timestamp(&plan_status, 2);
    let plan_end_unix = get_timestamp(&plan_status, 3);

    Ok(DevinQuota {
        ok: true,
        id: "devin".into(),
        name: "Devin".into(),
        plan_name,
        email: email.as_deref().map(crate::models::mask_email),
        user_id,
        team_id,
        daily_remaining_percent,
        weekly_remaining_percent,
        daily_reset_at_unix,
        weekly_reset_at_unix,
        overage_balance_micros,
        acu_consumed,
        acu_limit,
        plan_start_unix,
        plan_end_unix,
        source: Some(source.to_string()),
        fetched_at: now_millis(),
        error: None,
    })
}

pub async fn fetch_devin(client: &reqwest::Client) -> DevinQuota {
    // load_devin_token 是同步阻塞的（读 credentials.toml、只读打开 state.vscdb），
    // 放到阻塞线程池里，别占住 tokio 的 worker 线程。
    // 手动凭据槽优先，Devin CLI / Desktop 的本地凭据兜底。
    let loaded = tokio::task::spawn_blocking(|| {
        crate::manual_creds::get("devin").map_or_else(load_devin_token, |token| {
            Ok((token, "手动凭据".to_string()))
        })
    })
    .await;

    let (token, source) = match loaded {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            return DevinQuota {
                ok: false,
                id: "devin".into(),
                name: "Devin".into(),
                error: Some(e),
                fetched_at: now_millis(),
                ..Default::default()
            };
        }
        Err(join_err) => {
            return DevinQuota {
                ok: false,
                id: "devin".into(),
                name: "Devin".into(),
                error: Some(format!("devin.credential_task_failed: {}", join_err)),
                fetched_at: now_millis(),
                ..Default::default()
            };
        }
    };

    let live_result = fetch_devin_live(client, &token, &source).await;

    match live_result {
        Ok(quota) => quota,
        Err(e) => DevinQuota {
            ok: false,
            id: "devin".into(),
            name: "Devin".into(),
            source: Some(source),
            error: Some(e),
            fetched_at: now_millis(),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// varint 的 7 位分组编码：多字节数必须正确往返。
    /// 用 protobuf 规范里的自有例子（300 -> AC 02）当锚点。
    #[test]
    fn varint_roundtrips() {
        for value in [
            0u64,
            1,
            127,
            128,
            300,
            16383,
            16384,
            u32::MAX as u64,
            u64::MAX,
        ] {
            let encoded = encode_varint(value);
            let (decoded, pos) = decode_varint(&encoded, 0).unwrap();
            assert_eq!(decoded, value, "value: {value}");
            assert_eq!(pos, encoded.len(), "value: {value}");
        }
    }

    /// 编码出的字节要符合 protobuf 规范，不能只是「自己解自己」能对上。
    /// 300 的 varint 是 0xAC 0x02，这是规范里的标准例子。
    #[test]
    fn varint_matches_protobuf_spec_bytes() {
        assert_eq!(encode_varint(0), vec![0x00]);
        assert_eq!(encode_varint(1), vec![0x01]);
        assert_eq!(encode_varint(127), vec![0x7f]);
        assert_eq!(encode_varint(128), vec![0x80, 0x01]);
        assert_eq!(encode_varint(300), vec![0xac, 0x02]);
    }

    /// 截断的 varint（高位置 1 但没后续字节）必须返回 None 而不是死循环。
    #[test]
    fn truncated_varint_returns_none() {
        assert_eq!(decode_varint(&[0x80], 0), None);
        assert_eq!(decode_varint(&[0x80, 0x80], 0), None);
        // 超过 64 位的连续高位也必须停下
        let overlong = vec![0x80u8; 20];
        assert_eq!(decode_varint(&overlong, 0), None);
    }

    /// 编码后的 tag 要能还原出字段号和 wire type。
    #[test]
    fn tag_encodes_field_and_wire_type() {
        // field 1, wire 2 (length-delimited) -> 0x0A
        assert_eq!(encode_tag(1, 2), vec![0x0a]);
        // field 7, wire 2 -> 0x3A
        assert_eq!(encode_tag(7, 2), vec![0x3a]);
        // field 15, wire 0 -> 0x78
        assert_eq!(encode_tag(15, 0), vec![0x78]);
    }

    /// 各种 wire type 的字段都要能解析出来，且 field 1 的
    /// length-delimited 值原样保留。
    #[test]
    fn parses_mixed_wire_types() {
        let mut body = Vec::new();
        body.extend(encode_string(1, "hello"));
        body.extend(encode_tag(2, 0));
        body.extend(encode_varint(150));

        let fields = parse_proto(&body);
        assert_eq!(get_string(&fields, 1).unwrap(), "hello");
        assert_eq!(get_varint(&fields, 2).unwrap(), 150);
        assert_eq!(
            get_string(&fields, 2),
            None,
            "字段 2 是 varint，不该被当成串"
        );
    }

    /// field number 0 是非法值，解析必须停住，不能无限循环。
    #[test]
    fn zero_field_number_stops_parsing() {
        // tag = 0x00 -> field 0, wire 0
        let fields = parse_proto(&[0x00, 0x01, 0x02]);
        assert!(fields.is_empty());
    }

    /// 声明了长度但数据不够时必须停下，不能越界读。
    /// 这是用手写解析器最容易出安全问题的地方：接口数据不可信。
    #[test]
    fn truncated_length_delimited_stops_parsing() {
        // field 1, length 10, 但只跟了 2 字节
        let fields = parse_proto(&[0x0a, 0x0a, 0x01, 0x02]);
        assert!(fields.is_empty(), "不完整的字段不应被接受");
    }

    /// GetUserStatus 的嵌套结构：field 13 里的 plan_status 各字段要能被取到，
    /// 且时间戳（field 2 / 3 是嵌套的 Timestamp{seconds}）要解出正确的秒数。
    #[test]
    fn parses_nested_plan_status() {
        // plan_info{2: "Team"} 作为 plan_status.field1
        let plan_info = encode_string(2, "Team");
        // plan_status 各字段
        let mut plan_status = Vec::new();
        plan_status.extend(encode_message(1, &plan_info));
        plan_status.extend(encode_tag(14, 0));
        plan_status.extend(encode_varint(25)); // daily remaining 25%
        plan_status.extend(encode_tag(15, 0));
        plan_status.extend(encode_varint(60)); // weekly remaining 60%
        plan_status.extend(encode_tag(16, 0));
        plan_status.extend(encode_varint(1234)); // overage micros
        plan_status.extend(encode_tag(17, 0));
        plan_status.extend(encode_varint(1_700_000_000)); // daily reset
        plan_status.extend(encode_tag(18, 0));
        plan_status.extend(encode_varint(1_700_500_000)); // weekly reset
        plan_status.extend(encode_tag(19, 0));
        plan_status.extend(encode_varint(7)); // acu consumed
        plan_status.extend(encode_tag(20, 0));
        plan_status.extend(encode_varint(20)); // acu limit
                                               // Timestamp{seconds: 1700000000} 放在 field 2 / 3
        let mut ts_start = encode_tag(1, 0);
        ts_start.extend(encode_varint(1_690_000_000));
        plan_status.extend(encode_message(2, &ts_start));
        let mut ts_end = encode_tag(1, 0);
        ts_end.extend(encode_varint(1_720_000_000));
        plan_status.extend(encode_message(3, &ts_end));

        // 包一层 user_status{13: plan_status}，再包一层顶层{1: user_status}
        let mut user_status = encode_string(7, "dev@example.com");
        user_status.extend(encode_message(13, &plan_status));
        let top = encode_message(1, &user_status);

        let parsed_top = parse_proto(&top);
        // get_sub 返回的是 owned Vec，parse_proto 借它，所以必须先绑成变量再解析
        let user_bytes = get_sub(&parsed_top, 1).unwrap();
        let user = parse_proto(&user_bytes);
        assert_eq!(get_string(&user, 7).unwrap(), "dev@example.com");

        let ps_bytes = get_sub(&user, 13).unwrap();
        let ps = parse_proto(&ps_bytes);
        let info_bytes = get_sub(&ps, 1).unwrap();
        let info = parse_proto(&info_bytes);
        assert_eq!(get_string(&info, 2).unwrap(), "Team");
        assert_eq!(get_varint(&ps, 14).unwrap(), 25);
        assert_eq!(get_varint(&ps, 15).unwrap(), 60);
        assert_eq!(get_varint(&ps, 16).unwrap(), 1234);
        assert_eq!(get_varint(&ps, 17).unwrap(), 1_700_000_000);
        assert_eq!(get_varint(&ps, 18).unwrap(), 1_700_500_000);
        assert_eq!(get_varint(&ps, 19).unwrap(), 7);
        assert_eq!(get_varint(&ps, 20).unwrap(), 20);
        assert_eq!(get_timestamp(&ps, 2).unwrap(), 1_690_000_000);
        assert_eq!(get_timestamp(&ps, 3).unwrap(), 1_720_000_000);
    }

    /// 非 UTF-8 的 length-delimited 值当字符串取时必须返回 None，不能 panic。
    #[test]
    fn invalid_utf8_string_is_none() {
        let mut body = encode_tag(1, 2);
        body.extend(encode_varint(2));
        body.extend_from_slice(&[0xff, 0xfe]);

        let fields = parse_proto(&body);
        assert_eq!(get_string(&fields, 1), None);
        // 但底层字节还在，取 sub 拿得到
        assert_eq!(get_sub(&fields, 1), Some(vec![0xff, 0xfe]));
    }

    /// 空响应（服务端返回 0 字节）不该 panic，只是什么都取不到。
    /// 注意这里没有超时保护也应是 O(1)。
    #[test]
    fn empty_response_yields_no_fields() {
        let fields = parse_proto(&[]);
        assert!(fields.is_empty());
        assert_eq!(get_varint(&fields, 14), None);
        assert_eq!(get_string(&fields, 7), None);
        assert_eq!(get_sub(&fields, 1), None);
    }

    /// 重复字段取第一个出现（`find_map` 语义），保证行为被测试锁住。
    #[test]
    fn duplicate_fields_use_first_occurrence() {
        let mut body = Vec::new();
        body.extend(encode_tag(14, 0));
        body.extend(encode_varint(11));
        body.extend(encode_tag(14, 0));
        body.extend(encode_varint(22));

        let fields = parse_proto(&body);
        assert_eq!(get_varint(&fields, 14), Some(11));
    }
}
