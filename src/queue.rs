// Priority crawl queue.
//
// URLs sit in `pending` with a float priority score (higher = crawl sooner).
// `visited` tracks everything that has already been crawled this session so
// we never fetch the same URL twice.
//
// Thread-safe via DashMap — the background crawler and the on-demand search
// handler both push to the same queue.

use dashmap::DashMap;
use std::sync::Arc;

pub struct CrawlQueue {
    /// url -> priority score. Higher score = crawled sooner.
    pending: Arc<DashMap<String, f32>>,
    /// urls we have already crawled (or are actively crawling)
    visited: Arc<DashMap<String, ()>>,
}

impl CrawlQueue {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(DashMap::new()),
            visited: Arc::new(DashMap::new()),
        }
    }

    /// Add a URL to the queue if it hasn't been visited yet.
    /// If it's already pending with a lower score, update the score.
    pub fn push(&self, url: &str, score: f32) {
        if self.visited.contains_key(url) {
            return;
        }
        // Reject non-http URLs and obvious traps
        if !url.starts_with("http") {
            return;
        }
        if is_trap(url) {
            return;
        }
        self.pending
            .entry(url.to_string())
            .and_modify(|s| { if score > *s { *s = score; } })
            .or_insert(score);
    }

    /// Push a batch of URLs at the same priority.
    pub fn push_many(&self, urls: &[String], score: f32) {
        for url in urls {
            self.push(url, score);
        }
    }

    /// Pop up to `n` highest-priority URLs for crawling.
    /// Marks them as visited so they won't be returned again.
    pub fn pop_batch(&self, n: usize) -> Vec<String> {
        // Collect all pending items, sort by score descending
        let mut items: Vec<(String, f32)> = self
            .pending
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect();

        items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        items.truncate(n);

        let mut result = Vec::with_capacity(items.len());
        for (url, _) in items {
            self.pending.remove(&url);
            self.visited.insert(url.clone(), ());
            result.push(url);
        }
        result
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn visited_len(&self) -> usize {
        self.visited.len()
    }
}

/// Heuristics to avoid crawl traps — URLs that generate infinite pages.
fn is_trap(url: &str) -> bool {
    let lower = url.to_lowercase();

    // Calendar/date archive patterns
    if lower.contains("/calendar") || lower.contains("/archive/") {
        return true;
    }

    // Session tokens and tracking IDs in query strings
    let trap_params = ["session", "token", "sid", "jsessionid", "phpsessid"];
    if let Some(qs) = url.split('?').nth(1) {
        let qs_lower = qs.to_lowercase();
        for param in &trap_params {
            if qs_lower.contains(param) {
                return true;
            }
        }
    }

    // Very deep paths (>6 segments) are often auto-generated
    let path_depth = url.split('/').count().saturating_sub(3);
    if path_depth > 6 {
        return true;
    }

    // Search result pages of other sites
    if lower.contains("/search?") || lower.contains("?q=") || lower.contains("?query=") {
        return true;
    }

    false
}
