use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Cached,
    Indexed,
    Known,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub url:       String,
    pub domain:    String,
    pub title:     String,
    pub snippet:   String,
    pub tier:      Tier,
    pub last_seen: DateTime<Utc>,
    pub score:     f32,
    pub tags:      Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub url:      String,
    pub domain:   String,
    pub title:    String,
    pub snippet:  String,
    pub tier:     Tier,
    pub score:    f32,
    pub tags:     Vec<String>,
    pub age_secs: i64,
}

pub struct SearchIndex {
    entries: Arc<DashMap<String, IndexEntry>>,
}

impl SearchIndex {
    pub fn new() -> Self {
        Self { entries: Arc::new(DashMap::new()) }
    }

    pub fn upsert(&self, mut entry: IndexEntry) {
        if let Some(existing) = self.entries.get(&entry.url) {
            if existing.tier == Tier::Cached && entry.tier != Tier::Cached {
                entry.tier = Tier::Cached;
            }
            entry.score = entry.score.max(existing.score);
        }
        self.entries.insert(entry.url.clone(), entry);
    }

    #[allow(dead_code)]
    pub fn add_known(&self, url: &str, domain: &str, anchor_text: &str) {
        if self.entries.contains_key(url) {
            if let Some(mut e) = self.entries.get_mut(url) {
                e.score += 0.1;
            }
            return;
        }
        self.entries.insert(url.to_string(), IndexEntry {
            url:       url.to_string(),
            domain:    domain.to_string(),
            title:     anchor_text.to_string(),
            snippet:   String::new(),
            tier:      Tier::Known,
            last_seen: Utc::now(),
            score:     0.1,
            tags:      vec![],
        });
    }

    pub fn mark_cached(&self, url: &str) {
        if let Some(mut e) = self.entries.get_mut(url) {
            e.tier = Tier::Cached;
        }
    }

    pub fn get_title(&self, url: &str) -> Option<String> {
        self.entries.get(url).map(|e| e.title.clone())
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        let tokens: Vec<String> = query
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect();

        if tokens.is_empty() { return vec![]; }

        let mut scored: Vec<(f32, IndexEntry)> = self
            .entries
            .iter()
            .filter_map(|e| {
                let entry = e.value();
                if entry.tier == Tier::Known && entry.snippet.is_empty() {
                    return None;
                }
                let haystack = format!(
                    "{} {} {}",
                    entry.title.to_lowercase(),
                    entry.snippet.to_lowercase(),
                    entry.domain.to_lowercase()
                );
                let tf: f32 = tokens.iter().map(|t| {
                    let count = haystack.matches(t.as_str()).count() as f32;
                    if count > 0.0 { 1.0 + count.ln() } else { 0.0 }
                }).sum();

                if tf == 0.0 { return None; }

                let tier_boost = match entry.tier {
                    Tier::Cached  => 2.0,
                    Tier::Indexed => 1.0,
                    Tier::Known   => 0.5,
                };
                Some((tf * tier_boost + entry.score, entry.clone()))
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        let now = Utc::now();
        scored.into_iter().map(|(score, e)| SearchResult {
            domain:   e.domain.clone(),
            title:    e.title.clone(),
            snippet:  e.snippet.clone(),
            tier:     e.tier.clone(),
            tags:     e.tags.clone(),
            age_secs: (now - e.last_seen).num_seconds(),
            url:      e.url,
            score,
        }).collect()
    }

    /// Returns Known entries and stale Indexed entries for background crawling.
    /// Shuffled so successive cycles don't always hit the same URLs.
    pub fn candidates_for_crawl(&self, limit: usize) -> Vec<(String, String, String)> {
        let now = Utc::now();
        let stale_threshold = chrono::Duration::days(3);

        let mut candidates: Vec<(String, String, String)> = self
            .entries
            .iter()
            .filter(|e| match e.tier {
                Tier::Known   => true,
                Tier::Indexed => (now - e.last_seen) > stale_threshold,
                Tier::Cached  => false,
            })
            .map(|e| (e.url.clone(), e.title.clone(), e.snippet.clone()))
            .take(limit)
            .collect();

        // Pseudo-shuffle using time-seeded hash so each cycle picks different URLs
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        candidates.sort_by_key(|(url, _, _): &(String, String, String)| {
            let mut h = DefaultHasher::new();
            url.hash(&mut h);
            seed.hash(&mut h);
            h.finish()
        });

        candidates
    }

    pub fn entry_count(&self) -> usize { self.entries.len() }

    pub fn cached_count(&self) -> usize {
        self.entries.iter().filter(|e| e.tier == Tier::Cached).count()
    }

    pub fn known_count(&self) -> usize {
        self.entries.iter().filter(|e| e.tier == Tier::Known).count()
    }
}

// ── Persistence ────────────────────────────────────────────────────────────

impl SearchIndex {
    pub fn save(&self, path: &str) -> std::io::Result<()> {
        let entries: Vec<IndexEntry> = self.entries
            .iter()
            .map(|e| e.value().clone())
            .collect();
        let tmp = format!("{}.tmp", path);
        std::fs::write(&tmp, serde_json::to_string(&entries)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load(path: &str) -> Self {
        let index = Self::new();
        let Ok(raw) = std::fs::read_to_string(path) else { return index; };
        let Ok(entries) = serde_json::from_str::<Vec<IndexEntry>>(&raw) else { return index; };
        let count = entries.len();
        for entry in entries { index.entries.insert(entry.url.clone(), entry); }
        tracing::info!("loaded {} index entries from {}", count, path);
        index
    }
}
