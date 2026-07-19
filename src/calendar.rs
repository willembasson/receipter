//! Reads a Google Calendar "secret iCal" feed and extracts today's meetings.
//!
//! Google exposes each calendar as a private `.ics` URL (Calendar settings ->
//! "Secret address in iCal format"). We fetch it over HTTPS, parse the events,
//! expand recurring meetings, and keep the ones that occur today in the local
//! timezone. No OAuth or API client is required.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use ical::parser::ical::component::IcalEvent;
use ical::property::Property;
use rrule::RRuleSet;
use serde::Deserialize;
use std::io::BufReader;

/// One calendar to read from, as configured in the settings file.
#[derive(Debug, Clone, Deserialize)]
pub struct CalendarSource {
    /// Optional label shown next to meetings from this calendar.
    #[serde(default)]
    pub name: Option<String>,
    /// Google Calendar "secret iCal" URL.
    pub ics_url: String,
}

impl CalendarSource {
    fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.ics_url)
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
/// start time. A failure reading one calendar is logged and skipped so the
/// others (and the weather) still print.
pub fn meetings_on(sources: &[CalendarSource], date: NaiveDate) -> Vec<Meeting> {
    let mut meetings = Vec::new();

    for source in sources {
        match read_source(source, date) {
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

fn read_source(source: &CalendarSource, date: NaiveDate) -> Result<Vec<Meeting>> {
    let ics = fetch_ics(&source.ics_url)?;
    parse_meetings(&ics, date, source.name.as_deref())
}

fn fetch_ics(url: &str) -> Result<String> {
    let body = ureq::get(url)
        .header("User-Agent", "receipter-calendar/1.0")
        .call()
        .with_context(|| format!("requesting calendar from `{url}`"))?
        .body_mut()
        .read_to_string()
        .context("reading calendar response body")?;
    Ok(body)
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
}
