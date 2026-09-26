//! thoughtSignature 的存放处。
//!
//! 为什么需要它：上游要求「模型回合里的 functionCall part 必须带回它当初发的
//! thoughtSignature」，否则 400 INVALID_ARGUMENT（Phase 0 实测，见 PROTOCOL.md §7）。
//! 而 Anthropic 的 tool_use 块里没有签名字段，Factory 回传 tool_use 时签名就丢了 ——
//! 所以服务层必须在收到签名时按 tool_use.id 存下来，回传时再贴回去。
//!
//! 签名是二进制 base64，可长可短（实测 322 ~ 3228 字符）。只在内存里存，不落盘：
//! 它是上游的会话凭据，落盘没有收益、只有风险。
//!
//! 另外还存「尾部签名」：有些回合结束时上游会单独发一个只有签名、没有正文的 part，
//! 它属于整个回合，不属于某一次调用，按 session 存一个即可。

use std::collections::{HashMap, VecDeque};

use crate::types::now_millis;

/// 上游认这个哨兵值，等价于「我不校验签名」。签名实在没有时用它兜底。
pub const SIGNATURE_SENTINEL: &str = "skip_thought_signature_validator";

const DEFAULT_TTL_MS: i64 = 6 * 60 * 60 * 1000;
const DEFAULT_MAX: usize = 5000;

#[derive(Debug, Clone)]
struct Entry {
    signature: String,
    at: i64,
}

#[derive(Debug)]
pub struct SignatureStore {
    ttl_ms: i64,
    max_entries: usize,
    by_id: HashMap<String, Entry>,
    /// 插入顺序（JS 的 Map 就是插入序，淘汰从最旧的开始）。
    /// 同一个 id 覆盖写时不挪位置 —— 和 JS 的 Map 行为一致。
    order: VecDeque<String>,
    trailing: HashMap<String, Entry>,
    hits: u64,
    misses: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SignatureStats {
    pub ids: usize,
    pub sessions: usize,
    pub hits: u64,
    pub misses: u64,
}

impl Default for SignatureStore {
    fn default() -> Self {
        Self::new(DEFAULT_TTL_MS, DEFAULT_MAX)
    }
}

impl SignatureStore {
    pub fn new(ttl_ms: i64, max_entries: usize) -> Self {
        Self {
            ttl_ms,
            max_entries,
            by_id: HashMap::new(),
            order: VecDeque::new(),
            trailing: HashMap::new(),
            hits: 0,
            misses: 0,
        }
    }

    pub fn put(&mut self, id: &str, signature: &str) -> bool {
        self.put_at(id, signature, now_millis())
    }

    pub fn put_at(&mut self, id: &str, signature: &str, at: i64) -> bool {
        if id.is_empty() || signature.is_empty() {
            return false;
        }
        if !self.by_id.contains_key(id) {
            self.order.push_back(id.to_string());
        }
        self.by_id.insert(
            id.to_string(),
            Entry {
                signature: signature.to_string(),
                at,
            },
        );
        self.evict();
        true
    }

    /// 取签名；过期当作没有（过期后宁可用哨兵，也不送一个可能被拒的旧签名）。
    pub fn get(&mut self, id: &str) -> Option<String> {
        self.get_at(id, now_millis())
    }

    pub fn get_at(&mut self, id: &str, at: i64) -> Option<String> {
        let Some(entry) = self.by_id.get(id) else {
            self.misses += 1;
            return None;
        };
        if at - entry.at > self.ttl_ms {
            let signature = entry.signature.clone();
            drop(signature);
            self.by_id.remove(id);
            self.misses += 1;
            return None;
        }
        self.hits += 1;
        Some(entry.signature.clone())
    }

    pub fn put_trailing(&mut self, session_key: &str, signature: &str) {
        if session_key.is_empty() || signature.is_empty() {
            return;
        }
        self.trailing.insert(
            session_key.to_string(),
            Entry {
                signature: signature.to_string(),
                at: now_millis(),
            },
        );
    }

    pub fn take_trailing(&mut self, session_key: &str) -> Option<String> {
        let entry = self.trailing.get(session_key)?;
        if now_millis() - entry.at > self.ttl_ms {
            self.trailing.remove(session_key);
            return None;
        }
        Some(entry.signature.clone())
    }

