//! The passive "newer version available" notice (upstream
//! `update_check.py`), checked against this repository's GitHub Releases.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::endpoints::Endpoints;
use crate::paths::Paths;
use crate::pyjson;

/// Not upstream's `update_check.json`: cswap compares a different version
/// line and would keep overwriting ccswap's answer.
pub const CACHE_FILENAME: &str = "ccswap_update_check.json";
const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
const TIMEOUT: Duration = Duration::from_secs(2);

pub const UPGRADE_INSTRUCTIONS: &str = "To upgrade ccswap, download the latest release from:
  https://github.com/ashigirl96/ccswap/releases/latest
or build it from source:
  cargo install --git https://github.com/ashigirl96/ccswap ccswap";

pub fn cache_path(paths: &Paths) -> PathBuf {
    paths.usage_cache_dir().join(CACHE_FILENAME)
}

/// The notice when a release newer than `current` exists. Never fails: an
/// unreachable server is cached as "unknown" for a day so offline runs do
/// not pay the timeout every time.
pub fn check_for_update(paths: &Paths, endpoints: &Endpoints, current: &str) -> Option<String> {
    let path = cache_path(paths);
    let latest = match read_cache(&path) {
        Some(cached) => cached,
        None => {
            let fetched = fetch_latest(&endpoints.update_check_url);
            let _ = write_cache(&path, fetched.as_deref());
            fetched
        }
    }?;
    (parse_version(&latest)? > parse_version(current)?).then(|| {
        format!(
            "A newer version of ccswap is available ({latest}). You are using {current}. Run `ccswap upgrade` for upgrade instructions."
        )
    })
}

/// `Some(data)` for a fresh entry, where a cached failure is `Some(None)`.
fn read_cache(path: &Path) -> Option<Option<String>> {
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let timestamp = raw.get("timestamp")?.as_f64()?;
    let data = raw.get("data")?;
    if now() - timestamp >= CACHE_TTL.as_secs_f64() {
        return None;
    }
    Some(data.as_str().map(str::to_string))
}

fn write_cache(path: &Path, data: Option<&str>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        path,
        pyjson::to_string_compact(&json!({"timestamp": now(), "data": data})),
    )
}

fn fetch_latest(url: &str) -> Option<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", &format!("ccswap/{}", crate::VERSION))
        .call()
        .ok()?;
    let body: Value = serde_json::from_str(&response.body_mut().read_to_string().ok()?).ok()?;
    let tag = body.get("tag_name")?.as_str()?;
    Some(tag.strip_prefix('v').unwrap_or(tag).to_string())
}

/// Dotted integers, compared like Python tuples; anything else is `None`.
fn parse_version(version: &str) -> Option<Vec<u64>> {
    version
        .split('.')
        .map(|part| {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            part.parse().ok()
        })
        .collect()
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_part_by_part() {
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
        assert!(parse_version("1.2.0") > parse_version("1.2"));
        assert_eq!(parse_version("1.0.0rc1"), None);
        assert_eq!(parse_version("1..0"), None);
    }
}
