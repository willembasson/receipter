//! Bromley household waste collections.
//!
//! The council page people are pointed at
//! (<https://www.bromley.gov.uk/household-waste-recycling/bin-collection-days>)
//! is only a signpost; the actual service is FixMyStreet **WasteWorks** at
//! <https://recyclingservices.bromley.gov.uk>. Two requests get us there:
//!
//!   1. `POST /waste` with a postcode returns a page whose `<select>` lists
//!      every property at that postcode as `<option value="ID">ADDRESS</option>`.
//!      This must be a POST — the same query over GET returns the empty form.
//!   2. `GET /waste/<ID>` returns the collections… eventually. The first
//!      response is an interstitial reading "Loading your bin days...", while
//!      the backend fetches from the council's system. Re-requesting the same
//!      URL a few seconds later returns the real page.
//!
//! That second step is why the reference implementations reach for a headless
//! browser. They don't need to: nothing here runs JavaScript, and no session
//! cookie is required either — plain polling is enough, and in testing the page
//! resolved on the third or fourth attempt (~6s).
//!
//! Both responses are cached, so a repeat run within the cache TTL costs
//! nothing and skips the polling entirely.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;

use super::{BinSettings, Collection, Property, between, match_address, parse_collection_date};
use crate::cache::{self, Cache};
use crate::transport::postcode_from_address;

const BASE_URL: &str = "https://recyclingservices.bromley.gov.uk/waste";

/// WasteWorks serves a 503 to obviously-automated clients — the reference
/// scrapers specifically call out the `HeadlessChrome` User-Agent being
/// blocked — so present as an ordinary desktop browser. (The rest of this
/// crate sends `curl/8.0.0`, which is fine for the APIs it talks to; this site
/// is the exception.)
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36";

/// Marker text on the interstitial served while collections are being loaded.
const LOADING_MARKER: &str = "Loading your bin days";

/// How long to keep polling `/waste/<id>` for the loading page to resolve.
/// Observed load time is ~6s; 15 attempts at 2s caps the wait at ~30s before
/// falling back to the cached copy.
const MAX_ATTEMPTS: usize = 15;
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Resolve the configured address to a property and fetch its next collections.
pub async fn collections(
    cfg: &BinSettings,
    address: &str,
    today: NaiveDate,
    cache: Option<&Cache>,
) -> Result<Property> {
    let (id, matched) = property_id(cfg, address, cache).await?;
    let html = fetch_collections(&id, today, cache).await?;
    let collections = parse_collections(&html, today);
    if collections.is_empty() {
        log::warn!("no collections parsed for Bromley property `{id}`");
    }
    Ok(Property {
        id,
        address: matched,
        collections,
    })
}

/// The council's id for the configured property, plus the address it matched.
///
/// An explicit `property_id` short-circuits this entirely. Otherwise the
/// postcode lookup is cached as **stable**: a postcode's address list is fixed,
/// so it never needs refetching.
async fn property_id(
    cfg: &BinSettings,
    address: &str,
    cache: Option<&Cache>,
) -> Result<(String, String)> {
    if let Some(id) = &cfg.property_id {
        return Ok((id.clone(), cfg.property.clone().unwrap_or_default()));
    }

    let postcode = cfg
        .postcode
        .clone()
        .or_else(|| postcode_from_address(address))
        .ok_or_else(|| anyhow!("no `postcode` set in [bins] and none found in the address"))?;
    let wanted = cfg.property.as_deref().unwrap_or(address);

    let key: String = postcode
        .to_uppercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    let html = cache::cached(
        cache,
        "bins-bromley-addresses",
        &key,
        None,
        true,
        || async {
            fetch_addresses(&postcode)
                .await
                .with_context(|| format!("looking up Bromley addresses for `{postcode}`"))
        },
    )
    .await?;

    let options = parse_address_options(&html);
    if options.is_empty() {
        bail!("no addresses returned for postcode `{postcode}` — is it a Bromley postcode?");
    }
    match_address(&options, wanted)
}

/// POST the postcode to get the property list for it.
async fn fetch_addresses(postcode: &str) -> Result<String> {
    log::debug!("POST {BASE_URL} (postcode lookup)");
    let body = client()?
        .post(BASE_URL)
        .form(&[("postcode", postcode)])
        .send()
        .await
        .with_context(|| format!("requesting `{BASE_URL}`"))?
        .error_for_status()
        .with_context(|| format!("request to `{BASE_URL}` failed"))?
        .text()
        .await
        .context("reading response body")?;
    Ok(body)
}

