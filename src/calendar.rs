//! Reads calendars and extracts a given day's meetings.
//!
//! Two source types are supported:
//!   * `ics` — a Google Calendar "secret iCal" feed (Calendar settings ->
//!     "Secret address in iCal format"), fetched over HTTPS and parsed locally.
//!   * `service_account` — the Google Calendar REST API authenticated with a
//!     Google Cloud service account (share the calendar read-only with the
//!     service account's email). Recurring events are expanded server-side.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use ical::parser::ical::component::IcalEvent;
use ical::property::Property;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rrule::RRuleSet;
use serde::{Deserialize, Serialize};
use std::io::BufReader;
use std::path::PathBuf;

use crate::cache::{self, Cache};

/// One calendar to read from, as configured in the settings file. The TOML
/// `type` field selects the variant (`type = "ics"` or
/// `type = "service_account"`).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CalendarSource {
    /// A Google Calendar "secret iCal" feed.
    Ics(IcsSource),
    /// The Google Calendar REST API via a service account.
    ServiceAccount(ServiceAccountSource),
}

/// A secret-iCal calendar source.
#[derive(Debug, Clone, Deserialize)]
pub struct IcsSource {
    /// Optional label shown next to meetings from this calendar.
    #[serde(default)]
    pub name: Option<String>,
    /// Google Calendar "secret iCal" URL.
    pub ics_url: String,
}

/// A Google Calendar API source authenticated with a service account.
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceAccountSource {
    /// Optional label shown next to meetings from this calendar.
    #[serde(default)]
    pub name: Option<String>,
    /// Calendar to read, e.g. `you@gmail.com` or `primary`.
    pub calendar_id: String,
    /// Path to the Google service-account key JSON file.
    pub key_file: PathBuf,
}

impl CalendarSource {
    fn label(&self) -> &str {
        match self {
            CalendarSource::Ics(s) => s.name.as_deref().unwrap_or(&s.ics_url),
            CalendarSource::ServiceAccount(s) => s.name.as_deref().unwrap_or(&s.calendar_id),
        }
    }
}

/// A single meeting occurring today.
#[derive(Debug, Clone)]
pub struct Meeting {
    pub start: DateTime<Local>,
    pub all_day: bool,
    pub summary: String,
    /// Name of the calendar this meeting came from, if the calendar was named.
    pub calendar: Option<String>,
}

impl Meeting {
    /// A one-line label for the receipt, e.g. `09:30    Standup  (Work)`.
    pub fn line(&self) -> String {
        let time = if self.all_day {
            "all day".to_string()
        } else {
            format!("{:02}:{:02}", self.start.hour(), self.start.minute())
        };
        match &self.calendar {
            // Pad the time column so summaries line up regardless of label width.
            Some(name) => format!("{time:<7}  {}  ({name})", self.summary),
            None => format!("{time:<7}  {}", self.summary),
        }
    }
}

/// Read every configured calendar and return the meetings on `date`, sorted by
/// start time. Each source is fetched through `cache` (keyed per calendar and
/// date), so repeat runs, past dates, and offline/rate-limited fetches are
/// served from the cache. A failure reading one calendar is logged and skipped
/// so the others (and the weather) still print.
pub async fn meetings_on(
    sources: &[CalendarSource],
    date: NaiveDate,
    cache: Option<&Cache>,
) -> Vec<Meeting> {
    let mut meetings = Vec::new();

    for source in sources {
        match read_source(source, date, cache).await {
            Ok(mut found) => meetings.append(&mut found),
            Err(e) => eprintln!(
                "warning: could not load calendar `{}`: {e:#}",
                source.label()
            ),
        }
    }

    meetings.sort_by_key(|m| m.start);
    meetings
}

