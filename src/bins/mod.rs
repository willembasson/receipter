//! Household bin collections: which bins go out, and when.
//!
//! The section is deliberately quiet. It only appears on the eve of a
//! collection (`lead_days`, 1 by default) — the receipt you read the night
//! before, or the morning of the day before, is the one that tells you to put
//! the bins out. On every other day nothing is printed at all.
//!
//! Councils publish this data in wildly different ways, so each one lives in
//! its own submodule behind [`Collection`]: a council implementation's only job
//! is to turn the configured address into `(service name, next collection
//! date)` pairs. Everything downstream — the day-before gate, formatting,
//! truncation, icons — is shared here.
//!
//! Only Bromley and Hackney are implemented; see [`bromley`], [`hackney`] and
//! the README.

mod bromley;
mod hackney;

use anyhow::{Result, anyhow};
use chrono::{Datelike, Days, NaiveDate};
use serde::Deserialize;

use crate::cache::Cache;

/// Nerd Font glyph (Octicons trash) shown before the collection date in image
/// output. It lives in the Private Use Area, so the renderer detects and
/// enlarges it; text output omits it entirely.
pub const BIN_ICON: char = '\u{f48e}';

/// `[bins]` settings block.
#[derive(Debug, Clone, Deserialize)]
pub struct BinSettings {
    /// Which council's collection service to query, e.g. `"Bromley"` or
    /// `"Hackney"`.
    pub council: String,
    /// Postcode to look the property up by. When unset, it is parsed from the
    /// receipt address.
    #[serde(default)]
    pub postcode: Option<String>,
    /// The property to match within the postcode, e.g. `"11 Example Street"`.
    /// When unset, the receipt address is used.
    #[serde(default)]
    pub property: Option<String>,
    /// The council's own property id, skipping the address lookup entirely.
    /// Use this when the address match is ambiguous (`--list-bins` prints it).
    #[serde(default)]
    pub property_id: Option<String>,
    /// How many days ahead of a collection to print the section. `1` (the
    /// default) means "the day before"; `0` would mean the day itself.
    #[serde(default = "default_lead_days")]
    pub lead_days: u64,
}

fn default_lead_days() -> u64 {
    1
}

/// One service's next collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    /// The council's name for the service, e.g. `"Food Waste"`.
    pub service: String,
    /// The date it is next collected.
    pub date: NaiveDate,
}

/// A matched property and everything the council collects from it. The `id` is
/// carried through so `--list-bins` can report it, which is what you paste into
/// `property_id` when an address match needs pinning down.
#[derive(Debug, Clone)]
pub struct Property {
    /// The council's own identifier for the property.
    pub id: String,
    /// The address as the council spells it.
    pub address: String,
    /// Upcoming collections. Councils that publish a full rolling schedule
    /// (rather than just "next") may list several dates per service.
    pub collections: Vec<Collection>,
}

/// How the section should be laid out for the chosen output mode.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    /// Printable columns at the configured print width.
    pub columns: usize,
    /// Whether the output can render icon glyphs (image modes with a Nerd Font).
    pub icons: bool,
}

/// Build the "Bin day" section: the collection date and the services going out.
///
/// Normally this only fires when a collection falls exactly `lead_days` after
/// `target_date`, returning `Ok(None)` on every other day — which is most of
/// them, and is why the caller must omit the heading too rather than print an
/// empty section. `force` (the `--bins` flag) instead shows the next collection
/// from `target_date` onwards, so the section can be checked on any day.
///
/// The returned date is the collection's, not the receipt's, so the caller can
/// title it accurately ("tomorrow" vs "in 5 days") in either mode.
///
/// `today` is the date the data was fetched for; it anchors the year-less dates
/// councils tend to publish.
pub async fn bin_day(
    cfg: &BinSettings,
    address: &str,
    target_date: NaiveDate,
    today: NaiveDate,
    cache: Option<&Cache>,
    layout: Layout,
    force: bool,
) -> Result<Option<(NaiveDate, String)>> {
    let due = target_date + Days::new(cfg.lead_days);
    let property = collections(cfg, address, today, cache).await?;

    let Some(show) = section_date(&property.collections, target_date, due, force) else {
        log::debug!(
            "no bin collection on {} (next: {:?})",
            due.format("%Y-%m-%d"),
            property.collections.iter().map(|c| c.date).min()
        );
        return Ok(None);
    };
    Ok(render(&property.collections, show, layout).map(|body| (show, body)))
}

