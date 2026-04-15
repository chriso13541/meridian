mod cache;
mod crawler;
mod interest;
mod queue;
mod search;
mod tracker_strip;

const DATA_DIR:       &str = "./data";
const INDEX_PATH:    &str = "./data/index.json";
const INTERESTS_PATH:&str = "./data/interests.json";
const VISITED_PATH:  &str = "./data/visited.json";

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
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct AppState {
    index:     search::SearchIndex,
    cache:     cache::PageCache,
    crawler:   crawler::Crawler,
    interests: interest::InterestGraph,
    queue:     queue::CrawlQueue,
}

type SharedState = Arc<AppState>;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    #[serde(default = "default_page")]
    page: usize,
}
fn default_page() -> usize { 1 }

#[derive(Deserialize)]
struct FetchParams { url: String }

#[derive(Serialize)]
struct SearchResponse {
    query:         String,
    page:          usize,
    results:       Vec<search::SearchResult>,
    total_indexed: usize,
    total_cached:  usize,
    total_known:   usize,
    elapsed_ms:    u128,
    interests:     Vec<interest::InterestEntry>,
}

#[derive(Serialize)]
struct StatusResponse {
    indexed:          usize,
    cached:           usize,
    known:            usize,
    cache_bytes:      u64,
    queue_pending:    usize,
    queue_visited:    usize,
    interest_terms:   usize,
    interest_domains: usize,
}

#[derive(Serialize)]
struct InterestsResponse {
    terms:   Vec<interest::InterestEntry>,
    domains: Vec<interest::InterestEntry>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

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

    // Boost queue priority for URLs matching this query
    boost_queue_for_query(&state, &params.q);

    const PER_PAGE: usize = 25;
    let offset = (params.page.saturating_sub(1)) * PER_PAGE;

    let results: Vec<search::SearchResult> = state
        .index
        .search(&params.q, offset + PER_PAGE)
        .into_iter()
        .skip(offset)
        .collect();

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

/// When a user searches for something, find indexed entries whose title/snippet
/// match the query and bump their queue priority so they get re-crawled sooner.
fn boost_queue_for_query(state: &AppState, _query: &str) {
    let candidates = state.index.candidates_for_crawl(200);
    for (url, title, snippet) in candidates {
        let score = state.interests.score(&url, &title, &snippet);
        if score > 0.5 {
            state.queue.push(&url, score + 5.0); // +5 boost for user-triggered
        }
    }
}

async fn api_fetch(
    State(state): State<SharedState>,
    Query(params): Query<FetchParams>,
) -> impl IntoResponse {
    let url = &params.url;
    let title_hint = state.index.get_title(url).unwrap_or_default();
    state.interests.record_visit(url, &title_hint);

    // Also queue any Known links from this page at elevated priority
    if let Some(domain) = crawler::Crawler::extract_domain(url) {
        state.queue.push(url, 3.0); // re-crawl this page sooner
        drop(domain);
    }

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
                warn!("cache write failed for {}: {}", url, e);
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

    // Add outbound links to the crawl queue
    state.queue.push_many(&report.links, 1.0);

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
        queue_pending:    state.queue.pending_len(),
        queue_visited:    state.queue.visited_len(),
        interest_terms:   state.interests.term_count(),
        interest_domains: state.interests.domain_count(),
    })
}

// ---------------------------------------------------------------------------
// Full crawler loop
// ---------------------------------------------------------------------------

/// Loads seed URLs from seeds.txt (one URL per line, # = comment).
/// Falls back to a built-in list if the file doesn't exist.
async fn load_seeds() -> Vec<String> {
    let from_file: Vec<String> = tokio::fs::read_to_string("seeds.txt").await
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    if !from_file.is_empty() {
        info!("loaded {} seeds from seeds.txt", from_file.len());
        return from_file;
    }

    // Built-in seed list — broad enough to bootstrap a useful index
    let builtin = vec![
        // Tech news & aggregators
        "https://news.ycombinator.com",
        "https://lobste.rs",
        "https://thenewstack.io",
        "https://arstechnica.com",

        // AI / ML
        "https://www.anthropic.com/news",
        "https://openai.com/blog",
        "https://huggingface.co/blog",
        "https://lilianweng.github.io",
        "https://simonwillison.net",

        // Dev reference
        "https://doc.rust-lang.org/book/",
        "https://developer.mozilla.org/en-US/docs/Web",
        "https://stackoverflow.com/questions?tab=Votes",

        // Self-hosting / homelab
        "https://selfhosted.show",
        "https://www.reddit.com/r/selfhosted/.rss",
        "https://noted.lol",

        // General reference
        "https://en.wikipedia.org/wiki/Main_Page",
        "https://www.w3.org",
    ];

    info!("no seeds.txt found, using {} built-in seeds", builtin.len());
    builtin.into_iter().map(|s| s.to_string()).collect()
}