    pub fn stats(&self) -> SignatureStats {
        SignatureStats {
            ids: self.by_id.len(),
            sessions: self.trailing.len(),
            hits: self.hits,
            misses: self.misses,
        }
    }

    /// 超上限就从最旧的开始丢。用 `order` 记插入序，map 里已经没有的 key 顺手跳过。
    fn evict(&mut self) {
        while self.by_id.len() > self.max_entries {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.by_id.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// 服务层里签名仓库是「请求翻译」和「流式翻译」两边共用的，所以套一层 Arc<Mutex>。
/// 用 std 的锁而不是 tokio 的：持有时间只有几条 map 操作，绝不在锁里 await。
pub type SharedSignatures = std::sync::Arc<std::sync::Mutex<SignatureStore>>;

pub fn shared_signatures() -> SharedSignatures {
    std::sync::Arc::new(std::sync::Mutex::new(SignatureStore::default()))
}

/// 锁中毒（别的线程 panic 在锁里）不该把服务带崩：拿回内部值继续用。
fn lock(store: &SharedSignatures) -> std::sync::MutexGuard<'_, SignatureStore> {
    store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn sig_put(store: &SharedSignatures, id: &str, signature: &str) -> bool {
    lock(store).put(id, signature)
}

pub fn sig_get(store: &SharedSignatures, id: &str) -> Option<String> {
    lock(store).get(id)
}

pub fn sig_put_trailing(store: &SharedSignatures, session_key: &str, signature: &str) {
    lock(store).put_trailing(session_key, signature)
}

pub fn sig_take_trailing(store: &SharedSignatures, session_key: &str) -> Option<String> {
    lock(store).take_trailing(session_key)
}

pub fn sig_stats(store: &SharedSignatures) -> SignatureStats {
    lock(store).stats()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_roundtrip() {
        let mut store = SignatureStore::default();
        assert!(store.put("call-1", "sig-a"));
        assert_eq!(store.get("call-1").as_deref(), Some("sig-a"));
        assert_eq!(store.stats().hits, 1);
        assert!(store.get("没有这个").is_none());
        assert_eq!(store.stats().misses, 1);
    }

    #[test]
    fn empty_id_or_signature_is_rejected() {
        let mut store = SignatureStore::default();
        assert!(!store.put("", "sig"));
        assert!(!store.put("id", ""));
        assert_eq!(store.stats().ids, 0);
    }

    #[test]
    fn expired_signature_reads_as_missing() {
        let mut store = SignatureStore::new(1000, 10);
        store.put_at("call-1", "sig", 5_000);
        assert!(store.get_at("call-1", 5_500).is_some());
        // 过期：当作没有，并且顺手删掉
        assert!(store.get_at("call-1", 6_100).is_none());
        assert_eq!(store.stats().ids, 0);
    }

    #[test]
    fn trailing_signature_is_per_session() {
        let mut store = SignatureStore::default();
        store.put_trailing("sess-1", "tail-a");
        assert_eq!(store.take_trailing("sess-1").as_deref(), Some("tail-a"));
        assert_eq!(store.take_trailing("sess-2"), None);
    }

    #[test]
    fn evicts_oldest_beyond_cap() {
        let mut store = SignatureStore::new(DEFAULT_TTL_MS, 3);
        for i in 0..5 {
            store.put(&format!("call-{i}"), &format!("sig-{i}"));
        }
        assert_eq!(store.stats().ids, 3);
        assert!(store.get("call-0").is_none());
        assert!(store.get("call-1").is_none());
        assert!(store.get("call-4").is_some());
    }

    #[test]
    fn overwriting_keeps_insertion_position() {
        // JS 的 Map：覆盖写不改变插入序，淘汰顺序按「第一次插入」
        let mut store = SignatureStore::new(DEFAULT_TTL_MS, 2);
        store.put("a", "sig-a");
        store.put("b", "sig-b");
        store.put("a", "sig-a2"); // 覆盖 a
        store.put("c", "sig-c"); // 挤掉最旧的 a
        assert!(store.get("a").is_none());
        assert_eq!(store.get("b").as_deref(), Some("sig-b"));
        assert!(store.get("c").is_some());
    }
}
