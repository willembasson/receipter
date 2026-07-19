//! Nearest (or explicitly pinned) stations and bus stops with their next
//! departures, via TransportAPI.
//!
//! The flow is:
//!   1. Choose which stops to report: either the explicit `station_codes` /
//!      `bus_stop_codes` from the config, or the closest few found by a
//!      proximity search. The search location is explicit lat/lon, a configured
//!      postcode, or the postcode parsed from the receipt address (geocoded for
//!      free with postcodes.io).
//!   2. Fetch each stop's departures. Without a `when`, the live board (now) is
//!      used; with a `when`, the scheduled timetable for that date/time is used.
//!   3. Optionally filter bus departures to specific routes, and format.
//!
//! TransportAPI needs a free `app_id`/`app_key` (https://developer.transportapi.com).

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use chrono::{Local, NaiveDate, NaiveDateTime};
use serde::Deserialize;

use crate::cache::{self, Cache};

const PLACES_URL: &str = "https://transportapi.com/v3/uk/places.json";
const TRAIN_URL: &str = "https://transportapi.com/v3/uk/train/station";
const BUS_URL: &str = "https://transportapi.com/v3/uk/bus/stop";
const POSTCODES_URL: &str = "https://api.postcodes.io/postcodes";

/// Nerd Font glyphs shown before a stop name in image output (Font Awesome
/// train/bus). In text output a `(train)`/`(bus)` label is used instead. These
/// live in the Private Use Area, so the renderer can detect and enlarge them.
pub const TRAIN_ICON: char = '\u{f238}';
pub const BUS_ICON: char = '\u{f207}';

/// `[transport]` settings block.
#[derive(Debug, Clone, Deserialize)]
pub struct TransportSettings {
    /// TransportAPI application id.
    pub app_id: String,
    /// TransportAPI application key.
    pub app_key: String,
    /// Postcode to search around. When unset, it is parsed from the receipt
    /// address. Ignored if `lat`/`lon` are both set, or if only explicit stop
    /// codes are used.
    #[serde(default)]
    pub postcode: Option<String>,
    /// Explicit latitude (overrides the postcode lookup when paired with `lon`).
    #[serde(default)]
    pub lat: Option<f64>,
    /// Explicit longitude (overrides the postcode lookup when paired with `lat`).
    #[serde(default)]
    pub lon: Option<f64>,
    /// How many of the nearest train stations to show (ignored when
    /// `station_codes` is set).
    #[serde(default = "default_count")]
    pub stations: usize,
    /// How many of the nearest bus stops to show (ignored when `bus_stop_codes`
    /// is set).
    #[serde(default = "default_count")]
    pub bus_stops: usize,
    /// How many upcoming departures to list per stop. `0` disables the whole
    /// transport section (no lookups are made).
    #[serde(default = "default_count")]
    pub departures: usize,
    /// Explicit CRS station codes to use instead of the nearest stations.
    #[serde(default)]
    pub station_codes: Vec<String>,
    /// Explicit ATCO bus-stop codes to use instead of the nearest bus stops.
    #[serde(default)]
    pub bus_stop_codes: Vec<String>,
    /// When set, only bus departures on these routes/lines are shown.
    #[serde(default)]
    pub routes: Vec<String>,
}

fn default_count() -> usize {
    3
}

/// One stop to report on: an id (CRS or ATCO) and an optional display name.
struct Stop {
    id: String,
    name: Option<String>,
}

