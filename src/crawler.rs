// Demand-driven crawler.
//
// Two public entry points:
//   discover(query, n)    — lightweight HEAD/partial-GET to find n candidates
//                           for a search query. Called on every search.
//   fetch_page(url)       — full GET + tracker strip + cache. Called when a
//                           user clicks a result.
//
// Per-domain politeness is enforced with a DashMap tracking last-request
// timestamps. robots.txt is fetched once per domain and cached in memory.

use dashmap::DashMap;
use reqwest::Client;
use scraper::{Html, Selector};
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};
use url::Url;

use crate::search::{IndexEntry, Tier};
use crate::tracker_strip;

const MERIDIAN_UA: &str =
    "MeridianBot/1.0 (private demand-driven index; +https://generative-systems.net/meridianbot)";

// Default wait between requests to the same domain (seconds)
const DEFAULT_CRAWL_DELAY_SECS: u64 = 10;

// Maximum time to wait for a response before giving up
const FETCH_TIMEOUT_SECS: u64 = 10;

// Maximum response body size to process (bytes). Protects against
// infinite-stream traps and enormous pages.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024; // 2 MB

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredPage {
    pub url: String,
    pub domain: String,
    pub title: String,
    pub snippet: String,
    pub outbound_links: Vec<String>,
}

struct RobotRules {
    disallowed: Vec<String>,
    crawl_delay: u64,
    fetched_at: Instant,
}

pub struct Crawler {
    client: Client,
    // domain -> last request timestamp
    last_request: Arc<DashMap<String, Instant>>,
    // domain -> robots.txt rules
    robots_cache: Arc<DashMap<String, RobotRules>>,
}

impl Crawler {
    pub fn new() -> Self {
        let client = Client::builder()
            .user_agent(MERIDIAN_UA)
            .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
            .gzip(true)
            .deflate(true)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            last_request: Arc::new(DashMap::new()),
            robots_cache: Arc::new(DashMap::new()),
        }
    }

    fn extract_domain(url: &str) -> Option<String> {
        Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
    }

    // Returns the crawl delay in seconds for this domain (from robots.txt
    // or the default). Also returns false if the URL is disallowed.
    async fn check_robots(&self, url: &str) -> (bool, u64) {
        let Some(domain) = Self::extract_domain(url) else {
            return (true, DEFAULT_CRAWL_DELAY_SECS);
        };

        // Refresh robots.txt if we've never fetched it or it's >1 hour old
        let needs_refresh = self.robots_cache
            .get(&domain)
            .map(|r| r.fetched_at.elapsed() > Duration::from_secs(3600))
            .unwrap_or(true);

        if needs_refresh {
            self.fetch_robots(&domain).await;
        }

        let Some(rules) = self.robots_cache.get(&domain) else {
            return (true, DEFAULT_CRAWL_DELAY_SECS);
        };

        let path = Url::parse(url)
            .map(|u| u.path().to_string())
            .unwrap_or_default();

        let allowed = !rules
            .disallowed
            .iter()
            .any(|prefix| path.starts_with(prefix.as_str()));

        (allowed, rules.crawl_delay)
    }

    async fn fetch_robots(&self, domain: &str) {
        let robots_url = format!("https://{}/robots.txt", domain);
        let Ok(resp) = self.client.get(&robots_url).send().await else {
            // Can't reach robots.txt — treat as permissive
            self.robots_cache.insert(domain.to_string(), RobotRules {
                disallowed: vec![],
                crawl_delay: DEFAULT_CRAWL_DELAY_SECS,
                fetched_at: Instant::now(),
            });
            return;
        };

        let Ok(text) = resp.text().await else { return; };

        let mut disallowed = Vec::new();
        let mut crawl_delay = DEFAULT_CRAWL_DELAY_SECS;
        let mut in_relevant_block = false;

        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if line.to_lowercase().starts_with("user-agent:") {
                let agent = line["user-agent:".len()..].trim().to_lowercase();
                in_relevant_block = agent == "*" || agent == "meridianbot";
            } else if in_relevant_block {
                if line.to_lowercase().starts_with("disallow:") {
                    let path = line["disallow:".len()..].trim();
                    if !path.is_empty() {
                        disallowed.push(path.to_string());
                    }
                } else if line.to_lowercase().starts_with("crawl-delay:") {
                    if let Ok(d) = line["crawl-delay:".len()..].trim().parse::<u64>() {
                        crawl_delay = d;
                    }
                }
            }
        }

        self.robots_cache.insert(domain.to_string(), RobotRules {
            disallowed,
            crawl_delay,
            fetched_at: Instant::now(),
        });
    }

    /// Enforce per-domain politeness. Sleeps if we've hit this domain
    /// too recently.
    async fn respect_rate_limit(&self, domain: &str, delay_secs: u64) {
        if let Some(last) = self.last_request.get(domain) {
            let elapsed = last.elapsed();
            let required = Duration::from_secs(delay_secs);
            if elapsed < required {
                tokio::time::sleep(required - elapsed).await;
            }
        }
        self.last_request.insert(domain.to_string(), Instant::now());
    }

    /// Lightweight discovery fetch. Retrieves just enough of a page to
    /// populate an index entry: title, meta description, first 200 chars
    /// of body text, and outbound links.
    pub async fn discover_url(&self, url: &str) -> Option<DiscoveredPage> {
        let domain = Self::extract_domain(url)?;
        let (allowed, delay) = self.check_robots(url).await;

        if !allowed {
            debug!("robots.txt disallows {}", url);
            return None;
        }

        self.respect_rate_limit(&domain, delay).await;

        let resp = self.client.get(url).send().await
            .map_err(|e| warn!("fetch failed {}: {}", url, e)).ok()?;

        let status = resp.status();
        if !status.is_success() {
            debug!("non-200 {} for {}", status, url);
            return None;
        }

        // Read up to MAX_BODY_BYTES — don't download the whole world
        let bytes = resp.bytes().await.ok()?;
        let body = std::str::from_utf8(
            &bytes[..bytes.len().min(MAX_BODY_BYTES)]
        ).unwrap_or("");

        let (title, snippet, links) = extract_metadata(body, url);

        Some(DiscoveredPage {
            url: url.to_string(),
            domain,
            title,
            snippet,
            outbound_links: links,
        })
    }

    /// Full fetch + tracker strip. Returns clean HTML ready to cache.
    pub async fn fetch_and_strip(&self, url: &str) -> Option<(String, String, String)> {
        let domain = Self::extract_domain(url)?;
        let (allowed, delay) = self.check_robots(url).await;

        if !allowed {
            return None;
        }

        self.respect_rate_limit(&domain, delay).await;

        let resp = self.client.get(url).send().await
            .map_err(|e| warn!("full fetch failed {}: {}", url, e)).ok()?;

        if !resp.status().is_success() {
            return None;
        }

        let bytes = resp.bytes().await.ok()?;
        let raw_html = std::str::from_utf8(
            &bytes[..bytes.len().min(MAX_BODY_BYTES)]
        ).unwrap_or("");

        let clean_html = tracker_strip::strip(raw_html, url);
        let (title, snippet, _) = extract_metadata(&clean_html, url);

        Some((clean_html, title, snippet))
    }

    /// Build index entries from a discovery crawl of `urls`.
    /// Returns Vec<IndexEntry> — caller adds these to the SearchIndex.
    pub async fn discover_batch(&self, urls: &[String]) -> Vec<IndexEntry> {
        let mut results = Vec::new();
        for url in urls {
            if let Some(page) = self.discover_url(url).await {
                results.push(IndexEntry {
                    url: page.url,
                    domain: page.domain,
                    title: page.title,
                    snippet: page.snippet,
                    tier: Tier::Indexed,
                    last_seen: chrono::Utc::now(),
                    score: 1.0,
                    tags: vec![],
                });
            }
        }
        results
    }
}