/// Fetch a property's collections, polling past the "Loading your bin days..."
/// interstitial. Cached per property *and* per day: the page is a snapshot of
/// what is next, so yesterday's copy must not be served as today's answer.
async fn fetch_collections(id: &str, today: NaiveDate, cache: Option<&Cache>) -> Result<String> {
    let key = format!("{id}:{}", today.format("%Y-%m-%d"));
    cache::cached(
        cache,
        "bins-bromley",
        &key,
        Some(today),
        false,
        || async move {
            let url = format!("{BASE_URL}/{id}");
            for attempt in 1..=MAX_ATTEMPTS {
                log::debug!("GET {url} (attempt {attempt}/{MAX_ATTEMPTS})");
                let body = client()?
                    .get(&url)
                    .send()
                    .await
                    .with_context(|| format!("requesting `{url}`"))?
                    .error_for_status()
                    .with_context(|| format!("request to `{url}` failed"))?
                    .text()
                    .await
                    .context("reading response body")?;
                // Only a fully loaded page is worth returning (and caching).
                if !body.contains(LOADING_MARKER) {
                    return Ok(body);
                }
                tokio::time::sleep(RETRY_DELAY).await;
            }
            bail!("bin days were still loading after {MAX_ATTEMPTS} attempts")
        },
    )
    .await
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(20))
        .build()
        .context("building HTTP client")
}

// --- Parsing ---------------------------------------------------------------

/// Pull `(id, address)` out of every `<option value="ID">ADDRESS</option>` on
/// the postcode results page. The blank "choose an address" option is skipped.
fn parse_address_options(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for chunk in html.split("<option value=\"").skip(1) {
        let Some((id, rest)) = chunk.split_once('"') else {
            continue;
        };
        let Some(text) = between(rest, ">", "</option>") else {
            continue;
        };
        let address = super::strip_tags(text);
        if !id.is_empty() && !address.is_empty() {
            out.push((id.to_string(), address));
        }
    }
    out
}

/// Extract each service and its next collection date from a loaded property
/// page. Services with no "Next collection" row (Bulky waste, and the
/// batteries/textiles request service) are on-demand rather than scheduled, so
/// they simply drop out.
fn parse_collections(html: &str, today: NaiveDate) -> Vec<Collection> {
    let mut out = Vec::new();
    // Split on the exact container attribute: the modifier classes
    // (`waste-service-grid--service-name`) share the prefix but not the `">`.
    for chunk in html.split("class=\"waste-service-grid\">").skip(1) {
        let Some(name) = between(chunk, "waste-service-name\">", "</h3>") else {
            continue;
        };
        let service = super::strip_tags(name);
        let Some(next) = summary_value(chunk, "Next collection") else {
            continue;
        };
        let Some(date) = parse_collection_date(&next, today) else {
            log::debug!("unparseable next collection for `{service}`: {next:?}");
            continue;
        };
        if !service.is_empty() {
            out.push(Collection { service, date });
        }
    }
    out
}

