mod cache;
mod crawler;
mod interest;
mod search;
mod tracker_strip;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::{debug, info};

struct AppState {
    index:     search::SearchIndex,
    cache:     cache::PageCache,
    crawler:   crawler::Crawler,
    interests: interest::InterestGraph,
}

type SharedState = Arc<AppState>;

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    #[serde(default = "default_page")]
    page: usize,
}

fn default_page() -> usize { 1 }

#[derive(Deserialize)]
struct FetchParams {
    url: String,
}

#[derive(Serialize)]
struct SearchResponse {
    query: String,
    page: usize,
    results: Vec<search::SearchResult>,
    total_indexed: usize,
    total_cached: usize,
    total_known: usize,
    elapsed_ms: u128,
    interests: Vec<interest::InterestEntry>,
}

#[derive(Serialize)]
struct StatusResponse {
    indexed: usize,
    cached: usize,
    known: usize,
    cache_bytes: u64,
    interest_terms: usize,
    interest_domains: usize,
}

#[derive(Serialize)]
struct InterestsResponse {
    terms: Vec<interest::InterestEntry>,
    domains: Vec<interest::InterestEntry>,
}

async fn root() -> impl IntoResponse {
    match tokio::fs::read_to_string("static/index.html").await {
        Ok(html) => Html(html).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "static/index.html not found").into_response(),
    }
}

async fn api_search(
    State(state): State<SharedState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let start = std::time::Instant::now();
    state.interests.record_query(&params.q);

    const PER_PAGE: usize = 25;
    let offset = (params.page.saturating_sub(1)) * PER_PAGE;

    let all = state.index.search(&params.q, offset + PER_PAGE);
    let mut results: Vec<search::SearchResult> = all.into_iter().skip(offset).collect();

    if results.is_empty() {
        let seeds = build_seed_urls(&params.q);
        let entries = state.crawler.discover_batch(&seeds).await;
        for entry in entries { state.index.upsert(entry); }
        results = state.index
            .search(&params.q, offset + PER_PAGE)
            .into_iter().skip(offset).collect();
    }

    let stale_urls: Vec<String> = results
        .iter()
        .filter(|r| r.age_secs > 7 * 86400 || r.tier == search::Tier::Known)
        .map(|r| r.url.clone())
        .collect();

    if !stale_urls.is_empty() {
        let sc = Arc::clone(&state);
        tokio::spawn(async move {
            let entries = sc.crawler.discover_batch(&stale_urls).await;
            for entry in entries { sc.index.upsert(entry); }
        });
    }

    Json(SearchResponse {
        query:         params.q,
        page:          params.page,
        results,
        total_indexed: state.index.entry_count(),
        total_cached:  state.index.cached_count(),
        total_known:   state.index.known_count(),
        elapsed_ms:    start.elapsed().as_millis(),
        interests:     state.interests.top_terms(8),
    })
}

async fn api_fetch(
    State(state): State<SharedState>,
    Query(params): Query<FetchParams>,
) -> impl IntoResponse {
    let url = &params.url;
    let title_hint = state.index.get_title(url).unwrap_or_default();
    state.interests.record_visit(url, &title_hint);

    if state.cache.contains(url) {
        if let Some(html) = state.cache.get_html(url) {
            return (
                StatusCode::OK,
                [("X-Meridian-Cache", "hit"), ("Content-Type", "text/html")],
                html,
            ).into_response();
        }
    }

    match state.crawler.fetch_and_strip(url).await {
        Some((clean_html, title, _)) => {
            if let Err(e) = state.cache.store(url, &title, &clean_html) {
                tracing::warn!("cache write failed for {}: {}", url, e);
            } else {
                state.index.mark_cached(url);
            }
            (
                StatusCode::OK,
                [("X-Meridian-Cache", "miss"), ("Content-Type", "text/html")],
                clean_html,
            ).into_response()
        }
        None => (
            StatusCode::BAD_GATEWAY,
            [("X-Meridian-Cache", "error"), ("Content-Type", "text/plain")],
            format!("failed to fetch {}", url),
        ).into_response(),
    }
}

