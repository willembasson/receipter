//! Hackney household waste collections.
//!
//! The council page people are pointed at
//! (<https://www.hackney.gov.uk/rubbish-and-recycling/rubbish-recycling-and-food-waste-collections/check-your-collection-day>)
//! embeds a Nuxt single-page app, which in turn talks to an **unauthenticated
//! JSON API** — so unlike Bromley there is no HTML to scrape. The app's
//! `API_URI` and `TENANT_ID` are baked into the page's `window.__NUXT__` blob;
//! they're reproduced here as constants.
//!
//! Getting from a postcode to a set of dates takes four hops:
//!
//!   1. `POST /property/opensearch` with the postcode → the properties there,
//!      each with a `systemId`.
//!   2. `GET /alloywastepages/getproperty/<systemId>` → a comma-separated list
//!      of **bin** ids in `attributes_wasteContainersAssignableWasteContainers`.
//!   3. `GET /alloywastepages/getbin/<binId>` → that container's label and
//!      whether it is live.
//!   4. `GET /alloywastepages/getcollection/<binId>` → the schedule *workflow*
//!      ids, then `GET /alloywastepages/getworkflow/<id>` → the actual dates.
//!
//! That's a dozen or so requests for one property, which is exactly why every
//! response is cached individually: a repeat run makes none at all, and bins
//! that share a collection round share a cached workflow.
//!
//! **On service names:** Hackney labels a collection by its *container*
//! ("Recycling Sack", "Wheeled Bin (180ltr)", "1 x ES_Food 240 litres") rather
//! than by service, and the only other hint in the API is a bin-type id that
//! maps to an icon image. Some of those icons are ambiguous, so rather than
//! guess a service name and risk mislabelling a bin, this uses the council's
//! own label — the same text a resident sees on Hackney's own page.

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;
use serde::Deserialize;

use super::{BinSettings, Collection, Property, collapse_ws, match_address};
use crate::cache::{self, Cache};
use crate::transport::postcode_from_address;

/// Taken from `window.__NUXT__.config` on the council's waste pages.
const API_URI: &str = "https://waste-api-hackney-live.ieg4.net";
const TENANT_ID: &str = "f806d91c-e133-43a6-ba9a-c0ae4f4cccf6";

/// The property search is filtered to residential premises, mirroring what the
/// council's own front end sends.
const RESIDENTIAL_FILTER: &str = r#"{"Filter":"attributes_premisesBlpuClass","Include":true,"StringMatch":"Prefix","Value":"R"}"#;

/// Resolve the configured address to a property and fetch its next collections.
pub async fn collections(
    cfg: &BinSettings,
    address: &str,
    today: NaiveDate,
    cache: Option<&Cache>,
) -> Result<Property> {
    let (id, matched) = property_id(cfg, address, cache).await?;

    let mut collections = Vec::new();
    for bin_id in bin_ids(&id, today, cache).await? {
        // One awkward bin shouldn't cost you the whole section, so a failure
        // here warns and moves on to the next container.
        match upcoming_collections(&bin_id, today, cache).await {
            Ok(found) if found.is_empty() => {
                log::debug!("bin `{bin_id}` has no upcoming collection")
            }
            Ok(found) => collections.extend(found),
            Err(e) => eprintln!("warning: Hackney bin `{bin_id}` lookup failed: {e:#}"),
        }
    }
    if collections.is_empty() {
        log::warn!("no collections found for Hackney property `{id}`");
    }
    Ok(Property {
        id,
        address: matched,
        collections,
    })
}

/// The council's id for the configured property, plus the address it matched.
/// The postcode's address list is cached as **stable**: it never changes.
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
    let body = cache::cached(
        cache,
        "bins-hackney-addresses",
        &key,
        None,
        true,
        || async {
            let url = format!("{API_URI}/{TENANT_ID}/property/opensearch");
            let payload =
                format!(r#"{{"Postcode":"{postcode}","Filters":[{RESIDENTIAL_FILTER}]}}"#);
            post(&url, payload)
                .await
                .with_context(|| format!("looking up Hackney addresses for `{postcode}`"))
        },
    )
    .await?;

    let options = parse_addresses(&body)?;
    if options.is_empty() {
        bail!("no addresses returned for postcode `{postcode}` — is it a Hackney postcode?");
    }
    match_address(&options, wanted)
}