async fn read_source(
    source: &CalendarSource,
    date: NaiveDate,
    cache: Option<&Cache>,
) -> Result<Vec<Meeting>> {
    match source {
        CalendarSource::Ics(s) => {
            // Key by a hash of the secret iCal URL (so the token is never written
            // to the cache DB) plus the date, mirroring the transport per-date
            // history model.
            let key = format!("{}|{}", url_hash(&s.ics_url), date.format("%Y%m%d"));
            let ics = cache::cached(cache, "calendar", &key, Some(date), false, || {
                fetch_ics(&s.ics_url)
            })
            .await?;
            parse_meetings(&ics, date, s.name.as_deref())
        }
        CalendarSource::ServiceAccount(s) => {
            // The calendar id is not a secret, so it can key the cache directly.
            let key = format!("gcal|{}|{}", s.calendar_id, date.format("%Y%m%d"));
            let json = cache::cached(cache, "calendar", &key, Some(date), false, || {
                fetch_google_events(s, date)
            })
            .await?;
            parse_google_events(&json, s.name.as_deref())
        }
    }
}

/// Stable, non-reversible identifier for a secret iCal URL, used as a cache key
/// so the token itself is never persisted.
fn url_hash(url: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

async fn fetch_ics(url: &str) -> Result<String> {
    let body = reqwest::Client::new()
        .get(url)
        .header("User-Agent", "receipter-calendar/1.0")
        .send()
        .await
        .with_context(|| format!("requesting calendar from `{url}`"))?
        .error_for_status()
        .with_context(|| format!("calendar request to `{url}` failed"))?
        .text()
        .await
        .context("reading calendar response body")?;
    Ok(body)
}

// --- Google Calendar API (service account) ---------------------------------

/// The fields we need from a Google service-account key JSON file.
#[derive(Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    token_uri: String,
}

/// JWT claims for the service-account -> access-token exchange.
#[derive(Serialize)]
struct JwtClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

/// Google's OAuth token response (only the field we use).
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Mint a short-lived read-only access token from a service-account key by
/// signing and exchanging a JWT (the two-legged OAuth flow).
async fn service_account_token(key: &ServiceAccountKey) -> Result<String> {
    let now = chrono::Utc::now().timestamp();
    let claims = JwtClaims {
        iss: &key.client_email,
        scope: "https://www.googleapis.com/auth/calendar.readonly",
        aud: &key.token_uri,
        iat: now,
        exp: now + 3600,
    };
    let encoding = EncodingKey::from_rsa_pem(key.private_key.as_bytes())
        .context("parsing service-account private key")?;
    let jwt = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &encoding)
        .context("signing service-account JWT")?;

    let resp: TokenResponse = reqwest::Client::new()
        .post(&key.token_uri)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", jwt.as_str()),
        ])
        .send()
        .await
        .context("requesting Google access token")?
        .error_for_status()
        .context("Google token endpoint returned an error")?
        .json()
        .await
        .context("parsing Google token response")?;
    Ok(resp.access_token)
}

/// Fetch the raw `events.list` JSON for `date` from the Google Calendar API.
async fn fetch_google_events(source: &ServiceAccountSource, date: NaiveDate) -> Result<String> {
    let raw = std::fs::read_to_string(&source.key_file)
        .with_context(|| format!("reading service-account key `{}`", source.key_file.display()))?;
    let key: ServiceAccountKey =
        serde_json::from_str(&raw).context("parsing service-account key JSON")?;
    let token = service_account_token(&key).await?;

    let time_min = local_midnight(date).to_rfc3339();
    let time_max = local_midnight(date + Duration::days(1)).to_rfc3339();
    // `@` must be percent-encoded in the path; other id characters are path-safe.
    let cal = source.calendar_id.replace('@', "%40");
    let url = format!("https://www.googleapis.com/calendar/v3/calendars/{cal}/events");

    log::debug!("GET calendar/v3 events for `{}`", source.calendar_id);
    let body = reqwest::Client::new()
        .get(&url)
        .bearer_auth(&token)
        .query(&[
            ("timeMin", time_min.as_str()),
            ("timeMax", time_max.as_str()),
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
            ("maxResults", "50"),
        ])
        .send()
        .await
        .context("requesting Google Calendar events")?
        .error_for_status()
        .context("Google Calendar events request failed")?
        .text()
        .await
        .context("reading Google Calendar events body")?;
    Ok(body)
}

