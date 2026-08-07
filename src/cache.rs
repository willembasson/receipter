//! SQLite-backed cache for external API calls (weather and transport).
//!
//! Every external call is stored with its kind, a key encoding all its params
//! (including any date), the time of the call, and the raw response. This serves
//! three purposes:
//!   * **Cache**: identical params within a TTL are served from the DB instead
//!     of hitting the network, conserving API quota.
//!   * **History**: because each dated call is filed under its date, a later
//!     request for a now-past date returns the stored result even though the
//!     upstream service can no longer produce it.
//!   * **Resilience**: if a live fetch fails, the newest stored response is used
//!     as a fallback.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate};
use rusqlite::{Connection, OptionalExtension, params};

pub struct Cache {
    conn: Connection,
    ttl_minutes: i64,
    /// When true, fresh reads are skipped so live data is always refetched (and
    /// re-stored). History and failure-fallback still work.
    refresh: bool,
}

impl Cache {
    /// Open (creating if needed) the cache database at `path`.
    pub fn open(path: &Path, ttl_minutes: i64, refresh: bool) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening cache database `{}`", path.display()))?;
        Self::from_conn(conn, ttl_minutes, refresh)
    }

    fn from_conn(conn: Connection, ttl_minutes: i64, refresh: bool) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS api_cache (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 kind         TEXT NOT NULL,
                 cache_key    TEXT NOT NULL,
                 for_date     TEXT,
                 requested_at TEXT NOT NULL,
                 response     TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_api_cache_lookup
                 ON api_cache (kind, cache_key, id);",
        )
        .context("initialising cache schema")?;
        Ok(Self {
            conn,
            ttl_minutes,
            refresh,
        })
    }

    /// Store a response. `key` should encode every param (including any date);
    /// `for_date` is the date the data pertains to, if any.
    pub fn store(
        &self,
        kind: &str,
        key: &str,
        for_date: Option<NaiveDate>,
        response: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO api_cache (kind, cache_key, for_date, requested_at, response)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    kind,
                    key,
                    for_date.map(|d| d.format("%Y-%m-%d").to_string()),
                    Local::now().to_rfc3339(),
                    response,
                ],
            )
            .context("writing to cache")?;
        log::trace!("stored {kind} `{key}` (for_date {for_date:?})");
        Ok(())
    }

    /// The newest stored response for `(kind, key)`, ignoring age. Used for
    /// historic (past-date) lookups.
    pub fn lookup(&self, kind: &str, key: &str) -> Result<Option<String>> {
        Ok(self.latest(kind, key)?.map(|(_, resp)| resp))
    }

    /// A cached response for `(kind, key)` if the freshness policy accepts it:
    /// past dates are always served, `stable` kinds never expire, otherwise the
    /// entry must be younger than the TTL. `--refresh` suppresses non-historic
    /// hits.
    pub fn get_fresh(
        &self,
        kind: &str,
        key: &str,
        for_date: Option<NaiveDate>,
        stable: bool,
    ) -> Result<Option<String>> {
        let Some((requested_at, response)) = self.latest(kind, key)? else {
            log::debug!("cache miss: {kind} `{key}` (no stored entry)");
            return Ok(None);
        };

        let today = Local::now().date_naive();
        if for_date.is_some_and(|d| d < today) {
            log::debug!("cache hit: {kind} `{key}` (historic {for_date:?})");
            return Ok(Some(response)); // historic: always valid
        }
        if self.refresh {
            log::debug!("cache skipped: {kind} `{key}` (--refresh)");
            return Ok(None);
        }
        if stable {
            log::debug!("cache hit: {kind} `{key}` (stable)");
            return Ok(Some(response));
        }
        let age = Local::now()
            .signed_duration_since(requested_at)
            .num_minutes();
        if age < self.ttl_minutes {
            log::debug!(
                "cache hit: {kind} `{key}` (age {age} min < ttl {})",
                self.ttl_minutes
            );
            Ok(Some(response))
        } else {
            log::debug!(
                "cache stale: {kind} `{key}` (age {age} min >= ttl {})",
                self.ttl_minutes
            );
            Ok(None)
        }
    }

    /// Return a fresh cached value, or run `fetch`, store, and return it. On a
    /// fetch error, fall back to the newest stored response if there is one.
    pub async fn get_or_fetch<F, Fut>(
        &self,
        kind: &str,
        key: &str,
        for_date: Option<NaiveDate>,
        stable: bool,
        fetch: F,
    ) -> Result<String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<String>>,
    {
        if let Some(hit) = self.get_fresh(kind, key, for_date, stable)? {
            return Ok(hit);
        }
        match fetch().await {
            Ok(body) => {
                log::debug!(
                    "fetched {kind} `{key}` from network; storing ({} bytes)",
                    body.len()
                );
                self.store(kind, key, for_date, &body)?;
                Ok(body)
            }
            Err(e) => match self.lookup(kind, key)? {
                Some(old) => {
                    log::warn!("{kind} fetch failed ({e:#}); using cached copy");
                    eprintln!("warning: {kind} fetch failed ({e:#}); using cached copy");
                    Ok(old)
                }
                None => Err(e),
            },
        }
    }

    fn latest(&self, kind: &str, key: &str) -> Result<Option<(DateTime<Local>, String)>> {
        let row = self
            .conn
            .query_row(
                "SELECT requested_at, response FROM api_cache
                 WHERE kind = ?1 AND cache_key = ?2
                 ORDER BY id DESC LIMIT 1",
                params![kind, key],
                |row| {
                    let ts: String = row.get(0)?;
                    let resp: String = row.get(1)?;
                    Ok((ts, resp))
                },
            )
            .optional()
            .context("reading from cache")?;

        match row {
            Some((ts, resp)) => {
                let parsed = DateTime::parse_from_rfc3339(&ts)
                    .map(|dt| dt.with_timezone(&Local))
                    .unwrap_or_else(|_| Local::now());
                Ok(Some((parsed, resp)))
            }
            None => Ok(None),
        }
    }
}