/// The bin containers assigned to a property.
async fn bin_ids(property: &str, today: NaiveDate, cache: Option<&Cache>) -> Result<Vec<String>> {
    let url = format!("{API_URI}/{TENANT_ID}/alloywastepages/getproperty/{property}");
    let body = cached_get(cache, "bins-hackney-property", property, today, url).await?;
    parse_bin_ids(&body)
}

/// How many future dates to keep per container.
///
/// Hackney publishes a full rolling schedule (a hundred-odd dates reaching a
/// year out), and reporting only the very next one would quietly break
/// `--date`/`--bins` for any day beyond it. Keeping a run of them means the
/// day-before gate works for future dates too; eight is roughly two months of
/// a weekly round, which is well past anything worth printing.
const MAX_DATES_PER_BIN: usize = 8;

/// The upcoming collections for one container. Empty when it is disabled, not
/// on a live round, or has no dates left in its schedule.
async fn upcoming_collections(
    bin_id: &str,
    today: NaiveDate,
    cache: Option<&Cache>,
) -> Result<Vec<Collection>> {
    let url = format!("{API_URI}/{TENANT_ID}/alloywastepages/getbin/{bin_id}");
    let body = cached_get(cache, "bins-hackney-bin", bin_id, today, url).await?;
    let Some(service) = parse_service(&body)? else {
        return Ok(Vec::new());
    };

    let url = format!("{API_URI}/{TENANT_ID}/alloywastepages/getcollection/{bin_id}");
    let body = cached_get(cache, "bins-hackney-collection", bin_id, today, url).await?;

    // A container can sit on more than one round, so gather every workflow's
    // dates before trimming.
    let mut dates = Vec::new();
    for workflow in parse_workflow_ids(&body)? {
        let url = format!("{API_URI}/{TENANT_ID}/alloywastepages/getworkflow/{workflow}");
        let body = cached_get(cache, "bins-hackney-workflow", &workflow, today, url).await?;
        dates.extend(parse_dates(&body)?);
    }
    // The schedule reaches back years, so drop what has already happened.
    dates.retain(|d| *d >= today);
    dates.sort_unstable();
    dates.dedup();
    dates.truncate(MAX_DATES_PER_BIN);

    Ok(dates
        .into_iter()
        .map(|date| Collection {
            service: service.clone(),
            date,
        })
        .collect())
}

// --- Parsing ---------------------------------------------------------------

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default, rename = "addressSummaries")]
    address_summaries: Vec<AddressSummary>,
}

#[derive(Deserialize)]
struct AddressSummary {
    summary: String,
    #[serde(rename = "systemId")]
    system_id: String,
}

/// `(systemId, address)` for every property at the postcode. Hackney pads its
/// summaries with runs of spaces ("Flat 1     Appleton Court"), so they are
/// tidied before use.
fn parse_addresses(body: &str) -> Result<Vec<(String, String)>> {
    let data: SearchResponse =
        serde_json::from_str(body).context("parsing Hackney address search")?;
    Ok(data
        .address_summaries
        .into_iter()
        .map(|a| (a.system_id, collapse_ws(&a.summary)))
        .filter(|(id, addr)| !id.is_empty() && !addr.is_empty())
        .collect())
}

#[derive(Deserialize)]
struct PropertyResponse {
    #[serde(rename = "providerSpecificFields")]
    provider_specific_fields: Option<ProviderFields>,
}

#[derive(Deserialize)]
struct ProviderFields {
    #[serde(rename = "attributes_wasteContainersAssignableWasteContainers")]
    containers: Option<String>,
}