/// Which collection date, if any, the section should be about: the `due` date
/// when something is actually collected then, or — under `force` — the next
/// collection at or after `target`. Nothing before `target` is ever shown; a
/// collection that has already happened is not news.
fn section_date(
    collections: &[Collection],
    target: NaiveDate,
    due: NaiveDate,
    force: bool,
) -> Option<NaiveDate> {
    if collections.iter().any(|c| c.date == due) {
        return Some(due);
    }
    if force {
        return collections
            .iter()
            .map(|c| c.date)
            .filter(|d| *d >= target)
            .min();
    }
    None
}

/// The section body for collections falling on `due`, or `None` when none do.
/// Split out from [`bin_day`] so the gate and layout can be tested directly.
fn render(collections: &[Collection], due: NaiveDate, layout: Layout) -> Option<String> {
    let Layout { columns, icons } = layout;
    let mut going_out: Vec<&Collection> = collections.iter().filter(|c| c.date == due).collect();
    if going_out.is_empty() {
        return None;
    }
    going_out.sort_by(|a, b| a.service.cmp(&b.service));
    // A property can hold two identical containers; naming the service twice on
    // the receipt would just look like a bug.
    going_out.dedup_by(|a, b| a.service == b.service);

    let mut out = String::new();
    let date = due.format("%A, %-d %B").to_string();
    if icons {
        // The icon is drawn enlarged (~1.5 columns) plus two spaces, so reserve
        // four columns of prefix when budgeting the date.
        out.push_str(&format!(
            "{BIN_ICON}  {}\n\n",
            truncate(&date, columns.saturating_sub(4))
        ));
    } else {
        out.push_str(&format!("{}\n\n", truncate(&date, columns)));
    }
    for c in going_out {
        out.push_str(&format!(
            "  {}\n",
            truncate(&c.service, columns.saturating_sub(2))
        ));
    }
    Some(out)
}

/// Utility mode: every service this property has, with its next collection
/// date, plus the council property id — enough to fill in `property_id` when
/// the address match needs pinning down.
pub async fn list_bins(
    cfg: &BinSettings,
    address: &str,
    today: NaiveDate,
    cache: Option<&Cache>,
) -> Result<String> {
    let property = collections(cfg, address, today, cache).await?;

    // Councils that publish a full schedule return many dates per service; for
    // a setup listing only the next one per service is useful.
    let next = next_per_service(&property.collections);

    let mut out = format!("{} (property_id = {})\n\n", property.address, property.id);
    if next.is_empty() {
        out.push_str("No scheduled collections found for this property.\n");
        return Ok(out);
    }
    for c in next {
        out.push_str(&format!(
            "{}  {}  {}\n",
            c.date.format("%Y-%m-%d"),
            c.date.format("%a"),
            c.service
        ));
    }
    Ok(out)
}

/// The soonest collection for each distinct service, earliest first.
fn next_per_service(collections: &[Collection]) -> Vec<&Collection> {
    let mut next: Vec<&Collection> = Vec::new();
    for c in collections {
        match next.iter_mut().find(|n| n.service == c.service) {
            Some(existing) if c.date < existing.date => *existing = c,
            Some(_) => {}
            None => next.push(c),
        }
    }
    next.sort_by(|a, b| (a.date, &a.service).cmp(&(b.date, &b.service)));
    next
}

/// Dispatch to the configured council. Council names are matched
/// case-insensitively and ignoring any "Council"/"London Borough of" wrapping,
/// so `"Bromley"`, `"bromley"` and `"Bromley Council"` all work.
async fn collections(
    cfg: &BinSettings,
    address: &str,
    today: NaiveDate,
    cache: Option<&Cache>,
) -> Result<Property> {
    match council_key(&cfg.council).as_str() {
        "bromley" => bromley::collections(cfg, address, today, cache).await,
        "hackney" => hackney::collections(cfg, address, today, cache).await,
        _ => Err(anyhow!(
            "unsupported council `{}` in [bins]; only `Bromley` and `Hackney` are implemented so far",
            cfg.council
        )),
    }
}