/// Convenience wrapper: use the cache when present, otherwise just fetch.
pub async fn cached<F, Fut>(
    cache: Option<&Cache>,
    kind: &str,
    key: &str,
    for_date: Option<NaiveDate>,
    stable: bool,
    fetch: F,
) -> Result<String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    match cache {
        Some(c) => c.get_or_fetch(kind, key, for_date, stable, fetch).await,
        None => fetch().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn mem(ttl_minutes: i64, refresh: bool) -> Cache {
        Cache::from_conn(Connection::open_in_memory().unwrap(), ttl_minutes, refresh).unwrap()
    }

    #[tokio::test]
    async fn serves_a_fresh_hit_without_refetching() {
        let cache = mem(60, false);
        let calls = Cell::new(0);

        let a = cache
            .get_or_fetch("weather", "k", None, false, || async {
                calls.set(calls.get() + 1);
                Ok("body".to_string())
            })
            .await
            .unwrap();
        let b = cache
            .get_or_fetch("weather", "k", None, false, || async {
                calls.set(calls.get() + 1);
                Ok("body2".to_string())
            })
            .await
            .unwrap();

        assert_eq!(a, "body");
        assert_eq!(b, "body"); // second call served from cache
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn expired_entries_refetch() {
        let cache = mem(0, false); // nothing is ever "fresh"
        let calls = Cell::new(0);
        for _ in 0..2 {
            cache
                .get_or_fetch("weather", "k", None, false, || async {
                    calls.set(calls.get() + 1);
                    Ok("body".to_string())
                })
                .await
                .unwrap();
        }
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn past_dates_are_served_from_history() {
        let cache = mem(0, false); // expired for live, but past dates persist
        let yesterday = Local::now().date_naive() - chrono::Days::new(1);
        cache.store("weather", "k", Some(yesterday), "old").unwrap();

        let hit = cache
            .get_fresh("weather", "k", Some(yesterday), false)
            .unwrap();
        assert_eq!(hit.as_deref(), Some("old"));
    }

    #[tokio::test]
    async fn falls_back_to_cache_on_fetch_error() {
        let cache = mem(0, false);
        cache.store("train", "k", None, "cached").unwrap();
        let got = cache
            .get_or_fetch("train", "k", None, false, || async {
                Err(anyhow::anyhow!("network down"))
            })
            .await
            .unwrap();
        assert_eq!(got, "cached");
    }
}
