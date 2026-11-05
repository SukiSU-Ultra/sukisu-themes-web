use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

const REPOSITORY: &str =
    "https://api.github.com/repos/SukiSU-Ultra/SukiSU-Ultra/commits?per_page=1";
const VERSION_MAJOR: i64 = 4;
const VERSION_BASE_COMMIT: i64 = 2815;
const CACHE_TTL: Duration = Duration::from_secs(600);

struct CachedVersion {
    value: Option<i64>,
    updated: Instant,
}

static CACHE: OnceLock<Mutex<CachedVersion>> = OnceLock::new();

fn page_count(link: Option<&str>, first_page_items: usize) -> Option<i64> {
    let Some(link) = link else {
        return Some(first_page_items as i64);
    };
    let last_page = link
        .split(',')
        .find(|part| part.contains(r#"rel="last""#))
        .and_then(|part| part.split(';').next())
        .and_then(|target| target.split("&page=").nth(1))
        .and_then(|page| page.trim_end_matches(['>', '&']).parse::<i64>().ok())
        .filter(|page| *page > 0);
    if last_page.is_some() {
        return last_page;
    }
    if link.contains(r#"rel="next""#) {
        None
    } else {
        Some(first_page_items as i64)
    }
}

pub async fn current(paths: &crate::config::Paths) -> i64 {
    let configured = crate::config::manager_version_code(paths);
    if configured > 0 {
        return configured;
    }

    let cache = CACHE.get_or_init(|| {
        Mutex::new(CachedVersion {
            value: None,
            updated: Instant::now() - CACHE_TTL,
        })
    });
    let mut cache = cache.lock().await;
    if cache.updated.elapsed() < CACHE_TTL {
        return cache.value.unwrap_or(configured);
    }

    match fetch().await {
        Ok(value) => {
            cache.value = Some(value);
            cache.updated = Instant::now();
            value
        }
        Err(error) => {
            tracing::warn!(?error, "failed to refresh manager version from GitHub");
            cache.updated = Instant::now();
            cache.value.unwrap_or(configured)
        }
    }
}

async fn fetch() -> anyhow::Result<i64> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()?;
    let response = client.get(REPOSITORY).send().await?.error_for_status()?;
    let link = response
        .headers()
        .get(reqwest::header::LINK)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let commits: Vec<serde_json::Value> = response.json().await?;
    let count = page_count(link.as_deref(), commits.len())
        .ok_or_else(|| anyhow::anyhow!("GitHub did not return a valid commit count"))?;
    let code = VERSION_MAJOR * 10_000 + count - VERSION_BASE_COMMIT;
    anyhow::ensure!(
        code > 0,
        "GitHub commit count is below the manager version baseline"
    );
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::page_count;

    #[test]
    fn reads_last_page_from_github_link_header() {
        assert_eq!(
            page_count(
                Some(
                    r#"<https://api.github.com/repos/example/repo/commits?per_page=1&page=3912>; rel="last", <https://api.github.com/repos/example/repo/commits?per_page=1&page=2>; rel="next""#
                ),
                1,
            ),
            Some(3912),
        );
    }

    #[test]
    fn uses_first_page_length_when_history_has_no_pagination() {
        assert_eq!(page_count(None, 1), Some(1));
        assert_eq!(page_count(Some(""), 0), Some(0));
    }

    #[test]
    fn rejects_incomplete_paginated_link_headers() {
        assert_eq!(
            page_count(
                Some(r#"<https://api.github.com/repos/example/repo/commits?page=2>; rel="next""#),
                1
            ),
            None,
        );
    }
}
