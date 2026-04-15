// In-memory search index. Stores index entries keyed by URL.
//
// This is intentionally simple — a production upgrade path would swap
// `DashMap` for Tantivy (BM25, full-text) or point to a Manticore Search
// instance over HTTP. The `SearchIndex` interface stays the same either way.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Clean cached copy lives on disk. Served instantly.
    Cached,
    /// Crawled and indexed. No cached copy yet.
    Indexed,
    /// Seen as a link target but never fetched.
    Known,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub url: String,
    pub domain: String,
    pub title: String,
    pub snippet: String,
    pub tier: Tier,
    pub last_seen: DateTime<Utc>,
    /// Rough importance score: inbound link count weighted by source tier.
    pub score: f32,
    /// Topics extracted from page content / anchor text.
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub url: String,
    pub domain: String,
    pub title: String,
    pub snippet: String,
    pub tier: Tier,
    pub score: f32,
    pub tags: Vec<String>,
    pub age_secs: i64,
}

pub struct SearchIndex {
    entries: Arc<DashMap<String, IndexEntry>>,
}

impl SearchIndex {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
        }
    }

    /// Insert or update an entry. If one already exists with Tier::Cached,
    /// preserve that tier — a cached entry doesn't downgrade to Indexed.
    pub fn upsert(&self, mut entry: IndexEntry) {
        if let Some(existing) = self.entries.get(&entry.url) {
            if existing.tier == Tier::Cached && entry.tier != Tier::Cached {
                entry.tier = Tier::Cached;
            }
            // Accumulate inbound link score
            entry.score = entry.score.max(existing.score);
        }
        self.entries.insert(entry.url.clone(), entry);
    }

    /// Add a URL we've only seen linked-to but not fetched.
    pub fn add_known(&self, url: &str, domain: &str, anchor_text: &str) {
        // Don't downgrade an existing indexed/cached entry.
        if self.entries.contains_key(url) {
            // Bump score — another inbound link.
            if let Some(mut e) = self.entries.get_mut(url) {
                e.score += 0.1;
            }
            return;
        }
        self.entries.insert(url.to_string(), IndexEntry {
            url: url.to_string(),
            domain: domain.to_string(),
            title: anchor_text.to_string(),
            snippet: String::new(),
            tier: Tier::Known,
            last_seen: Utc::now(),
            score: 0.1,
            tags: vec![],
        });
    }

    pub fn mark_cached(&self, url: &str) {
        if let Some(mut e) = self.entries.get_mut(url) {
            e.tier = Tier::Cached;
        }
    }

    /// Simple keyword search. Tokenises query, scores each entry by
    /// how many tokens appear in title + snippet, weighted by tier and
    /// base score. Returns up to `limit` results sorted by relevance.
    ///
    /// Replace this body with a Tantivy query for proper BM25.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        let tokens: Vec<String> = query
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect();

        if tokens.is_empty() {
            return vec![];
        }

        let mut scored: Vec<(f32, IndexEntry)> = self
            .entries
            .iter()
            .filter_map(|e| {
                let entry = e.value();
                if entry.tier == Tier::Known && entry.snippet.is_empty() {
                    return None; // Known-only entries with no data aren't useful results
                }
                let haystack = format!(
                    "{} {} {}",
                    entry.title.to_lowercase(),
                    entry.snippet.to_lowercase(),
                    entry.domain.to_lowercase()
                );
                let tf: f32 = tokens
                    .iter()
                    .map(|t| {
                        let count = haystack.matches(t.as_str()).count() as f32;
                        if count > 0.0 { 1.0 + count.ln() } else { 0.0 }
                    })
                    .sum();

                if tf == 0.0 {
                    return None;
                }

                // Tier boost: cached > indexed > known
                let tier_boost = match entry.tier {
                    Tier::Cached  => 2.0,
                    Tier::Indexed => 1.0,
                    Tier::Known   => 0.5,
                };

                let final_score = tf * tier_boost + entry.score;
                Some((final_score, entry.clone()))
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        let now = Utc::now();
        scored
            .into_iter()
            .map(|(score, e)| SearchResult {
                domain: e.domain.clone(),
                title: e.title.clone(),
                snippet: e.snippet.clone(),
                tier: e.tier.clone(),
                tags: e.tags.clone(),
                age_secs: (now - e.last_seen).num_seconds(),
                url: e.url,
                score,
            })
            .collect()
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn cached_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.tier == Tier::Cached)
            .count()
    }

    pub fn known_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.tier == Tier::Known)
            .count()
    }
}

    /// Returns the stored title for a URL, if it exists in the index.
    pub fn get_title(&self, url: &str) -> Option<String> {
        self.entries.get(url).map(|e| e.title.clone())
    }

    /// Returns up to `limit` Known or stale Indexed entries as
    /// (url, title, snippet) tuples for the background crawler to score
    /// and potentially promote.
    pub fn candidates_for_crawl(&self, limit: usize) -> Vec<(String, String, String)> {
        let now = chrono::Utc::now();
        let mut candidates: Vec<(String, String, String)> = self
            .entries
            .iter()
            .filter(|e| {
                match e.tier {
                    Tier::Known => true,
                    Tier::Indexed => {
                        // Stale if not seen in 3 days
                        (now - e.last_seen).num_seconds() > 3 * 86400
                    }
                    Tier::Cached => false, // already have a clean copy
                }
            })
            .map(|e| (e.url.clone(), e.title.clone(), e.snippet.clone()))
            .take(limit)
            .collect();

        // Shuffle so we don't always process the same URLs
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        candidates.sort_by_key(|(url, _, _)| {
            let mut h = DefaultHasher::new();
            url.hash(&mut h);
            seed.hash(&mut h);
            h.finish()
        });

        candidates
    }
