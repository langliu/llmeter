//! Public repository metadata shown in the app sidebar.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

pub const GITHUB_REPO_URL: &str = "https://github.com/langliu/llmeter";

const GITHUB_API_REPO_URL: &str = "https://api.github.com/repos/langliu/llmeter";

/// Reads the public stargazers count for the project. Network or API
/// failures are not user-facing errors; they simply hide the count until a
/// later attempt succeeds.
pub fn github_stargazers() -> Option<u64> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .build(),
    );
    let mut response = agent
        .get(GITHUB_API_REPO_URL)
        .header("User-Agent", "Mozilla/5.0 LLMeter/0.1")
        .header("Accept", "application/vnd.github+json")
        .call()
        .ok()?;
    if !(200..300).contains(&response.status().as_u16()) {
        return None;
    }
    let body = response.body_mut().read_json::<Value>().ok()?;
    body.get("stargazers_count")?.as_u64()
}

const STARS_CACHE_FILE: &str = "github_stars.txt";

/// Fetches the stargazers count, falling back to the last successful count
/// from `cache_dir` when the API is unreachable or rate-limited (the
/// unauthenticated quota is 60 requests/hour per IP). A successful fetch
/// refreshes the cache.
pub fn cached_github_stargazers(cache_dir: &Path) -> Option<u64> {
    match github_stargazers() {
        Some(count) => {
            write_stars_cache(cache_dir, count);
            Some(count)
        }
        None => read_stars_cache(cache_dir),
    }
}

fn write_stars_cache(cache_dir: &Path, count: u64) {
    let _ = std::fs::create_dir_all(cache_dir);
    let _ = std::fs::write(cache_dir.join(STARS_CACHE_FILE), count.to_string());
}

fn read_stars_cache(cache_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(cache_dir.join(STARS_CACHE_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_url_matches_api_repo() {
        assert!(GITHUB_API_REPO_URL.ends_with("langliu/llmeter"));
        assert!(GITHUB_REPO_URL.starts_with("https://github.com/"));
    }

    #[test]
    fn stars_cache_round_trips() {
        let dir = std::env::temp_dir().join(format!("llmeter-stars-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(read_stars_cache(&dir), None, "no cache file yet");
        write_stars_cache(&dir, 2224);
        assert_eq!(read_stars_cache(&dir), Some(2224));

        std::fs::remove_dir_all(&dir).ok();
    }
}
