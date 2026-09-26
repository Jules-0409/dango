//! 手搓 protobuf：整条链路只用得上 varint 和 length-delimited 两种线格式，不引 prost。
//!
//! 为什么要自己写：上游的 `.proto` 没有公开，字段号是抓包实测算出来的（见 [`super`] 的注释），
//! 用 prost 反而要先造一份并不存在的 schema。编码只有「拼字段」，解码只需要「按字段号取值」。

/// 写一个 varint。
pub fn varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// 写字段标签（字段号 + 线格式）。
pub fn tag(out: &mut Vec<u8>, field: u32, wire: u8) {
    varint(out, ((field as u64) << 3) | wire as u64);
}

/// 写一个 varint 字段。
pub fn varint_field(out: &mut Vec<u8>, field: u32, v: u64) {
    tag(out, field, 0);
    varint(out, v);
}

/// 写一个 length-delimited 字段。
pub fn bytes_field(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    tag(out, field, 2);
    varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// 写一个字符串字段。
pub fn string_field(out: &mut Vec<u8>, field: u32, s: &str) {
    bytes_field(out, field, s.as_bytes());
}

/// 写一个嵌套消息：先序列化到临时缓冲，再整体作为 length-delimited 写进去。
pub fn message_field<F: FnOnce(&mut Vec<u8>)>(out: &mut Vec<u8>, field: u32, f: F) {
    let mut inner = Vec::new();
    f(&mut inner);
    bytes_field(out, field, &inner);
}

/// 读出来的一条字段。只认 varint 和 length-delimited。
#[derive(Debug, PartialEq, Eq)]
pub enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

impl<'a> Field<'a> {
    /// 当成嵌套消息继续读。
    pub fn reader(&self) -> Option<Reader<'a>> {
        match self {
            Field::Bytes(b) => Some(Reader::new(b)),
            Field::Varint(_) => None,
        }
    }

    /// 当成 UTF-8 字符串读。
    pub fn str(&self) -> Option<&'a str> {
        match self {
            Field::Bytes(b) => std::str::from_utf8(b).ok(),
            Field::Varint(_) => None,
        }
    }
}