/// Main crawl loop. Runs forever:
///   - Pops `batch_size` URLs from the queue
///   - Crawls each one (respecting robots.txt + rate limits)
///   - Indexes the result
///   - Pushes outbound links back into the queue, scored by the interest graph
///   - Sleeps `tick` between batches to stay gentle
async fn crawl_loop(state: SharedState, tick: Duration, batch_size: usize) {
    // Seed the queue
    let seeds = load_seeds().await;
    state.queue.push_many(&seeds, 1.0);
    info!("crawl loop started — {} urls seeded, tick={:?}, batch={}", seeds.len(), tick, batch_size);

    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        let batch = state.queue.pop_batch(batch_size);

        if batch.is_empty() {
            debug!("crawl loop: queue empty, re-seeding from index candidates");
            // Re-seed from Known entries that haven't been crawled yet
            let candidates = state.index.candidates_for_crawl(100);
            for (url, _title, _snippet) in candidates {
                state.queue.push(&url, 0.5);
            }
            continue;
        }

        info!("crawl loop: crawling {} urls (queue: {} pending, {} visited)",
            batch.len(), state.queue.pending_len(), state.queue.visited_len());

        for url in batch {
            let Some(result) = state.crawler.crawl_one(&url).await else {
                continue;
            };

            // Score outbound links against the interest graph before queuing
            for link in &result.outbound_links {
                let score = if state.interests.term_count() > 0 {
                    // Use the link URL and source page title as scoring signal
                    state.interests.score(link, &result.entry.title, "")
                } else {
                    0.5 // flat score until we have interest data
                };
                state.queue.push(link, score);
            }

            debug!("indexed: {} (+{} links)", url, result.outbound_links.len());
            state.index.upsert(result.entry);
        }
    }
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

fn save_all(state: &AppState) {
    if let Err(e) = state.index.save(INDEX_PATH) {
        tracing::warn!("index save failed: {}", e);
    }
    if let Err(e) = state.interests.save(INTERESTS_PATH) {
        tracing::warn!("interests save failed: {}", e);
    }
    if let Err(e) = state.queue.save_visited(VISITED_PATH) {
        tracing::warn!("visited save failed: {}", e);
    }
    tracing::info!("data saved to {}", DATA_DIR);
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("meridian=info".parse().unwrap()),
        )
        .init();

    std::fs::create_dir_all(DATA_DIR).expect("cannot create data dir");

    // Clean up any .tmp files left by a previous crash mid-save
    for name in ["index.json.tmp", "interests.json.tmp", "visited.json.tmp"] {
        let p = format!("{}/{}", DATA_DIR, name);
        if std::path::Path::new(&p).exists() {
            let _ = std::fs::remove_file(&p);
            info!("removed stale tmp file: {}", p);
        }
    }

    info!("loading persisted data…");
    let state: SharedState = Arc::new(AppState {
        index:     search::SearchIndex::load(INDEX_PATH),
        cache:     cache::PageCache::new(),
        crawler:   crawler::Crawler::new(),
        interests: interest::InterestGraph::load(INTERESTS_PATH),
        queue:     queue::CrawlQueue::load_visited(VISITED_PATH),
    });

    // Crawler: 4s tick, 35 urls/batch → ~400-600+ pages/min
    {
        let s = Arc::clone(&state);
        tokio::spawn(crawl_loop(s, Duration::from_secs(4), 35));
    }

    // Periodic save every 5 minutes
    {
        let s = Arc::clone(&state);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(300));
            ticker.tick().await; // skip first immediate tick
            loop {
                ticker.tick().await;
                save_all(&s);
            }
        });
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
        .with_state(state.clone());

    let addr = "0.0.0.0:3000";
    info!("Meridian listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    // Catch Ctrl-C and save before exit
    let state_shutdown = Arc::clone(&state);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("shutting down — saving data…");
        save_all(&state_shutdown);
        std::process::exit(0);
    });

    axum::serve(listener, app).await.unwrap();
}
