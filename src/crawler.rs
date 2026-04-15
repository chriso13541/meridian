// Web crawler.
//
// `crawl_one(url)` — fetches a single URL, respects robots.txt and per-domain
// rate limiting, returns the page metadata and all outbound links found.
//
// `fetch_and_strip(url)` — full fetch + tracker strip for on-demand caching.
//
// Rate limiting: one global semaphore caps total concurrent requests.
// Per-domain timestamps enforce minimum spacing between hits to the same host.

use dashmap::DashMap;
use reqwest::Client;
use scraper::{Html, Selector};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::{debug, warn};
use url::Url;

use crate::search::{IndexEntry, Tier};
use crate::tracker_strip;

const MERIDIAN_UA: &str =
    "MeridianBot/1.0 (private demand-driven index; +https://generative-systems.net/meridianbot)";

// Maximum simultaneous outgoing HTTP requests across all domains
const MAX_CONCURRENT: usize = 8;

// Default wait between requests to the same domain if robots.txt
// doesn't specify one
const DEFAULT_CRAWL_DELAY_SECS: u64 = 5;

const FETCH_TIMEOUT_SECS: u64 = 12;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub struct CrawlResult {
    pub entry: IndexEntry,
    pub outbound_links: Vec<String>,
}

struct RobotRules {
    disallowed: Vec<String>,
    crawl_delay: u64,
    fetched_at: Instant,
}

pub struct Crawler {
    client: Client,
    last_request: Arc<DashMap<String, Instant>>,
    robots_cache: Arc<DashMap<String, RobotRules>>,
    semaphore: Arc<Semaphore>,
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
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT)),
        }
    }

    pub fn extract_domain(url: &str) -> Option<String> {
        Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
    }

    async fn check_robots(&self, url: &str) -> (bool, u64) {
        let Some(domain) = Self::extract_domain(url) else {
            return (true, DEFAULT_CRAWL_DELAY_SECS);
        };

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

        let allowed = !rules.disallowed.iter()
            .any(|prefix| path.starts_with(prefix.as_str()));

        (allowed, rules.crawl_delay)
    }

    async fn fetch_robots(&self, domain: &str) {
        let robots_url = format!("https://{}/robots.txt", domain);
        let Ok(resp) = self.client.get(&robots_url).send().await else {
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
            if line.starts_with('#') || line.is_empty() { continue; }

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

    async fn wait_for_domain(&self, domain: &str, delay_secs: u64) {
        if let Some(last) = self.last_request.get(domain) {
            let elapsed = last.elapsed();
            let required = Duration::from_secs(delay_secs);
            if elapsed < required {
                tokio::time::sleep(required - elapsed).await;
            }
        }
        self.last_request.insert(domain.to_string(), Instant::now());
    }

    /// Crawl a single URL. Returns index entry + outbound links on success.
    pub async fn crawl_one(&self, url: &str) -> Option<CrawlResult> {
        let domain = Self::extract_domain(url)?;
        let (allowed, delay) = self.check_robots(url).await;

        if !allowed {
            debug!("robots.txt disallows {}", url);
            return None;
        }

        self.wait_for_domain(&domain, delay).await;

        let _permit = self.semaphore.acquire().await.ok()?;

        let resp = self.client.get(url).send().await
            .map_err(|e| debug!("fetch error {}: {}", url, e)).ok()?;

        if !resp.status().is_success() {
            debug!("non-200 {} for {}", resp.status(), url);
            return None;
        }

        let bytes = resp.bytes().await.ok()?;
        let body = std::str::from_utf8(
            &bytes[..bytes.len().min(MAX_BODY_BYTES)]
        ).unwrap_or("");

        let (title, snippet, links) = extract_metadata(body, url);

        Some(CrawlResult {
            entry: IndexEntry {
                url:       url.to_string(),
                domain:    domain.clone(),
                title,
                snippet,
                tier:      Tier::Indexed,
                last_seen: chrono::Utc::now(),
                score:     1.0,
                tags:      vec![],
            },
            outbound_links: links,
        })
    }

    /// Full fetch + tracker strip. Called when a user clicks a result.
    pub async fn fetch_and_strip(&self, url: &str) -> Option<(String, String, String)> {
        let domain = Self::extract_domain(url)?;
        let (allowed, delay) = self.check_robots(url).await;
        if !allowed { return None; }

        self.wait_for_domain(&domain, delay).await;

        let _permit = self.semaphore.acquire().await.ok()?;

        let resp = self.client.get(url).send().await
            .map_err(|e| warn!("full fetch failed {}: {}", url, e)).ok()?;

        if !resp.status().is_success() { return None; }

        let bytes = resp.bytes().await.ok()?;
        let raw_html = std::str::from_utf8(
            &bytes[..bytes.len().min(MAX_BODY_BYTES)]
        ).unwrap_or("");

        let clean_html = tracker_strip::strip(raw_html, url);
        let (title, snippet, _) = extract_metadata(&clean_html, url);

        Some((clean_html, title, snippet))
    }
}

fn extract_metadata(html: &str, base_url: &str) -> (String, String, Vec<String>) {
    let document = Html::parse_document(html);

    let title_sel = Selector::parse("title").unwrap();
    let meta_sel  = Selector::parse("meta[name='description']").unwrap();
    let h1_sel    = Selector::parse("h1").unwrap();
    let p_sel     = Selector::parse("p").unwrap();
    let a_sel     = Selector::parse("a[href]").unwrap();

    let title = document.select(&title_sel).next()
        .map(|e| e.text().collect::<String>().trim().to_string())
        .or_else(|| document.select(&h1_sel).next()
            .map(|e| e.text().collect::<String>().trim().to_string()))
        .unwrap_or_default();

    let snippet = document.select(&meta_sel).next()
        .and_then(|e| e.value().attr("content").map(|s| s.to_string()))
        .or_else(|| {
            document.select(&p_sel)
                .find(|p| p.text().collect::<String>().split_whitespace().count() > 15)
                .map(|p| p.text().collect::<String>().chars().take(220).collect())
        })
        .unwrap_or_default();

    let base = Url::parse(base_url).ok();
    let links: Vec<String> = document.select(&a_sel)
        .filter_map(|a| {
            let href = a.value().attr("href")?;
            if href.starts_with('#') || href.starts_with("javascript:") { return None; }
            let resolved = if href.starts_with("http") {
                href.to_string()
            } else if let Some(ref b) = base {
                b.join(href).ok()?.to_string()
            } else {
                return None;
            };
            if !resolved.starts_with("http") { return None; }
            Some(tracker_strip::clean_url(&resolved))
        })
        .take(80)
        .collect();

    (title, snippet, links)
}
