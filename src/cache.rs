// On-disk page cache. Clean HTML is stored at:
//   ./cache/<sha256-of-url-first-16-chars>/<sha256-of-url>.html
//
// The two-level directory splits prevent any single directory from
// holding too many files (a filesystem performance concern at scale).
//
// Metadata (title, fetch time) lives in a sidecar .json file.

use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use sha2::{Sha256, Digest};

const CACHE_DIR: &str = "./cache";

#[derive(Debug, Serialize, Deserialize)]
pub struct CacheMeta {
    pub url: String,
    pub title: String,
    pub fetched_at: DateTime<Utc>,
    pub content_length: usize,
}

pub struct PageCache {
    base: PathBuf,
}

impl PageCache {
    pub fn new() -> Self {
        let base = PathBuf::from(CACHE_DIR);
        std::fs::create_dir_all(&base).expect("cannot create cache dir");
        Self { base }
    }

    fn url_hash(url: &str) -> String {
        let mut h = Sha256::new();
        h.update(url.as_bytes());
        hex::encode(h.finalize())
    }

    fn paths(&self, url: &str) -> (PathBuf, PathBuf, PathBuf) {
        let hash = Self::url_hash(url);
        let shard = &hash[..2];
        let dir = self.base.join(shard).join(&hash[2..4]);
        let html = dir.join(format!("{}.html", hash));
        let meta = dir.join(format!("{}.json", hash));
        (dir, html, meta)
    }

    pub fn contains(&self, url: &str) -> bool {
        let (_, html, _) = self.paths(url);
        html.exists()
    }

    pub fn get_html(&self, url: &str) -> Option<String> {
        let (_, html_path, _) = self.paths(url);
        std::fs::read_to_string(html_path).ok()
    }

    pub fn get_meta(&self, url: &str) -> Option<CacheMeta> {
        let (_, _, meta_path) = self.paths(url);
        let raw = std::fs::read_to_string(meta_path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn store(&self, url: &str, title: &str, clean_html: &str) -> std::io::Result<()> {
        let (dir, html_path, meta_path) = self.paths(url);
        std::fs::create_dir_all(&dir)?;

        std::fs::write(&html_path, clean_html)?;

        let meta = CacheMeta {
            url: url.to_string(),
            title: title.to_string(),
            fetched_at: Utc::now(),
            content_length: clean_html.len(),
        };
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;

        Ok(())
    }

    /// Returns (total_entries, total_bytes_on_disk)
    pub fn stats(&self) -> (usize, u64) {
        let mut count = 0usize;
        let mut bytes = 0u64;
        if let Ok(rd) = std::fs::read_dir(&self.base) {
            for shard in rd.flatten() {
                if let Ok(inner) = std::fs::read_dir(shard.path()) {
                    for subshard in inner.flatten() {
                        if let Ok(files) = std::fs::read_dir(subshard.path()) {
                            for f in files.flatten() {
                                if f.path().extension().map(|e| e == "html").unwrap_or(false) {
                                    count += 1;
                                    bytes += f.metadata().map(|m| m.len()).unwrap_or(0);
                                }
                            }
                        }
                    }
                }
            }
        }
        (count, bytes)
    }
}
