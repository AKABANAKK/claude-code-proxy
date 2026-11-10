use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, RwLock};

use tiktoken_rs::o200k_base_singleton;

const CACHE_BYTES: usize = 16 * 1024 * 1024;
const CACHE_ENTRIES: usize = 4_096;
const MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;

struct TokenCache {
    counts: HashMap<std::sync::Arc<str>, u64>,
    order: VecDeque<std::sync::Arc<str>>,
    bytes: usize,
    byte_limit: usize,
    entry_limit: usize,
}

impl TokenCache {
    fn new(byte_limit: usize, entry_limit: usize) -> Self {
        Self {
            counts: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            byte_limit,
            entry_limit,
        }
    }

    fn get(&self, text: &str) -> Option<u64> {
        // HashMap also compares the complete text, so hash collisions cannot
        // return a token count for another input.
        self.counts.get(text).copied()
    }

    fn insert(&mut self, text: &str, count: u64) {
        if text.len() > self.byte_limit || self.entry_limit == 0 || self.counts.contains_key(text) {
            return;
        }
        while self.bytes + text.len() > self.byte_limit || self.counts.len() >= self.entry_limit {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if self.counts.remove(oldest.as_ref()).is_some() {
                self.bytes -= oldest.len();
            }
        }
        let text: std::sync::Arc<str> = text.into();
        self.bytes += text.len();
        self.order.push_back(text.clone());
        self.counts.insert(text, count);
    }
}

static CACHE: LazyLock<RwLock<TokenCache>> =
    LazyLock::new(|| RwLock::new(TokenCache::new(CACHE_BYTES, CACHE_ENTRIES)));

pub(super) fn token_count(text: &str) -> u64 {
    if text.len() <= MAX_ENTRY_BYTES
        && let Some(count) = CACHE.read().unwrap_or_else(|e| e.into_inner()).get(text)
    {
        return count;
    }
    // Tokenization never holds a cache lock. Independent requests can tokenize
    // misses concurrently, and large inputs cannot displace the whole cache.
    let count = u64::try_from(o200k_base_singleton().count_ordinary(text)).unwrap_or(u64::MAX);
    if text.len() <= MAX_ENTRY_BYTES {
        CACHE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(text, count);
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_counts_preserve_tokenizer_results_and_text_boundaries() {
        for text in [
            "",
            "hello",
            "hello world",
            "日本語の履歴です。",
            "\n\t",
            "{\"tool\":\"Read\"}",
        ] {
            let expected = o200k_base_singleton().count_ordinary(text) as u64;
            assert_eq!(token_count(text), expected);
            assert_eq!(token_count(text), expected);
        }
        assert_ne!(token_count("hello"), token_count("hello world"));
    }

    #[test]
    fn eviction_respects_both_limits_without_replacing_live_counts() {
        let mut cache = TokenCache::new(8, 2);
        cache.insert("one", 1);
        cache.insert("two", 2);
        cache.insert("one", 99);
        assert_eq!(cache.get("one"), Some(1));
        cache.insert("three", 3);
        assert_eq!(cache.get("one"), None);
        assert_eq!(cache.get("two"), Some(2));
        assert_eq!(cache.get("three"), Some(3));
        assert!(cache.bytes <= 8);
        assert!(cache.counts.len() <= 2);
        cache.insert("too large for this cache", 4);
        assert_eq!(cache.get("three"), Some(3));
    }
}