/// The `events.list` response shape (only the fields we use).
#[derive(Deserialize)]
struct GoogleEvents {
    #[serde(default)]
    items: Vec<GoogleEvent>,
}

#[derive(Deserialize)]
struct GoogleEvent {
    #[serde(default)]
    summary: Option<String>,
    start: Option<GoogleEventTime>,
}

#[derive(Deserialize)]
struct GoogleEventTime {
    /// RFC3339 timestamp for timed events, e.g. `2026-07-20T09:00:00+01:00`.
    #[serde(rename = "dateTime", default)]
    date_time: Option<String>,
    /// `YYYY-MM-DD` for all-day events.
    #[serde(default)]
    date: Option<String>,
}

/// Parse a Google `events.list` JSON payload into meetings. Google already
/// filtered to the requested day and expanded recurring events server-side.
fn parse_google_events(json: &str, calendar_name: Option<&str>) -> Result<Vec<Meeting>> {
    let data: GoogleEvents =
        serde_json::from_str(json).context("parsing Google Calendar events JSON")?;
    let calendar = calendar_name.map(str::to_string);

    let mut meetings = Vec::new();
    for event in data.items {
        let summary = event.summary.unwrap_or_else(|| "(no title)".to_string());
        let Some(start) = event.start else { continue };
        if let Some(dt) = start.date_time.as_deref() {
            if let Ok(parsed) = DateTime::parse_from_rfc3339(dt) {
                meetings.push(Meeting {
                    start: parsed.with_timezone(&Local),
                    all_day: false,
                    summary,
                    calendar: calendar.clone(),
                });
            }
        } else if let Some(d) = start.date.as_deref() {
            if let Ok(date) = NaiveDate::parse_from_str(d, "%Y-%m-%d") {
                meetings.push(Meeting {
                    start: local_midnight(date),
                    all_day: true,
                    summary,
                    calendar: calendar.clone(),
                });
            }
        }
    }

    meetings.sort_by_key(|m| m.start);
    Ok(meetings)
}

/// Parse an ICS document and return the meetings that fall on `date` (local time).
fn parse_meetings(ics: &str, date: NaiveDate, calendar_name: Option<&str>) -> Result<Vec<Meeting>> {
    let day_start = local_midnight(date);
    let day_end = local_midnight(date + Duration::days(1));

    let mut meetings = Vec::new();

    for calendar in ical::IcalParser::new(BufReader::new(ics.as_bytes())) {
        let calendar = calendar.context("parsing the ICS feed")?;
        for event in &calendar.events {
            collect_occurrences(event, day_start, day_end, calendar_name, &mut meetings);
        }
    }

    meetings.sort_by_key(|m| m.start);
    Ok(meetings)
}

/// Add any of `event`'s occurrences that land within `[day_start, day_end)`.
fn collect_occurrences(
    event: &IcalEvent,
    day_start: DateTime<Local>,
    day_end: DateTime<Local>,
    calendar_name: Option<&str>,
    out: &mut Vec<Meeting>,
) {
    let dtstart = match find_prop(event, "DTSTART") {
        Some(p) => p,
        None => return,
    };
    let summary = prop_value(event, "SUMMARY").unwrap_or_else(|| "(no title)".to_string());
    let all_day = is_date_only(dtstart);
    let calendar = calendar_name.map(str::to_string);

    // Recurring event: expand with the rrule crate.
    if find_prop(event, "RRULE").is_some() {
        let block = rrule_block(event);
        if let Ok(set) = block.parse::<RRuleSet>() {
            // Widen the lower bound by a second so an occurrence exactly at
            // midnight is not dropped by the exclusive `after` bound.
            let after = to_rrule_tz(day_start - Duration::seconds(1));
            let before = to_rrule_tz(day_end);
            for occ in set.after(after).before(before).all(64).dates {
                let start = occ.with_timezone(&Local);
                if start >= day_start && start < day_end {
                    out.push(Meeting {
                        start,
                        all_day,
                        summary: summary.clone(),
                        calendar: calendar.clone(),
                    });
                }
            }
        }
        return;
    }

    // Single (non-recurring) event.
    if all_day {
        if let Some(date) = dtstart.value.as_deref().and_then(parse_ics_date) {
            if date == day_start.date_naive() {
                out.push(Meeting {
                    start: local_midnight(date),
                    all_day: true,
                    summary,
                    calendar,
                });
            }
        }
    } else if let Some(start) = parse_ics_datetime(dtstart) {
        if start >= day_start && start < day_end {
            out.push(Meeting {
                start,
                all_day: false,
                summary,
                calendar,
            });
        }
    }
}