#[derive(Deserialize)]
struct ExtensionReport {
    url: String,
    title: String,
    snippet: String,
    links: Vec<String>,
}

async fn api_index(
    State(state): State<SharedState>,
    Json(report): Json<ExtensionReport>,
) -> impl IntoResponse {
    state.interests.record_visit(&report.url, &report.title);

    let domain = report.url.parse::<url::Url>().ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();

    state.index.upsert(search::IndexEntry {
        url:       report.url.clone(),
        domain,
        title:     report.title,
        snippet:   report.snippet,
        tier:      search::Tier::Cached,
        last_seen: chrono::Utc::now(),
        score:     1.5,
        tags:      vec![],
    });

    for link in &report.links {
        let link_domain = link.parse::<url::Url>().ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        state.index.add_known(link, &link_domain, "");
    }

    (StatusCode::ACCEPTED, "indexed")
}

async fn api_interests(State(state): State<SharedState>) -> impl IntoResponse {
    Json(InterestsResponse {
        terms:   state.interests.top_terms(10),
        domains: state.interests.top_domains(8),
    })
}

async fn api_status(State(state): State<SharedState>) -> impl IntoResponse {
    let (cache_count, cache_bytes) = state.cache.stats();
    Json(StatusResponse {
        indexed:          state.index.entry_count(),
        cached:           cache_count,
        known:            state.index.known_count(),
        cache_bytes,
        interest_terms:   state.interests.term_count(),
        interest_domains: state.interests.domain_count(),
    })
}

fn build_seed_urls(query: &str) -> Vec<String> {
    let encoded = url::form_urlencoded::byte_serialize(query.as_bytes())
        .collect::<String>();
    vec![
        format!("https://html.duckduckgo.com/html/?q={}", encoded),
        format!("https://en.wikipedia.org/w/index.php?search={}", encoded),
        format!("https://hn.algolia.com/api/v1/search?query={}&tags=story&hitsPerPage=10", encoded),
    ]
}

/// Background crawl loop. Wakes every `interval`, scores all Known/stale
/// entries against the interest graph, and crawls the top `batch_size`.
async fn background_crawler(state: SharedState, interval: Duration, batch_size: usize) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        if state.interests.term_count() == 0 {
            debug!("background crawler: no interest signal yet, skipping");
            continue;
        }

        let candidates = state.index.candidates_for_crawl(batch_size * 4);
        if candidates.is_empty() {
            debug!("background crawler: no candidates");
            continue;
        }

        let mut scored: Vec<(f32, String)> = candidates
            .into_iter()
            .map(|(url, title, snippet): (String, String, String)| {
                let score = state.interests.score(&url, &title, &snippet);
                (score, url)
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(batch_size);

        let urls: Vec<String> = scored.into_iter().map(|(_, u)| u).collect();
        if urls.is_empty() { continue; }

        info!("background crawler: crawling {} urls", urls.len());
        let entries = state.crawler.discover_batch(&urls).await;
        let n = entries.len();
        for entry in entries { state.index.upsert(entry); }
        debug!("background crawler: indexed {} pages", n);
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("meridian=debug".parse().unwrap()),
        )
        .init();

    let state: SharedState = Arc::new(AppState {
        index:     search::SearchIndex::new(),
        cache:     cache::PageCache::new(),
        crawler:   crawler::Crawler::new(),
        interests: interest::InterestGraph::new(),
    });

    {
        let bg = Arc::clone(&state);
        tokio::spawn(background_crawler(bg, Duration::from_secs(60), 10));
        info!("background crawler started (60s interval, 10 urls/cycle)");
    }

    let app = Router::new()
        .route("/",              get(root))
        .route("/api/search",    get(api_search))
        .route("/api/fetch",     post(api_fetch))
        .route("/api/index",     post(api_index))
        .route("/api/interests", get(api_interests))
        .route("/api/status",    get(api_status))
        .nest_service("/static", ServeDir::new("static"))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = "0.0.0.0:3000";
    info!("Meridian listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