/// Build the formatted "Transport" body: the chosen stations and bus stops with
/// their next departures. `when` selects a scheduled timetable lookup for that
/// date/time; `None` uses the live "now" board. Returns the section text (no
/// heading/separator).
pub fn nearby_departures(
    cfg: &TransportSettings,
    address: &str,
    when: Option<NaiveDateTime>,
    cache: Option<&Cache>,
    columns: usize,
    icons: bool,
) -> Result<String> {
    let (stations, stops) = select_stops(cfg, address, cache)?;

    if stations.is_empty() && stops.is_empty() {
        return Ok("No stations or bus stops configured or found nearby.\n".to_string());
    }

    let mut out = String::new();

    for station in stations {
        match fetch_train_departures(cfg, &station.id, when, cache) {
            Ok((resolved, deps)) => {
                if let Some(n) = &resolved {
                    remember_name(cache, &station.id, n);
                }
                let name = station
                    .name
                    .or(resolved)
                    .or_else(|| cached_name(cache, &station.id))
                    .unwrap_or_else(|| station.id.clone());
                out.push_str(&stop_header(icons, TRAIN_ICON, "train", &name, columns));
                out.push('\n');
                push_departures(&mut out, &deps, cfg.departures, columns);
            }
            Err(e) => {
                let name = station
                    .name
                    .or_else(|| cached_name(cache, &station.id))
                    .unwrap_or_else(|| station.id.clone());
                eprintln!("warning: departures for `{}` failed: {e:#}", station.id);
                out.push_str(&stop_header(icons, TRAIN_ICON, "train", &name, columns));
                out.push_str("\n  (departures unavailable)\n");
            }
        }
        out.push('\n');
    }

    for stop in stops {
        match fetch_bus_departures(cfg, &stop.id, when, cache) {
            Ok((resolved, deps)) => {
                if let Some(n) = &resolved {
                    remember_name(cache, &stop.id, n);
                }
                let name = stop
                    .name
                    .or(resolved)
                    .or_else(|| cached_name(cache, &stop.id))
                    .unwrap_or_else(|| stop.id.clone());
                out.push_str(&stop_header(icons, BUS_ICON, "bus", &name, columns));
                out.push('\n');
                push_departures(&mut out, &deps, cfg.departures, columns);
            }
            Err(e) => {
                let name = stop
                    .name
                    .or_else(|| cached_name(cache, &stop.id))
                    .unwrap_or_else(|| stop.id.clone());
                eprintln!("warning: departures for `{}` failed: {e:#}", stop.id);
                out.push_str(&stop_header(icons, BUS_ICON, "bus", &name, columns));
                out.push_str("\n  (departures unavailable)\n");
            }
        }
        out.push('\n');
    }

    Ok(out.trim_end().to_string() + "\n")
}

/// Build a stop's heading line: an icon prefix in image output, or a
/// `(train)`/`(bus)` label in text output. The name is shortened to fit the
/// available column width either way.
fn stop_header(icons: bool, icon: char, label: &str, name: &str, columns: usize) -> String {
    if icons {
        // The icon is drawn enlarged (~1.5 columns) plus two spaces, so reserve
        // four columns of prefix when budgeting the name.
        format!("{icon}  {}", truncate(name, columns.saturating_sub(4)))
    } else {
        let suffix = format!(" ({label})");
        let budget = columns.saturating_sub(suffix.chars().count());
        format!("{}{suffix}", truncate(name, budget))
    }
}