/// Reassemble the DTSTART/RRULE/RDATE/EXRULE/EXDATE lines so `RRuleSet` can
/// parse them. DTSTART must come first.
fn rrule_block(event: &IcalEvent) -> String {
    let mut lines = Vec::new();
    if let Some(p) = find_prop(event, "DTSTART") {
        lines.push(property_to_line(p));
    }
    for name in ["RRULE", "RDATE", "EXRULE", "EXDATE"] {
        for p in event
            .properties
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(name))
        {
            lines.push(property_to_line(p));
        }
    }
    lines.join("\n")
}

/// Serialize a property back to its `NAME;PARAM=v:value` iCal form.
fn property_to_line(p: &Property) -> String {
    let mut line = p.name.clone();
    if let Some(params) = &p.params {
        for (key, values) in params {
            line.push(';');
            line.push_str(key);
            line.push('=');
            line.push_str(&values.join(","));
        }
    }
    line.push(':');
    if let Some(value) = &p.value {
        line.push_str(value);
    }
    line
}

fn parse_ics_datetime(p: &Property) -> Option<DateTime<Local>> {
    let value = p.value.as_deref()?;

    if let Some(stripped) = value.strip_suffix('Z') {
        // UTC, e.g. 20240115T090000Z
        let naive = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S").ok()?;
        return Some(utc_datetime(naive).with_timezone(&Local));
    }

    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
    match param(p, "TZID").and_then(|tzid| tzid.parse::<chrono_tz::Tz>().ok()) {
        // Zoned, e.g. DTSTART;TZID=Europe/London:20240115T090000
        Some(tz) => tz
            .from_local_datetime(&naive)
            .single()
            .map(|dt| dt.with_timezone(&Local)),
        // Floating time: interpret in the local timezone.
        None => Local.from_local_datetime(&naive).single(),
    }
}

fn parse_ics_date(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y%m%d").ok()
}

fn local_midnight(date: NaiveDate) -> DateTime<Local> {
    let naive = date.and_hms_opt(0, 0, 0).expect("midnight is always valid");
    Local
        .from_local_datetime(&naive)
        .earliest()
        .expect("local midnight exists")
}

fn to_rrule_tz(dt: DateTime<Local>) -> DateTime<rrule::Tz> {
    dt.with_timezone(&rrule::Tz::LOCAL)
}

fn utc_datetime(naive: NaiveDateTime) -> DateTime<chrono::Utc> {
    chrono::Utc.from_utc_datetime(&naive)
}

fn find_prop<'a>(event: &'a IcalEvent, name: &str) -> Option<&'a Property> {
    event
        .properties
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
}

fn prop_value(event: &IcalEvent, name: &str) -> Option<String> {
    find_prop(event, name).and_then(|p| p.value.clone())
}

fn param<'a>(p: &'a Property, key: &str) -> Option<&'a str> {
    p.params
        .as_ref()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, values)| values.first())
        .map(String::as_str)
}