/// The `<dd>` value paired with a given `<dt>` key in a GOV.UK summary list.
fn summary_value(chunk: &str, key: &str) -> Option<String> {
    let after = chunk.split(&format!("{key}</dt>")).nth(1)?;
    let dd = between(after, "<dd", "</dd>")?;
    let inner = dd.split_once('>')?.1;
    let value = super::strip_tags(inner);
    // Adjusted collections carry an explanatory aside; the date is what matters.
    Some(
        value
            .replace(
                "(this collection has been adjusted from its usual time)",
                "",
            )
            .trim()
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    /// Trimmed from a real `POST /waste` response.
    const ADDRESSES: &str = r#"
        <select class="govuk-select js-autocomplete" id="address" name="address" required>
          <option value=""></option>
          <option value="3678997">1 Example Close, Townsville, AB1 2CD</option>
          <option value="3678998">11 Example Close, Townsville, AB1 2CD</option>
          <option value="3678999">12 Example Close, Townsville, AB1 2CD</option>
        </select>"#;

    /// Trimmed from a real `GET /waste/<id>` response, keeping the markup that
    /// matters: the container class, the service heading, and the summary list.
    const COLLECTIONS: &str = r#"
      <div class="waste__collections">
        <div class="waste-service-grid">
          <div class="waste-service-grid--service-name">
            <h3 class="govuk-heading-m waste-service-name">
              Food Waste
            </h3>
          </div>
          <div class="waste-service-grid--service-description">
            <dl class="govuk-summary-list">
              <div class="govuk-summary-list__row">
                <dt class="govuk-summary-list__key">Frequency</dt>
                <dd class="govuk-summary-list__value">Every Thursday</dd>
              </div>
              <div class="govuk-summary-list__row">
                <dt class="govuk-summary-list__key">Next collection</dt>
                <dd class="govuk-summary-list__value">
                  Thursday, 13th August
                </dd>
              </div>
              <div class="govuk-summary-list__row">
                <dt class="govuk-summary-list__key">Last collection</dt>
                <dd class="govuk-summary-list__value">Thursday, 6th August, at  8:50am</dd>
              </div>
            </dl>
          </div>
        </div>
        <div class="waste-service-grid">
          <div class="waste-service-grid--service-name">
            <h3 class="govuk-heading-m waste-service-name">
              Mixed Recycling (Cans, Plastics &amp; Glass)
            </h3>
          </div>
          <dl class="govuk-summary-list">
            <div class="govuk-summary-list__row">
              <dt class="govuk-summary-list__key">Next collection</dt>
              <dd class="govuk-summary-list__value">
                Tuesday, 18th August
                (this collection has been adjusted from its usual time)
              </dd>
            </div>
          </dl>
        </div>
        <div class="waste-service-grid">
          <div class="waste-service-grid--service-name">
            <h3 class="govuk-heading-m waste-service-name">Bulky waste</h3>
          </div>
        </div>
      </div>"#;

    #[test]
    fn reads_every_address_option() {
        let options = parse_address_options(ADDRESSES);
        assert_eq!(options.len(), 3, "the blank option should be skipped");
        assert_eq!(options[1].0, "3678998");
        assert_eq!(options[1].1, "11 Example Close, Townsville, AB1 2CD");
    }

    #[test]
    fn matches_the_full_address() {
        let options = parse_address_options(ADDRESSES);
        let (id, _) = match_address(&options, "11 Example Close, Townsville, AB1 2CD").unwrap();
        assert_eq!(id, "3678998");
    }

    #[test]
    fn matches_on_house_and_street_alone() {
        let options = parse_address_options(ADDRESSES);
        let (id, addr) = match_address(&options, "11 Example Close").unwrap();
        assert_eq!(id, "3678998");
        assert_eq!(addr, "11 Example Close, Townsville, AB1 2CD");
    }

    #[test]
    fn does_not_confuse_house_1_with_house_11() {
        let options = parse_address_options(ADDRESSES);
        assert_eq!(
            match_address(&options, "1 Example Close").unwrap().0,
            "3678997"
        );
    }

    #[test]
    fn unmatched_addresses_suggest_a_fix() {
        let options = parse_address_options(ADDRESSES);
        let err = match_address(&options, "99 Nowhere Road")
            .unwrap_err()
            .to_string();
        assert!(err.contains("property_id"), "{err}");
    }

    #[test]
    fn reads_scheduled_services_only() {
        let got = parse_collections(COLLECTIONS, date("2026-08-07"));
        assert_eq!(
            got,
            vec![
                Collection {
                    service: "Food Waste".into(),
                    date: date("2026-08-13"),
                },
                Collection {
                    service: "Mixed Recycling (Cans, Plastics & Glass)".into(),
                    date: date("2026-08-18"),
                },
            ],
            "on-demand services without a next collection should be dropped"
        );
    }

    #[test]
    fn ignores_the_last_collection_row() {
        // "Last collection" is 6 August; nothing should report that date.
        let got = parse_collections(COLLECTIONS, date("2026-08-07"));
        assert!(got.iter().all(|c| c.date > date("2026-08-07")));
    }

    #[test]
    fn the_loading_interstitial_yields_nothing() {
        let html = "<div class=\"waste\"><p>Loading your bin days...</p></div>";
        assert!(parse_collections(html, date("2026-08-07")).is_empty());
    }
}
