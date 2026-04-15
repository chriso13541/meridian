mod cache;
mod crawler;
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
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::info;

// ---------------------------------------------------------------------------
// Shared application state
// ---------------------------------------------------------------------------

struct AppState {
    index: search::SearchIndex,
    cache: cache::PageCache,
    crawler: crawler::Crawler,
}

type SharedState = Arc<AppState>;

// ---------------------------------------------------------------------------
// Query / response types
// ---------------------------------------------------------------------------

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
}

#[derive(Serialize)]
struct StatusResponse {
    indexed: usize,
    cached: usize,
    known: usize,
    cache_bytes: u64,
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

/// GET / — serve the main HTML frontend
async fn root() -> impl IntoResponse {
    match tokio::fs::read_to_string("static/index.html").await {
        Ok(html) => Html(html).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "static/index.html not found").into_response(),
    }
}

/// GET /api/search?q=<query>&page=<n>
/// Returns JSON search results from the in-memory index.
/// If results are stale (not seen in the last 7 days), a background
/// rediscovery crawl is triggered for those URLs.
async fn api_search(
    State(state): State<SharedState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let start = std::time::Instant::now();

    // Results per page
    const PER_PAGE: usize = 25;
    let offset = (params.page.saturating_sub(1)) * PER_PAGE;

    // Pull a wider set and paginate in Rust (avoids re-querying the index)
    let all = state.index.search(&params.q, offset + PER_PAGE);
    let mut results: Vec<search::SearchResult> = all.into_iter().skip(offset).collect();

    // Cold start: nothing in the index for this query yet.
    // Crawl seed URLs derived from the query synchronously so the user
    // gets real results on their first search rather than an empty page.
    if results.is_empty() {
        let seeds = build_seed_urls(&params.q);
        let entries = state.crawler.discover_batch(&seeds).await;
        for entry in entries {
            state.index.upsert(entry);
        }
        results = state
            .index
            .search(&params.q, offset + PER_PAGE)
            .into_iter()
            .skip(offset)
            .collect();
    }

    // Kick off background discovery for stale/unknown URLs in this result set
    let stale_urls: Vec<String> = results
        .iter()
        .filter(|r| r.age_secs > 7 * 86400 || r.tier == search::Tier::Known)
        .map(|r| r.url.clone())
        .collect();

    if !stale_urls.is_empty() {
        let state_clone = Arc::clone(&state);
        tokio::spawn(async move {
            let entries = state_clone.crawler.discover_batch(&stale_urls).await;
            for entry in entries {
                state_clone.index.upsert(entry);
            }
        });
    }

    let elapsed = start.elapsed().as_millis();

    Json(SearchResponse {
        query: params.q,
        page: params.page,
        results,
        total_indexed: state.index.entry_count(),
        total_cached: state.index.cached_count(),
        total_known: state.index.known_count(),
        elapsed_ms: elapsed,
    })
}

/// POST /api/fetch?url=<url>
/// Triggered when the user clicks a result. Fetches the live page,
/// strips trackers, caches it, and marks it as Tier::Cached in the index.
/// Returns the cleaned HTML so the browser can render it immediately
/// without a second round-trip.
async fn api_fetch(
    State(state): State<SharedState>,
    Query(params): Query<FetchParams>,
) -> impl IntoResponse {
    let url = &params.url;

    // Serve from cache if fresh enough
    if state.cache.contains(url) {
        if let Some(html) = state.cache.get_html(url) {
            let meta = state.cache.get_meta(url);
            let _title = meta.map(|m| m.title).unwrap_or_default();
            return (
                StatusCode::OK,
                [("X-Meridian-Cache", "hit"), ("Content-Type", "text/html")],
                html,
            ).into_response();
        }
    }

    // Live fetch + strip
    match state.crawler.fetch_and_strip(url).await {
        Some((clean_html, title, _snippet)) => {
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

/// POST /api/index  — receive a page report from the browser extension.
/// Body: { url, title, snippet, links: [string] }
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
    let domain = report.url
        .parse::<url::Url>()
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();

    // The visited page itself becomes a Tier::Cached entry
    // (user has literally viewed it — it's in their browser cache)
    state.index.upsert(search::IndexEntry {
        url: report.url.clone(),
        domain,
        title: report.title,
        snippet: report.snippet,
        tier: search::Tier::Cached,
        last_seen: chrono::Utc::now(),
        score: 1.5,
        tags: vec![],
    });

    // All outbound links become Tier::Known seeds
    for link in &report.links {
        let link_domain = link
            .parse::<url::Url>()
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        state.index.add_known(link, &link_domain, "");
    }

    (StatusCode::ACCEPTED, "indexed")
}

/// GET /api/status
async fn api_status(State(state): State<SharedState>) -> impl IntoResponse {
    let (cache_count, cache_bytes) = state.cache.stats();
    Json(StatusResponse {
        indexed: state.index.entry_count(),
        cached: cache_count,
        known: state.index.known_count(),
        cache_bytes,
    })
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

/// Build a list of seed URLs for a cold-start crawl.
/// Uses DuckDuckGo HTML (no JS required, no API key) and a handful of
/// well-known directories as fallback seeds.
fn build_seed_urls(query: &str) -> Vec<String> {
    let encoded = url::form_urlencoded::byte_serialize(query.as_bytes())
        .collect::<String>();

    vec![
        // DuckDuckGo HTML search — returns real links without JS
        format!("https://html.duckduckgo.com/html/?q={}", encoded),
        // Wikipedia search
        format!("https://en.wikipedia.org/w/index.php?search={}", encoded),
        // HackerNews Algolia search API — great for tech queries
        format!("https://hn.algolia.com/api/v1/search?query={}&tags=story&hitsPerPage=10", encoded),
    ]
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
        index: search::SearchIndex::new(),
        cache: cache::PageCache::new(),
        crawler: crawler::Crawler::new(),
    });

    let app = Router::new()
        .route("/", get(root))
        .route("/api/search", get(api_search))
        .route("/api/fetch", post(api_fetch))
        .route("/api/index", post(api_index))
        .route("/api/status", get(api_status))
        // Serve everything under static/ (CSS, JS, fonts if you add them)
        .nest_service("/static", ServeDir::new("static"))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = "0.0.0.0:3000";
    info!("Meridian listening on http://{}", addr);
    info!("open http://localhost:3000 in your browser");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