fn is_date_only(p: &Property) -> bool {
    param(p, "VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        // An all-day event today, a daily recurring meeting, and an all-day
        // event on a different day (which must be excluded).
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         BEGIN:VEVENT\r\n\
         SUMMARY:Holiday\r\n\
         DTSTART;VALUE=DATE:20260715\r\n\
         END:VEVENT\r\n\
         BEGIN:VEVENT\r\n\
         SUMMARY:Standup\r\n\
         DTSTART:20260101T120000Z\r\n\
         RRULE:FREQ=DAILY\r\n\
         END:VEVENT\r\n\
         BEGIN:VEVENT\r\n\
         SUMMARY:Past holiday\r\n\
         DTSTART;VALUE=DATE:20260710\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR\r\n"
            .to_string()
    }

    #[test]
    fn keeps_only_todays_events_including_recurring() {
        // Wednesday, 15 July 2026.
        let date = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();
        let meetings = parse_meetings(&fixture(), date, Some("Work")).unwrap();

        let summaries: Vec<&str> = meetings.iter().map(|m| m.summary.as_str()).collect();
        assert!(
            summaries.contains(&"Holiday"),
            "all-day event today should be kept"
        );
        assert!(
            summaries.contains(&"Standup"),
            "recurring daily meeting should occur today"
        );
        assert!(
            !summaries.contains(&"Past holiday"),
            "events on other days must be excluded"
        );
        assert_eq!(meetings.len(), 2);

        // The calendar name is carried through and shown on the line.
        assert!(meetings
            .iter()
            .all(|m| m.calendar.as_deref() == Some("Work")));
        assert!(meetings.iter().any(|m| m.line().contains("(Work)")));
    }

    #[test]
    fn parses_tagged_calendar_sources() {
        #[derive(Deserialize)]
        struct Wrapper {
            calendars: Vec<CalendarSource>,
        }

        let toml = r#"
            [[calendars]]
            type = "ics"
            name = "Work"
            ics_url = "https://example.com/basic.ics"

            [[calendars]]
            type = "service_account"
            calendar_id = "you@gmail.com"
            key_file = "key.json"
        "#;

        let w: Wrapper = toml::from_str(toml).unwrap();
        assert_eq!(w.calendars.len(), 2);
        match &w.calendars[0] {
            CalendarSource::Ics(s) => {
                assert_eq!(s.name.as_deref(), Some("Work"));
                assert_eq!(s.ics_url, "https://example.com/basic.ics");
            }
            _ => panic!("first source should be an ics feed"),
        }
        match &w.calendars[1] {
            CalendarSource::ServiceAccount(s) => {
                assert_eq!(s.name, None);
                assert_eq!(s.calendar_id, "you@gmail.com");
                assert_eq!(s.key_file, std::path::PathBuf::from("key.json"));
            }
            _ => panic!("second source should be a service account"),
        }
    }

    #[test]
    fn parses_google_events_timed_and_all_day() {
        let json = r#"{
            "items": [
                {
                    "summary": "Standup",
                    "start": { "dateTime": "2026-07-20T09:30:00+01:00" }
                },
                {
                    "summary": "Holiday",
                    "start": { "date": "2026-07-20" }
                },
                {
                    "start": { "dateTime": "2026-07-20T14:00:00+01:00" }
                }
            ]
        }"#;

        let meetings = parse_google_events(json, Some("Work")).unwrap();
        assert_eq!(meetings.len(), 3);

        // Sorted by start; the all-day event sorts to local midnight, so it
        // comes before the timed events.
        assert_eq!(meetings[0].summary, "Holiday");
        assert!(meetings[0].all_day);
        // The 09:30 timed event is kept and not marked all-day.
        assert!(meetings.iter().any(|m| m.summary == "Standup" && !m.all_day));
        // A missing summary falls back to a placeholder.
        assert!(meetings.iter().any(|m| m.summary == "(no title)"));
        assert!(meetings.iter().all(|m| m.calendar.as_deref() == Some("Work")));
    }
}
