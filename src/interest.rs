// Server-side interest graph.
//
// Tracks two signals:
//   - query terms: words the user has searched for, weighted by frequency
//   - domains: sites the user has actually visited, weighted by visit count
//
// The background crawler uses this to decide what to crawl next:
// Known/stale URLs whose domain or title text scores highly against the
// interest graph get promoted to the front of the crawl queue.

use dashmap::DashMap;
use serde::Serialize;
use std::sync::Arc;

/// Stopwords we don't want polluting the interest graph with noise.
const STOPWORDS: &[&str] = &[
    "the","and","for","are","but","not","you","all","can","had","her","was",
    "one","our","out","day","get","has","him","his","how","its","let","may",
    "new","now","old","see","two","way","who","boy","did","does","don","got",
    "put","say","she","too","use","from","have","that","this","with","will",
    "been","into","just","like","more","over","such","than","them","then",
    "they","want","were","what","when","your",
];

fn is_stopword(w: &str) -> bool {
    STOPWORDS.contains(&w)
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 3)
        .map(|t| t.to_lowercase())
        .filter(|t| !is_stopword(t))
        .collect()
}

pub fn extract_domain(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
}

#[derive(Serialize, Clone)]
pub struct InterestEntry {
    pub term: String,
    pub score: f32,
}

pub struct InterestGraph {
    /// Raw term → hit count
    terms: Arc<DashMap<String, f32>>,
    /// Domain → hit count
    domains: Arc<DashMap<String, f32>>,
}

impl InterestGraph {
    pub fn new() -> Self {
        Self {
            terms: Arc::new(DashMap::new()),
            domains: Arc::new(DashMap::new()),
        }
    }

    /// Called every time the user runs a search.
    pub fn record_query(&self, query: &str) {
        for token in tokenize(query) {
            *self.terms.entry(token).or_insert(0.0) += 1.0;
        }
    }

    /// Called when the user visits / clicks a result.
    pub fn record_visit(&self, url: &str, title: &str) {
        if let Some(domain) = extract_domain(url) {
            *self.domains.entry(domain).or_insert(0.0) += 1.0;
        }
        // Title words also feed the term graph, but at lower weight
        for token in tokenize(title) {
            *self.terms.entry(token).or_insert(0.0) += 0.3;
        }
    }

    /// Score a candidate URL+title+snippet against the current interest graph.
    /// Returns a value in [0, ∞) — higher = more relevant to crawl next.
    pub fn score(&self, url: &str, title: &str, snippet: &str) -> f32 {
        let mut score = 0.0f32;

        // Domain match — strongest signal
        if let Some(domain) = extract_domain(url) {
            if let Some(d) = self.domains.get(&domain) {
                score += *d * 3.0;
            }
        }

        // Term matches in title (high weight) and snippet (lower weight)
        for token in tokenize(title) {
            if let Some(t) = self.terms.get(&token) {
                score += *t * 2.0;
            }
        }
        for token in tokenize(snippet) {
            if let Some(t) = self.terms.get(&token) {
                score += *t * 0.5;
            }
        }

        score
    }

    /// Top N terms by score, for display in the sidebar.
    pub fn top_terms(&self, n: usize) -> Vec<InterestEntry> {
        let mut entries: Vec<InterestEntry> = self
            .terms
            .iter()
            .map(|e| InterestEntry { term: e.key().clone(), score: *e.value() })
            .collect();
        entries.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        entries.truncate(n);
        entries
    }

    /// Top N domains by visit count.
    pub fn top_domains(&self, n: usize) -> Vec<InterestEntry> {
        let mut entries: Vec<InterestEntry> = self
            .domains
            .iter()
            .map(|e| InterestEntry { term: e.key().clone(), score: *e.value() })
            .collect();
        entries.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        entries.truncate(n);
        entries
    }

    pub fn term_count(&self) -> usize { self.terms.len() }
    pub fn domain_count(&self) -> usize { self.domains.len() }
}

// ── Persistence ────────────────────────────────────────────────────────────

use std::collections::HashMap;

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedInterests {
    terms:   HashMap<String, f32>,
    domains: HashMap<String, f32>,
}

impl InterestGraph {
    pub fn save(&self, path: &str) -> std::io::Result<()> {
        let data = SavedInterests {
            terms:   self.terms.iter().map(|e| (e.key().clone(), *e.value())).collect(),
            domains: self.domains.iter().map(|e| (e.key().clone(), *e.value())).collect(),
        };
        let tmp = format!("{}.tmp", path);
        std::fs::write(&tmp, serde_json::to_string(&data)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load(path: &str) -> Self {
        let graph = Self::new();
        let Ok(raw) = std::fs::read_to_string(path) else { return graph; };
        let Ok(data) = serde_json::from_str::<SavedInterests>(&raw) else { return graph; };
        for (k, v) in data.terms   { graph.terms.insert(k, v); }
        for (k, v) in data.domains { graph.domains.insert(k, v); }
        graph
    }
}