/// 顺序读取若干字段。
///
/// 遇上看不懂的线格式（group、越界、截断）就直接停下并返回 `None`：上游加字段、换编码时
/// 桥退化成一个「这一帧没读到东西」，不该整轮报错。
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// 按字段号找第一个匹配的字段；找不到返回 `None`，解析没法继续也返回 `None`。
    pub fn find(&mut self, want: u32) -> Option<Field<'a>> {
        while let Some((field, value)) = self.next_field() {
            if field == want {
                return Some(value);
            }
        }
        None
    }

    pub fn next_field(&mut self) -> Option<(u32, Field<'a>)> {
        let key = self.read_varint()?;
        let field = (key >> 3) as u32;
        if field == 0 {
            return None;
        }
        match (key & 7) as u8 {
            0 => Some((field, Field::Varint(self.read_varint()?))),
            2 => {
                let len = self.read_varint()? as usize;
                let bytes = self.take(len)?;
                Some((field, Field::Bytes(bytes)))
            }
            // 固定 32/64 位：跳过，继续读后面的（用量字段里就有 fixed64）
            1 => {
                self.take(8)?;
                self.next_field()
            }
            5 => {
                self.take(4)?;
                self.next_field()
            }
            _ => None,
        }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        if end > self.buf.len() {
            return None;
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Some(slice)
    }

    fn read_varint(&mut self) -> Option<u64> {
        let mut shift = 0u32;
        let mut acc = 0u64;
        loop {
            if shift > 63 {
                return None;
            }
            let byte = *self.take(1)?.first()?;
            acc |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Some(acc);
            }
            shift += 7;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_matches_known_encodings() {
        let mut out = Vec::new();
        varint(&mut out, 0);
        assert_eq!(out, vec![0x00]);
        out.clear();
        varint(&mut out, 1);
        assert_eq!(out, vec![0x01]);
        out.clear();
        varint(&mut out, 300);
        assert_eq!(out, vec![0xac, 0x02]);
        out.clear();
        // 649 = 0x289，抓包里第一帧的长度前缀就是这个
        varint(&mut out, 649);
        assert_eq!(out, vec![0x89, 0x05]);
    }

    #[test]
    fn nested_message_round_trips() {
        let mut out = Vec::new();
        message_field(&mut out, 1, |m| {
            string_field(m, 1, "只回答三个字：在的");
            varint_field(m, 4, 1);
        });
        // 外层：tag=0x0a, len, 内容
        assert_eq!(out[0], 0x0a);
        let mut r = Reader::new(&out);
        let (field, value) = r.next_field().unwrap();
        assert_eq!(field, 1);
        let mut inner = value.reader().unwrap();
        assert_eq!(inner.find(1).unwrap().str().unwrap(), "只回答三个字：在的");
        let mut inner = Reader::new(match Reader::new(&out).next_field().unwrap().1 {
            Field::Bytes(b) => b,
            _ => panic!("应该是嵌套消息"),
        });
        assert!(matches!(inner.find(4), Some(Field::Varint(1))));
    }

    /// 固定宽字段要能跳过，不能被它打断后面的解析（用量帧里有 fixed64）。
    #[test]
    fn skips_fixed_width_fields() {
        let mut buf = Vec::new();
        tag(&mut buf, 8, 1);
        buf.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // fixed64
        string_field(&mut buf, 9, "后面还能读");
        let mut r = Reader::new(&buf);
        assert_eq!(r.find(9).unwrap().str().unwrap(), "后面还能读");
    }

    #[test]
    fn truncated_input_stops_without_panic() {
        let mut buf = Vec::new();
        string_field(&mut buf, 1, "abcdef");
        buf.truncate(buf.len() - 2);
        let mut r = Reader::new(&buf);
        assert!(r.next_field().is_none());
    }

    /// 真实抓包：一帧正文增量（`RunResponse.1.1.1 = "在"`，尾巴上还有个时间戳字段）。
    #[test]
    fn real_captured_text_delta_frame_decodes() {
        // 抓包里 res-f10.bin 的原始字节：1 { 1 { 1: "在" } 25: 时间戳 }
        let frame: &[u8] = &[
            0x0a, 0x0f, 0x0a, 0x05, 0x0a, 0x03, 0xe5, 0x9c, 0xa8, 0xc8, 0x01, 0xac, 0xb9, 0xcb,
            0xe3, 0x8b, 0x34,
        ];
        let mut r = Reader::new(frame);
        let mut event = r.find(1).unwrap().reader().unwrap();
        let mut text = event.find(1).unwrap().reader().unwrap();
        assert_eq!(text.find(1).unwrap().str().unwrap(), "在");
    }

    /// 真实抓包：思考帧走 `RunResponse.1.4.1`。
    #[test]
    fn real_captured_thinking_frame_decodes() {
        // res-f7.bin 原样（含尾巴上的时间戳和用量块，长度必须对得上）
        let frame: &[u8] = &[
            0x0a, 0x32, 0x22, 0x28, 0x0a, 0x24, 0xe7, 0x94, 0xa8, 0xe6, 0x88, 0xb7, 0xe8, 0xa6,
            0x81, 0xe6, 0xb1, 0x82, 0xe4, 0xbb, 0x85, 0xe7, 0x94, 0xa8, 0xe4, 0xb8, 0x89, 0xe4,
            0xb8, 0xaa, 0xe5, 0xad, 0x97, 0xe5, 0x9b, 0x9e, 0xe5, 0xa4, 0x8d, 0xe3, 0x80, 0x82,
            0x10, 0x01, 0xc8, 0x01, 0xaa, 0xb9, 0xcb, 0xe3, 0x8b, 0x34,
        ];
        let mut r = Reader::new(frame);
        let mut event = r.find(1).unwrap().reader().unwrap();
        let mut thinking = event.find(4).unwrap().reader().unwrap();
        assert_eq!(
            thinking.find(1).unwrap().str().unwrap(),
            "用户要求仅用三个字回复。"
        );
    }

    /// 真实抓包：用量帧走 `RunResponse.1.14`（输入 / 输出 / 两个缓存位）。
    #[test]
    fn real_captured_usage_frame_decodes() {
        let frame: &[u8] = &[
            0x0a, 0x0d, 0x72, 0x0b, 0x08, 0x91, 0x4a, 0x10, 0x23, 0x18, 0x00, 0x20, 0x00, 0x28,
            0x21,
        ];
        let mut r = Reader::new(frame);
        let mut event = r.find(1).unwrap().reader().unwrap();
        let mut usage = event.find(14).unwrap().reader().unwrap();
        assert_eq!(usage.find(1), Some(Field::Varint(9489)));
        assert_eq!(usage.find(2), Some(Field::Varint(35)));
    }

    /// 真实抓包：Connect 结束帧的 body 是 `{}`（正常结束，没有 error）。
    #[test]
    fn real_captured_trailer_is_empty_json() {
        assert_eq!(std::str::from_utf8(&[0x7b, 0x7d]).unwrap(), "{}");
    }
}