/// Extracts (title, snippet, outbound_links) from raw HTML.
fn extract_metadata(html: &str, base_url: &str) -> (String, String, Vec<String>) {
    let document = Html::parse_document(html);

    let title_sel = Selector::parse("title").unwrap();
    let meta_sel = Selector::parse("meta[name='description']").unwrap();
    let h1_sel = Selector::parse("h1").unwrap();
    let p_sel = Selector::parse("p").unwrap();
    let a_sel = Selector::parse("a[href]").unwrap();

    let title = document
        .select(&title_sel)
        .next()
        .map(|e| e.text().collect::<String>().trim().to_string())
        .or_else(|| {
            document
                .select(&h1_sel)
                .next()
                .map(|e| e.text().collect::<String>().trim().to_string())
        })
        .unwrap_or_default();

    let snippet = document
        .select(&meta_sel)
        .next()
        .and_then(|e| e.value().attr("content").map(|s| s.to_string()))
        .or_else(|| {
            document
                .select(&p_sel)
                .find(|p| {
                    let t = p.text().collect::<String>();
                    t.split_whitespace().count() > 15
                })
                .map(|p| {
                    let text = p.text().collect::<String>();
                    text.chars().take(200).collect::<String>()
                })
        })
        .unwrap_or_default();

    let base = Url::parse(base_url).ok();
    let links: Vec<String> = document
        .select(&a_sel)
        .filter_map(|a| {
            let href = a.value().attr("href")?;
            if href.starts_with('#') || href.starts_with("javascript:") {
                return None;
            }
            // Resolve relative URLs
            let resolved = if href.starts_with("http") {
                href.to_string()
            } else if let Some(ref b) = base {
                b.join(href).ok()?.to_string()
            } else {
                return None;
            };
            // Only index http/https
            if !resolved.starts_with("http") { return None; }
            Some(tracker_strip::clean_url(&resolved))
        })
        .take(100) // cap outbound links per page
        .collect();

    (title, snippet, links)
}
