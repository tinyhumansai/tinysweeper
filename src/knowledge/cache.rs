//! The content-hash cache in front of the extraction model call.
//!
//! Always compiled; no I/O of its own.
//!
//! Keyed on the hash of the file's *content*, never on its path or on the
//! repository. That is what makes one extraction per unique file content, ever:
//! a fork whose `AGENTS.md` is byte-identical to its upstream's is answered
//! from the cache, and so is every subsequent push that did not touch the file
//! — which is nearly all of them. A cache keyed on the repository would miss
//! both of those and pay for a model call on every push.
//!
//! Storing the *result* of extraction rather than the file also means the cache
//! holds only post-validation bullets, so nothing that failed structural
//! validation is ever replayed out of it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Cached extraction results, keyed by content hash.
///
/// Cheap to clone: every clone shares one map.
#[derive(Debug, Clone, Default)]
pub struct RuleCache {
    entries: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

/// The process-wide cache.
///
/// The server is long-lived and reviews the same repositories repeatedly, so
/// the cache being global is the difference between paying for extraction once
/// and paying for it per review. Tests that want isolation construct their own
/// with [`RuleCache::new`].
static GLOBAL: OnceLock<RuleCache> = OnceLock::new();

impl RuleCache {
    /// An empty, unshared cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-wide cache.
    pub fn global() -> Self {
        GLOBAL.get_or_init(RuleCache::default).clone()
    }

    /// The rules extracted from content with this hash, if it has been seen.
    ///
    /// An empty `Vec` is a real answer — "this file yielded no rules" — and is
    /// cached exactly like a non-empty one, so a file with nothing to say is
    /// not re-extracted on every push.
    pub fn get(&self, content_hash: &str) -> Option<Vec<String>> {
        self.entries
            .lock()
            .expect("rule cache lock")
            .get(content_hash)
            .cloned()
    }

    /// Record the rules extracted from content with this hash.
    pub fn put(&self, content_hash: &str, rules: Vec<String>) {
        self.entries
            .lock()
            .expect("rule cache lock")
            .insert(content_hash.to_string(), rules);
    }

    /// How many distinct file contents have been extracted.
    pub fn len(&self) -> usize {
        self.entries.lock().expect("rule cache lock").len()
    }

    /// Whether nothing has been cached yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_that_was_never_seen_misses() {
        let cache = RuleCache::new();
        assert!(cache.get("nope").is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn an_empty_result_is_cached_as_an_answer() {
        // Otherwise a file with no actionable rules is re-extracted forever,
        // which is the common case and the expensive one.
        let cache = RuleCache::new();
        cache.put("abc", Vec::new());
        assert_eq!(cache.get("abc"), Some(Vec::new()));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn rules_round_trip_and_clones_share_one_map() {
        let cache = RuleCache::new();
        let other = cache.clone();
        other.put("h", vec!["Never unwrap in library code.".into()]);
        assert_eq!(
            cache.get("h"),
            Some(vec!["Never unwrap in library code.".to_string()])
        );
    }

    #[test]
    fn the_global_cache_is_one_instance() {
        assert_eq!(RuleCache::global().len(), RuleCache::global().len());
        RuleCache::global().put("global-test", vec!["x".into()]);
        assert_eq!(
            RuleCache::global().get("global-test"),
            Some(vec!["x".into()])
        );
    }
}