/// Decide which stations and bus stops to report on: explicit codes when given,
/// otherwise the nearest few from a proximity search.
fn select_stops(
    cfg: &TransportSettings,
    address: &str,
    cache: Option<&Cache>,
) -> Result<(Vec<Stop>, Vec<Stop>)> {
    let train_nearest = cfg.station_codes.is_empty() && cfg.stations > 0;
    let bus_nearest = cfg.bus_stop_codes.is_empty() && cfg.bus_stops > 0;

    // Only geocode if a proximity search is needed for either mode.
    let loc = if train_nearest || bus_nearest {
        Some(resolve_location(cfg, address, cache)?)
    } else {
        None
    };

    let stations = if !cfg.station_codes.is_empty() {
        cfg.station_codes
            .iter()
            .map(|id| Stop {
                id: id.clone(),
                name: None,
            })
            .collect()
    } else if let Some((lat, lon)) = loc {
        // Fetch a few extra and let `nearest` pick, in case ordering varies.
        let places = fetch_places_of_type(cfg, lat, lon, "train_station", cfg.stations + 3, cache)?;
        nearest(&places, lat, lon, "train_station", cfg.stations)
            .into_iter()
            .filter_map(|p| {
                p.station_code.clone().map(|id| Stop {
                    id,
                    name: Some(p.display_name()),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    let stops = if !cfg.bus_stop_codes.is_empty() {
        cfg.bus_stop_codes
            .iter()
            .map(|id| Stop {
                id: id.clone(),
                name: None,
            })
            .collect()
    } else if let Some((lat, lon)) = loc {
        let places = fetch_places_of_type(cfg, lat, lon, "bus_stop", cfg.bus_stops + 3, cache)?;
        nearest(&places, lat, lon, "bus_stop", cfg.bus_stops)
            .into_iter()
            .filter_map(|p| {
                p.atcocode.clone().map(|id| Stop {
                    id,
                    name: Some(p.display_name()),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    log::debug!(
        "selected {} station(s) and {} bus stop(s)",
        stations.len(),
        stops.len()
    );
    Ok((stations, stops))
}

/// A single upcoming departure, normalised across trains and buses.
struct Departure {
    /// Best-known departure time as `HH:MM`, used for sorting and display.
    time: String,
    /// Where it's going (destination or bus route + direction).
    to: String,
    /// Short status note, e.g. "on time", "exp 09:38", "Cancelled", "Plat 2".
    note: String,
}

impl Departure {
    fn line(&self, columns: usize) -> String {
        // Prefix is "  " + 5-char time + "  " = 9 columns; reserve the note (plus
        // its 2-space gap) at the end and give the rest to the destination.
        let note_w = if self.note.is_empty() {
            0
        } else {
            self.note.chars().count() + 2
        };
        let to_budget = columns.saturating_sub(9 + note_w).max(4);
        let to = truncate(&self.to, to_budget);
        if self.note.is_empty() {
            format!("  {:<5}  {to}", self.time)
        } else {
            format!("  {:<5}  {to}  {}", self.time, self.note)
        }
    }
}

fn push_departures(out: &mut String, deps: &[Departure], limit: usize, columns: usize) {
    if deps.is_empty() {
        out.push_str("  (no departures)\n");
        return;
    }
    for dep in deps.iter().take(limit) {
        out.push_str(&dep.line(columns));
        out.push('\n');
    }
}

// --- Location resolution ---------------------------------------------------

fn resolve_location(
    cfg: &TransportSettings,
    address: &str,
    cache: Option<&Cache>,
) -> Result<(f64, f64)> {
    if let (Some(lat), Some(lon)) = (cfg.lat, cfg.lon) {
        return Ok((lat, lon));
    }
    let postcode = cfg
        .postcode
        .clone()
        .or_else(|| postcode_from_address(address))
        .ok_or_else(|| anyhow!("no `postcode` set in [transport] and none found in the address"))?;
    geocode_postcode(&postcode, cache)
}

/// Take the last comma-separated part of the address as the postcode.
fn postcode_from_address(address: &str) -> Option<String> {
    let last = address.rsplit(',').next()?.trim();
    if last.is_empty() {
        None
    } else {
        Some(last.to_string())
    }
}

#[derive(Deserialize)]
struct PostcodeResponse {
    result: Option<PostcodeResult>,
}

#[derive(Deserialize)]
struct PostcodeResult {
    latitude: f64,
    longitude: f64,
}

fn geocode_postcode(postcode: &str, cache: Option<&Cache>) -> Result<(f64, f64)> {
    let cleaned: String = postcode.chars().filter(|c| !c.is_whitespace()).collect();
    let url = format!("{POSTCODES_URL}/{cleaned}");
    // postcodes.io is keyless, so there's no secret to redact and the mapping
    // never changes: treat it as a stable, indefinitely valid cache entry.
    let body = cache::cached(cache, "geocode", &cleaned, None, true, || {
        get(&url).with_context(|| format!("geocoding postcode `{postcode}`"))
    })?;
    let data: PostcodeResponse =
        serde_json::from_str(&body).context("parsing postcodes.io response")?;
    let result = data
        .result
        .ok_or_else(|| anyhow!("postcode `{postcode}` not found"))?;
    Ok((result.latitude, result.longitude))
}

// --- Places (nearest stops) ------------------------------------------------

#[derive(Deserialize)]
struct PlacesResponse {
    #[serde(default)]
    member: Vec<Place>,
}

#[derive(Deserialize, Clone)]
struct Place {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    latitude: Option<f64>,
    #[serde(default)]
    longitude: Option<f64>,
    /// CRS station code (train stations only).
    #[serde(default)]
    station_code: Option<String>,
    /// ATCO code (bus stops only).
    #[serde(default)]
    atcocode: Option<String>,
}

impl Place {
    fn display_name(&self) -> String {
        match (&self.name, &self.description) {
            (Some(n), Some(d)) if !d.is_empty() && d != n => format!("{n}, {d}"),
            (Some(n), _) => n.clone(),
            (None, Some(d)) => d.clone(),
            _ => "(unnamed)".to_string(),
        }
    }
}

/// Fetch nearby places of a single type, ordered by distance. Querying one type
/// at a time matters: the combined nearby search is capped (~10 results), and in
/// a built-up area the closest results are all bus stops, which would crowd out
/// the more widely-spaced train stations entirely.
fn fetch_places_of_type(
    cfg: &TransportSettings,
    lat: f64,
    lon: f64,
    place_type: &str,
    limit: usize,
    cache: Option<&Cache>,
) -> Result<Vec<Place>> {
    let url = format!(
        "{PLACES_URL}?lat={lat}&lon={lon}&type={place_type}&limit={limit}&app_id={}&app_key={}",
        cfg.app_id, cfg.app_key
    );
    // Places don't move, so cache them indefinitely (stable). Round coordinates
    // in the key so near-identical lookups share an entry.
    let key = format!("{place_type}|{lat:.4}|{lon:.4}|{limit}");
    let body = cached_get(cache, "places", &key, None, true, url, &cfg.app_key)
        .context("requesting nearby places from TransportAPI")?;
    let data: PlacesResponse =
        serde_json::from_str(&body).context("parsing TransportAPI places response")?;
    remember_names(cache, &data.member);
    Ok(data.member)
}

/// Record each place's code -> display name in the cache, so a stop pinned by
/// code can still be shown with a friendly name later even when the departures
/// API is unavailable (e.g. over quota). Running `--list-stops` once populates
/// this. Names are stable, so we only write when the value is new to keep the
/// append-only cache from growing on repeat runs.
fn remember_names(cache: Option<&Cache>, places: &[Place]) {
    for p in places {
        if let Some(code) = p.station_code.as_deref().or(p.atcocode.as_deref()) {
            remember_name(cache, code, &p.display_name());
        }
    }
}

/// Record a single stop code -> name mapping (from a place or a live board).
fn remember_name(cache: Option<&Cache>, code: &str, name: &str) {
    if let Some(c) = cache {
        if c.lookup("name", code).ok().flatten().as_deref() != Some(name) {
            let _ = c.store("name", code, None, name);
        }
    }
}

/// The cached friendly name for a stop code, if one was recorded earlier.
fn cached_name(cache: Option<&Cache>, code: &str) -> Option<String> {
    cache?.lookup("name", code).ok().flatten()
}

/// List nearby train stations and bus stops with their codes and distances, to
/// help populate `station_codes` (CRS) and `bus_stop_codes` (ATCO).
pub fn list_stops(cfg: &TransportSettings, address: &str, cache: Option<&Cache>) -> Result<String> {
    let (lat, lon) = resolve_location(cfg, address, cache)?;
    let stations = fetch_places_of_type(cfg, lat, lon, "train_station", 10, cache)?;
    let stops = fetch_places_of_type(cfg, lat, lon, "bus_stop", 10, cache)?;

    let mut out = String::new();
    out.push_str("Nearest train stations  (station_codes = CRS)\n");
    append_listing(&mut out, &stations, lat, lon, "train_station", |p| {
        p.station_code.clone()
    });
    out.push_str("\nNearest bus stops  (bus_stop_codes = ATCO)\n");
    append_listing(&mut out, &stops, lat, lon, "bus_stop", |p| {
        p.atcocode.clone()
    });
    Ok(out)
}

/// Append up to ten of the closest places of `kind`, each as
/// `  CODE  DIST  Name`, sorted nearest first.
fn append_listing(
    out: &mut String,
    places: &[Place],
    lat: f64,
    lon: f64,
    kind: &str,
    code_of: impl Fn(&Place) -> Option<String>,
) {
    let mut rows: Vec<(f64, String, String)> = places
        .iter()
        .filter(|p| p.kind == kind)
        .filter_map(|p| {
            let (plat, plon) = (p.latitude?, p.longitude?);
            let code = code_of(p)?;
            Some((haversine(lat, lon, plat, plon), code, p.display_name()))
        })
        .collect();
    rows.sort_by(|a, b| a.0.total_cmp(&b.0));

    if rows.is_empty() {
        out.push_str("  (none found nearby)\n");
        return;
    }

    let code_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
    for (dist, code, name) in rows.into_iter().take(10) {
        out.push_str(&format!(
            "  {code:<code_w$}  {:>7}  {name}\n",
            format_distance(dist)
        ));
    }
}

fn format_distance(metres: f64) -> String {
    if metres < 1000.0 {
        format!("{metres:.0} m")
    } else {
        format!("{:.1} km", metres / 1000.0)
    }
}

/// Keep the `limit` closest places of the given kind, sorted by distance.
fn nearest(places: &[Place], lat: f64, lon: f64, kind: &str, limit: usize) -> Vec<Place> {
    let mut matches: Vec<(f64, Place)> = places
        .iter()
        .filter(|p| p.kind == kind)
        .filter_map(|p| match (p.latitude, p.longitude) {
            (Some(plat), Some(plon)) => Some((haversine(lat, lon, plat, plon), p.clone())),
            _ => None,
        })
        .collect();
    matches.sort_by(|a, b| a.0.total_cmp(&b.0));
    matches.into_iter().take(limit).map(|(_, p)| p).collect()
}

/// Great-circle distance in metres.
fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().asin()
}

// --- Train departures ------------------------------------------------------

#[derive(Deserialize)]
struct TrainLive {
    #[serde(default)]
    station_name: Option<String>,
    #[serde(default)]
    departures: TrainDeps,
}

/// The live board nests departures under `all`; the timetable returns a bare
/// array. Accept either.
#[derive(Deserialize)]
#[serde(untagged)]
enum TrainDeps {
    Grouped(TrainGroup),
    Flat(Vec<TrainDeparture>),
}

impl Default for TrainDeps {
    fn default() -> Self {
        TrainDeps::Flat(Vec::new())
    }
}

impl TrainDeps {
    fn into_vec(self) -> Vec<TrainDeparture> {
        match self {
            TrainDeps::Grouped(g) => g.all,
            TrainDeps::Flat(v) => v,
        }
    }
}

#[derive(Deserialize, Default)]
struct TrainGroup {
    #[serde(default)]
    all: Vec<TrainDeparture>,
}

#[derive(Deserialize)]
struct TrainDeparture {
    #[serde(default)]
    aimed_departure_time: Option<String>,
    #[serde(default)]
    expected_departure_time: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    destination_name: Option<String>,
}

fn fetch_train_departures(
    cfg: &TransportSettings,
    code: &str,
    when: Option<NaiveDateTime>,
    cache: Option<&Cache>,
) -> Result<(Option<String>, Vec<Departure>)> {
    let url = match when {
        Some(dt) => format!(
            "{TRAIN_URL}/{code}/{}/{}/timetable.json?app_id={}&app_key={}&train_status=passenger",
            dt.format("%Y-%m-%d"),
            dt.format("%H:%M"),
            cfg.app_id,
            cfg.app_key
        ),
        None => format!(
            "{TRAIN_URL}/{code}/live.json?app_id={}&app_key={}&train_status=passenger",
            cfg.app_id, cfg.app_key
        ),
    };
    let (for_date, when_key) = match when {
        Some(dt) => (Some(dt.date()), dt.format("%Y-%m-%dT%H:%M").to_string()),
        None => (Some(Local::now().date_naive()), "live".to_string()),
    };
    let key = format!("{code}|{when_key}");
    let body = cached_get(cache, "train", &key, for_date, false, url, &cfg.app_key)?;
    let data: TrainLive = serde_json::from_str(&body).context("parsing train board")?;
    let station_name = data.station_name.clone();

    let mut deps: Vec<Departure> = data
        .departures
        .into_vec()
        .into_iter()
        .filter_map(|d| {
            let time = d.aimed_departure_time.clone()?;
            let mut notes = Vec::new();
            if let Some(plat) = d.platform.as_deref().filter(|p| !p.is_empty()) {
                notes.push(format!("Plat {plat}"));
            }
            notes.push(train_status(&d));
            Some(Departure {
                time,
                to: d.destination_name.unwrap_or_else(|| "?".to_string()),
                note: notes.join("  "),
            })
        })
        .collect();
    apply_time_window(&mut deps, when);
    Ok((station_name, deps))
}

/// "on time", "exp 09:38", or "Cancelled".
fn train_status(d: &TrainDeparture) -> String {
    let status = d.status.as_deref().unwrap_or("");
    if status.eq_ignore_ascii_case("CANCELLED") {
        return "Cancelled".to_string();
    }
    match (
        d.aimed_departure_time.as_deref(),
        d.expected_departure_time.as_deref(),
    ) {
        (Some(aimed), Some(exp)) if !exp.is_empty() && exp != aimed => format!("exp {exp}"),
        _ => "on time".to_string(),
    }
}

// --- Bus departures --------------------------------------------------------

#[derive(Deserialize)]
struct BusLive {
    #[serde(default)]
    stop_name: Option<String>,
    #[serde(default)]
    departures: BusDeps,
}

/// The live board keys departures by route; the timetable returns a bare array.
/// Accept either.
#[derive(Deserialize)]
#[serde(untagged)]
enum BusDeps {
    Grouped(HashMap<String, Vec<BusDeparture>>),
    Flat(Vec<BusDeparture>),
}

impl Default for BusDeps {
    fn default() -> Self {
        BusDeps::Flat(Vec::new())
    }
}

impl BusDeps {
    fn into_vec(self) -> Vec<BusDeparture> {
        match self {
            BusDeps::Grouped(m) => m.into_values().flatten().collect(),
            BusDeps::Flat(v) => v,
        }
    }
}

#[derive(Deserialize)]
struct BusDeparture {
    #[serde(default)]
    line: Option<String>,
    #[serde(default)]
    line_name: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    aimed_departure_time: Option<String>,
    #[serde(default)]
    expected_departure_time: Option<String>,
    #[serde(default)]
    best_departure_estimate: Option<String>,
}

fn fetch_bus_departures(
    cfg: &TransportSettings,
    atco: &str,
    when: Option<NaiveDateTime>,
    cache: Option<&Cache>,
) -> Result<(Option<String>, Vec<Departure>)> {
    let url = match when {
        Some(dt) => format!(
            "{BUS_URL}/{atco}/timetable.json?date={}&app_id={}&app_key={}&group=route&nextbuses=no",
            dt.format("%Y-%m-%d"),
            cfg.app_id,
            cfg.app_key
        ),
        None => format!(
            "{BUS_URL}/{atco}/live.json?app_id={}&app_key={}&group=route&nextbuses=no",
            cfg.app_id, cfg.app_key
        ),
    };
    let (for_date, when_key) = match when {
        Some(dt) => (Some(dt.date()), dt.format("%Y-%m-%d").to_string()),
        None => (Some(Local::now().date_naive()), "live".to_string()),
    };
    let key = format!("{atco}|{when_key}");
    let body = cached_get(cache, "bus", &key, for_date, false, url, &cfg.app_key)?;
    let data: BusLive = serde_json::from_str(&body).context("parsing bus board")?;
    let stop_name = data.stop_name.clone();

    let mut deps: Vec<Departure> = data
        .departures
        .into_vec()
        .into_iter()
        .filter(|d| route_matches(&cfg.routes, d))
        .filter_map(|d| {
            let time = d
                .best_departure_estimate
                .clone()
                .or_else(|| d.aimed_departure_time.clone())?;
            let route = d.line_name.or(d.line).unwrap_or_default();
            let direction = d.direction.unwrap_or_default();
            let to = format!("{route} {direction}").trim().to_string();
            let note = match (
                d.aimed_departure_time.as_deref(),
                d.expected_departure_time.as_deref(),
            ) {
                (Some(aimed), Some(exp)) if !exp.is_empty() && exp != aimed => format!("exp {exp}"),
                _ => String::new(),
            };
            Some(Departure { time, to, note })
        })
        .collect();
    apply_time_window(&mut deps, when);
    Ok((stop_name, deps))
}

/// True if `routes` is empty (no filter) or the bus's line/line_name matches one.
fn route_matches(routes: &[String], d: &BusDeparture) -> bool {
    if routes.is_empty() {
        return true;
    }
    let line = d.line.as_deref().unwrap_or_default();
    let line_name = d.line_name.as_deref().unwrap_or_default();
    routes
        .iter()
        .any(|r| r.eq_ignore_ascii_case(line) || r.eq_ignore_ascii_case(line_name))
}

/// Sort by time and, for a timetable lookup, drop anything before the requested
/// time (the bus timetable covers the whole day). `HH:MM` strings compare
/// chronologically.
fn apply_time_window(deps: &mut Vec<Departure>, when: Option<NaiveDateTime>) {
    deps.sort_by(|a, b| a.time.cmp(&b.time));
    if let Some(dt) = when {
        let cutoff = dt.format("%H:%M").to_string();
        deps.retain(|d| d.time >= cutoff);
    }
}

// --- Helpers ---------------------------------------------------------------

/// Fetch `url` through the cache, redacting the TransportAPI `app_key` from the
/// stored body first. TransportAPI embeds the key in nested URLs
/// (e.g. `service_timetable.id`), so it must never be persisted verbatim.
fn cached_get(
    cache: Option<&Cache>,
    kind: &str,
    key: &str,
    for_date: Option<NaiveDate>,
    stable: bool,
    url: String,
    app_key: &str,
) -> Result<String> {
    let app_key = app_key.to_string();
    cache::cached(cache, kind, key, for_date, stable, move || {
        get(&url).map(|body| redact(&body, &app_key))
    })
}

/// Replace occurrences of the API key with a placeholder before storage.
fn redact(body: &str, app_key: &str) -> String {
    if app_key.is_empty() {
        body.to_string()
    } else {
        body.replace(app_key, "APP_KEY")
    }
}

/// GET a URL as a String, pretending to be curl (some services vary output by
/// User-Agent).
fn get(url: &str) -> Result<String> {
    // Log the endpoint only (drop the query string, which carries the API key).
    log::debug!("GET {}", url.split('?').next().unwrap_or(url));
    ureq::get(url)
        .header("User-Agent", "curl/8.0.0")
        .call()
        .with_context(|| format!("requesting `{url}`"))?
        .body_mut()
        .read_to_string()
        .context("reading response body")
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

    #[test]
    fn parses_postcode_from_address() {
        assert_eq!(
            postcode_from_address("11 Example Street, Townsville, AB1 2CD").as_deref(),
            Some("AB1 2CD")
        );
        assert_eq!(postcode_from_address("").as_deref(), None);
    }

    #[test]
    fn keeps_the_closest_places_of_a_kind() {
        let places = vec![
            place("train_station", "Far", 51.60, -0.10),
            place("train_station", "Near", 51.41, -0.02),
            place("bus_stop", "Stop", 51.41, -0.02),
        ];
        let got = nearest(&places, 51.41, -0.02, "train_station", 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name.as_deref(), Some("Near"));
    }

    #[test]
    fn time_window_drops_earlier_departures() {
        let mut deps = vec![dep("08:50"), dep("09:05"), dep("09:20")];
        let when = "2026-07-16T09:00:00".parse::<NaiveDateTime>().unwrap();
        apply_time_window(&mut deps, Some(when));
        let times: Vec<&str> = deps.iter().map(|d| d.time.as_str()).collect();
        assert_eq!(times, vec!["09:05", "09:20"]);
    }

    #[test]
    fn route_filter_matches_line_or_line_name() {
        let bus = BusDeparture {
            line: Some("162".into()),
            line_name: Some("162".into()),
            direction: None,
            aimed_departure_time: None,
            expected_departure_time: None,
            best_departure_estimate: None,
        };
        assert!(route_matches(&[], &bus));
        assert!(route_matches(&["162".into()], &bus));
        assert!(!route_matches(&["54".into()], &bus));
    }

    #[test]
    fn formats_distance_in_m_or_km() {
        assert_eq!(format_distance(320.0), "320 m");
        assert_eq!(format_distance(1500.0), "1.5 km");
    }

    fn place(kind: &str, name: &str, lat: f64, lon: f64) -> Place {
        Place {
            kind: kind.to_string(),
            name: Some(name.to_string()),
            description: None,
            latitude: Some(lat),
            longitude: Some(lon),
            station_code: None,
            atcocode: None,
        }
    }

    fn dep(time: &str) -> Departure {
        Departure {
            time: time.to_string(),
            to: "Somewhere".to_string(),
            note: String::new(),
        }
    }
}