/// Normalise a configured council name to a lookup key.
fn council_key(council: &str) -> String {
    let lower = council.to_lowercase();
    let stripped = lower
        .trim()
        .trim_start_matches("london borough of")
        .trim_end_matches("council")
        .trim_end_matches("borough of")
        .trim();
    stripped
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// --- Shared helpers for council scrapers -----------------------------------
//
// These live here rather than in a council module because every council that
// publishes HTML needs the same three things: entity decoding, tag stripping,
// and a date parser that copes with the year-less, ordinal-suffixed dates
// ("Thursday, 13th August") that council sites are so fond of.

/// Parse a council-style date such as `"Thursday, 13th August"`.
///
/// Two things make this awkward: the ordinal suffix (`st`/`nd`/`rd`/`th`),
/// which `chrono` cannot parse, and the missing year. The year is inferred by
/// trying the years around `today` and keeping the first candidate that both
/// parses *and* is not in the past — which also validates the result, because
/// `chrono` rejects a `%A` weekday that disagrees with the date, so a wrong
/// year is caught rather than silently accepted.
fn parse_collection_date(text: &str, today: NaiveDate) -> Option<NaiveDate> {
    let cleaned = strip_ordinals(text);
    // Some entries carry a time ("Thursday, 6th August, at 8:50am") or a note;
    // keep only the leading "Weekday, D Month".
    let cleaned = cleaned
        .split(", at ")
        .next()
        .unwrap_or(&cleaned)
        .trim()
        .trim_end_matches(',')
        .to_string();

    // A collection listed as "next" is upcoming, so prefer this year, then next
    // year (a December receipt pointing at a January collection).
    for year in [today.year(), today.year() + 1, today.year() - 1] {
        let with_year = format!("{cleaned} {year}");
        if let Ok(d) = NaiveDate::parse_from_str(&with_year, "%A, %d %B %Y")
            && d >= today - Days::new(7)
        {
            return Some(d);
        }
    }
    None
}

/// Remove the ordinal suffix from any day number: `13th August` -> `13 August`.
fn strip_ordinals(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            while i < chars.len() && chars[i].is_ascii_digit() {
                out.push(chars[i]);
                i += 1;
            }
            // A suffix only counts when it is the whole of the next word.
            let suffix: String = chars[i..].iter().take(2).collect::<String>().to_lowercase();
            let ends_word = chars.get(i + 2).is_none_or(|c| !c.is_alphanumeric());
            if ends_word && matches!(suffix.as_str(), "st" | "nd" | "rd" | "th") {
                i += 2;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Drop any HTML tags and collapse the remaining whitespace to single spaces.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut depth = 0usize;
    for c in html.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    collapse_ws(&decode_entities(&out))
}

/// Decode the handful of HTML entities that actually turn up in service names
/// ("Cans, Plastics &amp; Glass"), plus numeric character references.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The slice of `hay` after the first `start` and before the next `end`.
fn between<'a>(hay: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = hay.find(start)? + start.len();
    let rest = &hay[from..];
    let to = rest.find(end)?;
    Some(&rest[..to])
}

/// Reduce an address to a comparable form: lowercase, alphanumerics and single
/// spaces only, so "11 Lankton Close." and "11  Lankton Close" agree.
fn normalise_address(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The part of an address before the first comma — the house and street.
fn head(address: &str) -> &str {
    address.split(',').next().unwrap_or(address).trim()
}

/// Pick the configured property out of a council's `(id, address)` list.
///
/// The full address is tried first, then the house-and-street part alone (the
/// text before the first comma), which is what lets a receipt address of
/// "11 Example Street, Townsville, AB1 2CD" match the council's own rendering
/// of the same place. The second pass is a *prefix* match on the normalised
/// text, because councils vary in what they append — Bromley writes
/// "11 Example Street, Townsville, AB1 2CD" while Hackney writes
/// "11 Example Street  E8 1ND" with no commas at all.
///
/// Normalisation collapses runs of whitespace to a single space, so the prefix
/// can only end on a word boundary: "1 Example Street" never matches
/// "11 Example Street", because the candidate reads "11 e…" where the prefix
/// wants "1 e…".
fn match_address(options: &[(String, String)], wanted: &str) -> Result<(String, String)> {
    let want_full = normalise_address(wanted);
    if let Some((id, addr)) = options
        .iter()
        .find(|(_, a)| normalise_address(a) == want_full)
    {
        return Ok((id.clone(), addr.clone()));
    }

    let want_head = normalise_address(head(wanted));
    let hits: Vec<&(String, String)> = options
        .iter()
        .filter(|(_, a)| {
            let a = normalise_address(a);
            a == want_head || a.starts_with(&format!("{want_head} "))
        })
        .collect();
    match hits.as_slice() {
        [(id, addr)] => Ok((id.clone(), addr.clone())),
        [] => Err(anyhow!(
            "no address at this postcode matches `{wanted}` (tried `{want_head}`); \
             run `--list-bins` after setting `property`, or set `property_id` directly. \
             Candidates include: {}",
            options
                .iter()
                .take(3)
                .map(|(_, a)| a.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )),
        many => Err(anyhow!(
            "`{wanted}` matches {} addresses at this postcode; \
             set `property` more precisely or `property_id` to one of: {}",
            many.len(),
            many.iter()
                .take(5)
                .map(|(id, a)| format!("{a} (id {id})"))
                .collect::<Vec<_>>()
                .join("; ")
        )),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn collection(service: &str, on: &str) -> Collection {
        Collection {
            service: service.into(),
            date: date(on),
        }
    }

    /// Two services on the 13th, one on the 18th.
    fn sample() -> Vec<Collection> {
        vec![
            collection("Mixed Recycling", "2026-08-13"),
            collection("Food Waste", "2026-08-13"),
            collection("Garden Waste", "2026-08-18"),
        ]
    }

    /// Text-mode layout at a typical 80mm width.
    fn text(columns: usize) -> Layout {
        Layout {
            columns,
            icons: false,
        }
    }

    #[test]
    fn collapses_a_full_schedule_to_the_next_of_each_service() {
        // A council that publishes a rolling schedule returns many dates per
        // service; `--list-bins` should show only what's next for each.
        let all = vec![
            collection("Recycling", "2026-08-21"),
            collection("Food", "2026-08-13"),
            collection("Recycling", "2026-08-07"),
            collection("Food", "2026-08-20"),
            collection("Recycling", "2026-08-14"),
        ];
        let got: Vec<(&str, String)> = next_per_service(&all)
            .iter()
            .map(|c| (c.service.as_str(), c.date.to_string()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("Recycling", "2026-08-07".to_string()),
                ("Food", "2026-08-13".to_string())
            ]
        );
    }

    #[test]
    fn identical_containers_are_named_once() {
        let twins = vec![
            collection("Wheeled Bin (180ltr)", "2026-08-13"),
            collection("Wheeled Bin (180ltr)", "2026-08-13"),
        ];
        let out = render(&twins, date("2026-08-13"), text(40)).unwrap();
        assert_eq!(out.matches("Wheeled Bin").count(), 1, "{out:?}");
    }

    #[test]
    fn renders_only_the_bins_going_out_that_day() {
        let out = render(&sample(), date("2026-08-13"), text(40)).unwrap();
        assert_eq!(
            out,
            "Thursday, 13 August\n\n  Food Waste\n  Mixed Recycling\n"
        );
    }

    #[test]
    fn stays_silent_when_nothing_is_collected() {
        // The day before the 13th is when the section shows; every other target
        // date must produce nothing at all, heading included.
        for day in ["2026-08-12", "2026-08-14", "2026-08-17"] {
            assert!(
                render(&sample(), date(day), text(40)).is_none(),
                "expected no section for {day}"
            );
        }
        assert!(render(&[], date("2026-08-13"), text(40)).is_none());
    }

    #[test]
    fn the_day_before_gate_follows_lead_days() {
        // lead_days = 1 on the 12th targets the 13th, which has collections.
        let due = date("2026-08-12") + Days::new(1);
        assert!(render(&sample(), due, text(40)).is_some());
        // The same date with no notice would look at the 12th, which has none.
        let due = date("2026-08-12") + Days::new(0);
        assert!(render(&sample(), due, text(40)).is_none());
    }

    #[test]
    fn without_force_only_the_due_date_is_shown() {
        let target = date("2026-08-12");
        let due = target + Days::new(1);
        assert_eq!(
            section_date(&sample(), target, due, false),
            Some(date("2026-08-13"))
        );
        // Two days out: nothing is due tomorrow, so nothing is shown.
        let target = date("2026-08-11");
        assert_eq!(
            section_date(&sample(), target, target + Days::new(1), false),
            None
        );
    }

    #[test]
    fn force_falls_back_to_the_next_collection() {
        // Nothing is due tomorrow, but --bins should still show what's next.
        let target = date("2026-08-11");
        assert_eq!(
            section_date(&sample(), target, target + Days::new(1), true),
            Some(date("2026-08-13"))
        );
        // Once the 13th has passed, the next one is the 18th.
        let target = date("2026-08-14");
        assert_eq!(
            section_date(&sample(), target, target + Days::new(1), true),
            Some(date("2026-08-18"))
        );
    }

    #[test]
    fn force_never_shows_a_collection_that_has_been_and_gone() {
        let target = date("2026-08-19"); // past both sample collections
        assert_eq!(
            section_date(&sample(), target, target + Days::new(1), true),
            None
        );
        assert_eq!(section_date(&[], target, target, true), None);
    }

    #[test]
    fn force_still_prefers_the_due_date_when_there_is_one() {
        // On the eve of the 13th, --bins must not skip ahead to the 18th.
        let target = date("2026-08-12");
        assert_eq!(
            section_date(&sample(), target, target + Days::new(1), true),
            Some(date("2026-08-13"))
        );
    }

    #[test]
    fn image_output_gets_an_icon_and_text_output_does_not() {
        let with = render(
            &sample(),
            date("2026-08-13"),
            Layout {
                columns: 40,
                icons: true,
            },
        )
        .unwrap();
        let without = render(&sample(), date("2026-08-13"), text(40)).unwrap();
        assert!(with.starts_with(BIN_ICON));
        assert!(!without.contains(BIN_ICON));
    }

    #[test]
    fn long_service_names_are_shortened_to_the_print_width() {
        let long = vec![collection(
            "Mixed Recycling (Cans, Plastics & Glass) and everything else",
            "2026-08-13",
        )];
        let out = render(&long, date("2026-08-13"), text(20)).unwrap();
        assert!(
            out.lines().all(|l| l.chars().count() <= 20),
            "lines overflowed: {out:?}"
        );
        assert!(out.contains('\u{2026}'));
    }

    #[test]
    fn council_names_normalise_to_a_key() {
        assert_eq!(council_key("Bromley"), "bromley");
        assert_eq!(council_key("bromley"), "bromley");
        assert_eq!(council_key("Bromley Council"), "bromley");
        assert_eq!(council_key("London Borough of Bromley"), "bromley");
        assert_eq!(council_key("Hackney"), "hackney");
        assert_eq!(council_key("London Borough of Hackney"), "hackney");
    }

    #[test]
    fn parses_year_less_ordinal_dates() {
        let today = date("2026-08-07"); // a Friday
        assert_eq!(
            parse_collection_date("Thursday, 13th August", today),
            Some(date("2026-08-13"))
        );
        assert_eq!(
            parse_collection_date("Tuesday, 18th August", today),
            Some(date("2026-08-18"))
        );
    }

    #[test]
    fn rolls_the_year_over_at_christmas() {
        let today = date("2026-12-30");
        // 4 January is a Monday in 2027, but a Sunday in 2026: only the
        // next-year reading has a weekday that agrees.
        assert_eq!(
            parse_collection_date("Monday, 4th January", today),
            Some(date("2027-01-04"))
        );
    }

    #[test]
    fn rejects_dates_whose_weekday_disagrees() {
        // 13 August 2026 is a Thursday, so "Monday" cannot be right.
        assert_eq!(
            parse_collection_date("Monday, 13th August", date("2026-08-07")),
            None
        );
        assert_eq!(
            parse_collection_date("Loading your bin days...", date("2026-08-07")),
            None
        );
    }

    #[test]
    fn ignores_a_trailing_collection_time() {
        assert_eq!(
            parse_collection_date("Thursday, 6th August, at  8:50am", date("2026-08-07")),
            Some(date("2026-08-06"))
        );
    }

    #[test]
    fn strips_ordinals_without_eating_other_digits() {
        assert_eq!(strip_ordinals("13th August"), "13 August");
        assert_eq!(strip_ordinals("1st, 2nd, 3rd"), "1, 2, 3");
        // "21st Street" is a road name, but the suffix is still its own word.
        assert_eq!(strip_ordinals("at 8:50am"), "at 8:50am");
    }

    #[test]
    fn decodes_entities_in_service_names() {
        assert_eq!(
            strip_tags("<h3>Mixed Recycling (Cans, Plastics &amp; Glass)</h3>"),
            "Mixed Recycling (Cans, Plastics & Glass)"
        );
        assert_eq!(decode_entities("caf&#233; &amp; bar"), "café & bar");
        // A bare ampersand is left alone rather than swallowing the rest.
        assert_eq!(decode_entities("Tom & Jerry"), "Tom & Jerry");
    }

    #[test]
    fn normalises_addresses_for_comparison() {
        assert_eq!(normalise_address("11  Lankton Close."), "11 lankton close");
        assert_ne!(
            normalise_address("1 Lankton Close"),
            normalise_address("11 Lankton Close")
        );
    }

    #[tokio::test]
    async fn unsupported_councils_are_a_clear_error() {
        let cfg = BinSettings {
            council: "Atlantis".into(),
            postcode: None,
            property: None,
            property_id: None,
            lead_days: 1,
        };
        let err = collections(&cfg, "1 Example Street, AB1 2CD", date("2026-08-07"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Atlantis"), "{err}");
        assert!(err.contains("Bromley") && err.contains("Hackney"), "{err}");
    }
}