fn parse_bin_ids(body: &str) -> Result<Vec<String>> {
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }
    let data: PropertyResponse =
        serde_json::from_str(body).context("parsing Hackney property record")?;
    Ok(data
        .provider_specific_fields
        .and_then(|f| f.containers)
        .map(|c| {
            c.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

#[derive(Deserialize)]
struct BinResponse {
    #[serde(rename = "subTitle")]
    sub_title: Option<String>,
    collection: Option<String>,
    #[serde(default)]
    disabled: bool,
}

/// The display label for a container, or `None` if it shouldn't be reported.
///
/// A container is skipped when it is `disabled` (the council's own front end
/// shows no dates for those) or when its collection isn't `"Live"`.
fn parse_service(body: &str) -> Result<Option<String>> {
    if body.trim().is_empty() {
        return Ok(None);
    }
    let data: BinResponse = serde_json::from_str(body).context("parsing Hackney bin record")?;
    if data.disabled || data.collection.as_deref() != Some("Live") {
        return Ok(None);
    }
    let name = service_name(data.sub_title.as_deref().unwrap_or_default());
    Ok((!name.is_empty()).then_some(name))
}

/// Tidy a container label for printing. `ES_` marks an estate-services
/// container in Hackney's internal data and means nothing to a resident, so it
/// goes; the rest is the council's own wording and is left alone.
fn service_name(sub_title: &str) -> String {
    collapse_ws(&sub_title.replace("ES_", ""))
}

#[derive(Deserialize)]
struct CollectionResponse {
    #[serde(default, rename = "scheduleCodeWorkflowIDs")]
    workflow_ids: Vec<String>,
}

/// The schedule workflows for a bin. A property with no individual schedule
/// answers `204 No Content`, which arrives here as an empty body.
fn parse_workflow_ids(body: &str) -> Result<Vec<String>> {
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }
    let data: CollectionResponse =
        serde_json::from_str(body).context("parsing Hackney collection schedule")?;
    Ok(data.workflow_ids)
}

#[derive(Deserialize)]
struct WorkflowResponse {
    trigger: Option<Trigger>,
}

#[derive(Deserialize)]
struct Trigger {
    #[serde(default)]
    dates: Vec<String>,
}

/// Collection dates from a workflow. These are RFC 3339 timestamps at an
/// arbitrary small hour ("2026-08-13T01:20:00Z") standing for the whole day, so
/// only the date part is meaningful.
fn parse_dates(body: &str) -> Result<Vec<NaiveDate>> {
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }
    let data: WorkflowResponse =
        serde_json::from_str(body).context("parsing Hackney collection workflow")?;
    Ok(data
        .trigger
        .map(|t| {
            t.dates
                .iter()
                .filter_map(|d| d.get(..10))
                .filter_map(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                .collect()
        })
        .unwrap_or_default())
}

// --- HTTP ------------------------------------------------------------------

/// GET a URL through the cache, keyed per day: schedules are snapshots of what
/// is next, so yesterday's copy must not answer today's question.
async fn cached_get(
    cache: Option<&Cache>,
    kind: &str,
    key: &str,
    today: NaiveDate,
    url: String,
) -> Result<String> {
    let key = format!("{key}:{}", today.format("%Y-%m-%d"));
    cache::cached(cache, kind, &key, Some(today), false, || async move {
        get(&url).await
    })
    .await
}

async fn get(url: &str) -> Result<String> {
    log::debug!("GET {url}");
    read(reqwest::Client::new().get(url), url).await
}

async fn post(url: &str, body: String) -> Result<String> {
    log::debug!("POST {url}");
    read(
        reqwest::Client::new()
            .post(url)
            .header("Content-Type", "application/json")
            .body(body),
        url,
    )
    .await
}

/// Send a request and read its body. `204 No Content` is a normal answer here
/// (a property with no schedule of its own), and arrives as an empty string.
async fn read(req: reqwest::RequestBuilder, url: &str) -> Result<String> {
    req.send()
        .await
        .with_context(|| format!("requesting `{url}`"))?
        .error_for_status()
        .with_context(|| format!("request to `{url}` failed"))?
        .text()
        .await
        .context("reading response body")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    /// Trimmed from a real `POST /property/opensearch` response.
    const ADDRESSES: &str = r#"{"addressSummaries":[
        {"summary":"Flat 1     Appleton Court      Marcon Place  E8 1ND",
         "postcode":"E8 1ND","systemId":"5f898d4790478c0067fa8b13","uprn":"100023014489"},
        {"summary":"Flat 2     Appleton Court      Marcon Place  E8 1ND",
         "postcode":"E8 1ND","systemId":"5f898d4790478c0067fa8b1e","uprn":"100023014500"},
        {"summary":"Flat 11    Appleton Court      Marcon Place  E8 1ND",
         "postcode":"E8 1ND","systemId":"5f898d4790478c0067fa8b29","uprn":"100023014511"}]}"#;

    const PROPERTY: &str = r#"{"uprn":"100023014489","providerSpecificFields":{
        "attributes_wasteContainersAssignableWasteContainers":
        "62f23930b687430167550135,62f23930b687430167550743,62f23931b687430167550bb9"}}"#;

    #[test]
    fn reads_and_tidies_padded_address_summaries() {
        let got = parse_addresses(ADDRESSES).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].0, "5f898d4790478c0067fa8b13");
        assert_eq!(got[0].1, "Flat 1 Appleton Court Marcon Place E8 1ND");
    }

    #[test]
    fn matches_a_flat_without_relying_on_commas() {
        // Hackney summaries have no commas at all, so the shared matcher has to
        // work off a prefix rather than a comma-delimited segment.
        let options = parse_addresses(ADDRESSES).unwrap();
        let (id, _) = match_address(&options, "Flat 2 Appleton Court, London, E8 1ND").unwrap();
        assert_eq!(id, "5f898d4790478c0067fa8b1e");
    }

    #[test]
    fn does_not_confuse_flat_1_with_flat_11() {
        let options = parse_addresses(ADDRESSES).unwrap();
        assert_eq!(
            match_address(&options, "Flat 1 Appleton Court").unwrap().0,
            "5f898d4790478c0067fa8b13"
        );
        assert_eq!(
            match_address(&options, "Flat 11 Appleton Court").unwrap().0,
            "5f898d4790478c0067fa8b29"
        );
    }

    #[test]
    fn splits_the_container_list() {
        let got = parse_bin_ids(PROPERTY).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], "62f23930b687430167550135");
    }

    #[test]
    fn a_property_with_no_containers_is_not_an_error() {
        assert!(
            parse_bin_ids(r#"{"providerSpecificFields":{}}"#)
                .unwrap()
                .is_empty()
        );
        assert!(parse_bin_ids("").unwrap().is_empty());
    }

    #[test]
    fn names_live_containers_and_skips_the_rest() {
        let live = r#"{"subTitle":"1 x ES_Food 240 litres ","collection":"Live","disabled":false}"#;
        assert_eq!(
            parse_service(live).unwrap().as_deref(),
            Some("1 x Food 240 litres")
        );

        let kerbside = r#"{"subTitle":"Recycling Sack ","collection":"Live","disabled":false}"#;
        assert_eq!(
            parse_service(kerbside).unwrap().as_deref(),
            Some("Recycling Sack")
        );

        let disabled =
            r#"{"subTitle":"2 x ES_Refuse Chamberlain ","collection":"Live","disabled":true}"#;
        assert_eq!(parse_service(disabled).unwrap(), None);

        let dormant =
            r#"{"subTitle":"Wheeled Bin (180ltr) ","collection":"Ceased","disabled":false}"#;
        assert_eq!(parse_service(dormant).unwrap(), None);
    }

    #[test]
    fn a_204_is_read_as_no_schedule_rather_than_a_failure() {
        assert!(parse_workflow_ids("").unwrap().is_empty());
        assert!(parse_dates("").unwrap().is_empty());
        assert_eq!(parse_service("").unwrap(), None);
    }

    #[test]
    fn reads_workflow_ids() {
        let body = r#"{"scheduleCodeWorkflowIDs":["workflows_testWorkflowRoundFri_5f91baf1e27d9800678b30d6"]}"#;
        assert_eq!(parse_workflow_ids(body).unwrap().len(), 1);
    }

    #[test]
    fn takes_the_day_from_each_timestamp() {
        let body = r#"{"name":"Workflow_Round Fri","trigger":{"discriminator":"schedule",
            "dates":["2024-12-06T02:20:00Z","2026-08-07T01:20:00Z","2026-08-14T01:20:00Z"]}}"#;
        let got = parse_dates(body).unwrap();
        assert_eq!(
            got,
            vec![date("2024-12-06"), date("2026-08-07"), date("2026-08-14")]
        );
        // The schedule reaches back years, so only future dates are of interest.
        let next = got.into_iter().filter(|d| *d >= date("2026-08-08")).min();
        assert_eq!(next, Some(date("2026-08-14")));
    }

    #[test]
    fn a_workflow_without_a_trigger_yields_no_dates() {
        assert!(
            parse_dates(r#"{"name":"Workflow_Round Fri"}"#)
                .unwrap()
                .is_empty()
        );
    }
}
