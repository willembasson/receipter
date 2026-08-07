use std::fs;
use std::io::{Cursor, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{Local, NaiveDate, NaiveTime};
use clap::{ArgGroup, Parser};
use escpos::driver::{ConsoleDriver, Driver, NetworkDriver};
use escpos::printer::Printer;
use escpos::printer_options::PrinterOptions;
use escpos::utils::*;
use image::{DynamicImage, GrayImage, Luma};
use imageproc::drawing::{draw_text_mut, text_size};
use serde::Deserialize;

mod calendar;
use calendar::Meeting;
mod cache;
mod transport;

/// Fetch the current weather from wttr.in and print it on an ESC/POS printer.
#[derive(Debug, Parser)]
#[command(version, about)]
#[command(group(ArgGroup::new("mode").args(["stdout", "raw"]).multiple(false)))]
struct Cli {
    /// Path to the settings file.
    #[arg(short, long, default_value = "settings.toml")]
    config: PathBuf,

    /// Increase logging verbosity: -v for debug, -vv for trace. Logs go to
    /// stderr, so they don't interfere with --stdout output.
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Location to look up the weather for (overrides the settings file).
    #[arg(short, long)]
    location: Option<String>,

    /// Printer endpoint as "host:port" (overrides the settings file).
    #[arg(short, long)]
    endpoint: Option<String>,

    /// Number of forecast days to request from wttr.in (0 = current only).
    #[arg(short, long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=2))]
    days: u8,

    /// Date to print as YYYYMMDD (default: today). Weather is available for
    /// today and the next couple of forecast days.
    #[arg(short = 'D', long, value_name = "YYYYMMDD", value_parser = parse_date)]
    date: Option<NaiveDate>,

    /// Shorthand for the date of tomorrow (conflicts with --date).
    #[arg(short = 't', long, conflicts_with = "date")]
    tomorrow: bool,

    /// Look up transport departures at this time (HHMM) instead of now. Enables
    /// the transport section for any --date, using the scheduled timetable.
    #[arg(long, value_name = "HHMM", value_parser = parse_time)]
    at: Option<NaiveTime>,

    /// List nearby stations and bus stops with their codes (to fill in
    /// `station_codes` / `bus_stop_codes`), then exit.
    #[arg(long)]
    list_stops: bool,

    /// Ignore cached freshness and refetch live data (still stored to the cache).
    #[arg(long)]
    refresh: bool,

    /// Disable the SQLite cache entirely for this run.
    #[arg(long)]
    no_cache: bool,

    /// Print the weather text to stdout instead of sending it to the printer.
    #[arg(long)]
    stdout: bool,

    /// Emit the raw ESC/POS byte stream to stdout (ConsoleDriver, for debugging).
    #[arg(long)]
    raw: bool,

    /// Render the report as an image so full Unicode (arrows, degrees, icons) prints.
    /// This is the default; combine with --stdout to display it inline in a
    /// kitty-compatible terminal.
    #[arg(long = "imageText")]
    image_text: bool,

    /// Print as plain ESC/POS text instead of the default image rendering
    /// (loses icons and other non-ASCII glyphs).
    #[arg(long)]
    text: bool,

    /// Write the rendered image to a PNG file instead of printing (preview mode).
    #[arg(short, long, value_name = "FILE", conflicts_with_all = ["stdout", "raw"])]
    output: Option<PathBuf>,

    /// How to reduce a printed image to the printer's pure black and white:
    /// `auto` dithers photographs but keeps an already black-and-white image
    /// (such as a receipt) crisp, `on` always dithers, `off` always thresholds.
    #[arg(long, value_name = "MODE", value_enum, default_value_t = Dither::Auto)]
    dither: Dither,

    /// Lighten a printed image by this percentage before it is reduced to black
    /// and white (0 = unchanged, 100 = blank). Thermal paper spreads each dot,
    /// so a dithered photo usually prints darker than it looks on screen; try
    /// `--lighten 30` to lay down 30% fewer dots.
    #[arg(long, value_name = "PERCENT", default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=100))]
    lighten: u8,

    /// Print this image instead of the report (PNG, JPEG, GIF, BMP or WebP).
    /// Use `-` to read it from stdin; an image piped in on stdin is picked up
    /// automatically.
    #[arg(value_name = "IMAGE")]
    image: Option<PathBuf>,
}

/// How an image is reduced to the two levels the printer can actually put on
/// paper. See [`prepare_image_for_print`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Dither {
    /// Dither only images with enough mid-tones to need it.
    Auto,
    /// Always dither.
    On,
    /// Never dither; threshold at mid-gray.
    Off,
}

/// Values loaded from the settings file. CLI arguments take precedence.
#[derive(Debug, Deserialize)]
struct Settings {
    endpoint: String,
    location: String,
    address: String,
    /// Zero or more Google Calendar "secret iCal" sources. When set, today's
    /// meetings are printed below the weather.
    #[serde(default)]
    calendars: Vec<calendar::CalendarSource>,
    /// Optional TransportAPI config. When set, the nearest stations and bus
    /// stops with their next departures are printed below the meetings.
    #[serde(default)]
    transport: Option<transport::TransportSettings>,
    #[serde(default)]
    cache: CacheSettings,
    #[serde(default)]
    image: ImageSettings,
}

/// Settings that control the SQLite cache for external calls.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct CacheSettings {
    /// Path to the SQLite database file.
    path: PathBuf,
    /// How long a live result stays fresh before being refetched, in minutes.
    ttl_minutes: i64,
}

impl Default for CacheSettings {
    fn default() -> Self {
        Self {
            path: PathBuf::from("cache.sqlite"),
            ttl_minutes: 30,
        }
    }
}

/// Settings that control the `--imageText` (image) rendering mode.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct ImageSettings {
    /// Print width in dots (80mm = 576, 58mm = 384). Must be a multiple of 8.
    width: u32,
    /// Path to the TrueType/OpenType font used to rasterize the report.
    font: PathBuf,
    /// Body font size, in pixels.
    font_size: f32,
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self {
            width: 576,
            font: PathBuf::from("fonts/JetBrainsMonoNerdFontMono-Regular.ttf"),
            font_size: 28.0,
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let settings = load_settings(&cli.config)?;

    let location = cli.location.clone().unwrap_or(settings.location);
    let endpoint = cli.endpoint.clone().unwrap_or(settings.endpoint);
    let address = settings.address;
    log::debug!("loaded settings from `{}`", cli.config.display());

    // Image passthrough: an image given as an argument, or piped in on stdin,
    // is printed as-is. None of the report lookups happen in this mode.
    if let Some(bytes) = read_input_image(cli.image.as_deref())? {
        log::debug!("input: {} byte(s) of image data", bytes.len());
        let png = prepare_image_for_print(&bytes, &settings.image, cli.dither, cli.lighten)?;
        return output_image(&cli, &endpoint, &png, &settings.image);
    }

    // Open the cache up front so every external call can read/write it. A cache
    // failure is non-fatal: we just run without it.
    let cache = if cli.no_cache {
        log::debug!("cache disabled via --no-cache");
        None
    } else {
        match cache::Cache::open(
            &settings.cache.path,
            settings.cache.ttl_minutes,
            cli.refresh,
        ) {
            Ok(c) => {
                log::debug!(
                    "cache open at `{}` (ttl {} min{})",
                    settings.cache.path.display(),
                    settings.cache.ttl_minutes,
                    if cli.refresh { ", --refresh" } else { "" }
                );
                Some(c)
            }
            Err(e) => {
                eprintln!("warning: cache disabled: {e:#}");
                None
            }
        }
    };
    let cache = cache.as_ref();

    // Utility mode: print nearby stops with their codes, then exit.
    if cli.list_stops {
        match &settings.transport {
            Some(cfg) => {
                print!("{}", transport::list_stops(cfg, &address, cache).await?);
                return Ok(());
            }
            None => bail!("--list-stops needs a [transport] section with app_id/app_key"),
        }
    }

    let today = Local::now().date_naive();
    let target_date = if cli.tomorrow {
        today + chrono::Days::new(1)
    } else {
        cli.date.unwrap_or(today)
    };
    let is_today = target_date == today;
    let date = target_date.format("%A, %d %B %Y").to_string();
    log::info!(
        "building report for {} (location `{}`)",
        target_date.format("%Y-%m-%d"),
        location
    );

    // Weather: today uses wttr.in's ASCII art; a future date within the forecast
    // window uses the JSON forecast; a past date is served from cache history.
    // Results are cached (and dated) so a later run for a past date still works.
    let report = weather_report(cache, &location, cli.days, target_date, today).await?;

    // Append the day's meetings below the weather when calendars are configured.
    // A calendar failure shouldn't stop the rest from printing.
    let report = if settings.calendars.is_empty() {
        report
    } else {
        log::debug!(
            "loading meetings from {} calendar(s)",
            settings.calendars.len()
        );
        let heading = match (target_date - today).num_days() {
            0 => "Today's meetings",
            1 => "Tomorrow's meetings",
            _ => "Meetings",
        };
        append_meetings(
            &report,
            heading,
            &calendar::meetings_on(&settings.calendars, target_date, cache).await,
        )
    };

    // Append nearby departures below the meetings. Without --at they're the
    // live board (today only); with --at they're the scheduled timetable for the
    // target date/time. A transport failure shouldn't stop the rest.
    let when = cli.at.map(|t| target_date.and_time(t));
    let columns = body_columns(&settings.image);
    // Image output (default, --imageText, --output) can show icon glyphs, but
    // only if the configured font actually has them; otherwise (and in the text
    // modes --stdout/--raw/--text) fall back to `(train)`/`(bus)` labels.
    let icons = !cli.stdout && !cli.raw && !cli.text && font_has_icons(&settings.image);
    let report = match &settings.transport {
        // `departures = 0` disables the transport section entirely (no lookups,
        // no heading). It only appears for today, or for any date given `--at`.
        Some(cfg) if cfg.departures > 0 && (is_today || when.is_some()) => {
            log::debug!(
                "loading transport departures ({})",
                when.map(|w| w.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "live".to_string())
            );
            match transport::nearby_departures(cfg, &address, when, cache, columns, icons).await {
                Ok(body) => {
                    let heading = match cli.at {
                        Some(t) => format!("Transport from {}", t.format("%H:%M")),
                        None => "Transport".to_string(),
                    };
                    append_section(&report, &heading, &body)
                }
                Err(e) => {
                    eprintln!("warning: could not load transport info: {e:#}");
                    report
                }
            }
        }
        _ => report,
    };

    if let Some(path) = cli.output.as_deref() {
        // Preview mode: render the image and save it instead of printing.
        log::debug!("output: writing PNG preview to `{}`", path.display());
        let png = build_report_png(&address, &date, &report, &settings.image)?;
        fs::write(path, &png).with_context(|| format!("writing image to `{}`", path.display()))?;
        println!("Wrote weather image to {}", path.display());
    } else if cli.stdout {
        if cli.image_text {
            // Render the receipt as an image and either show it inline (kitty
            // graphics protocol) or, when stdout is redirected, emit the PNG
            // bytes so they can be piped or saved.
            log::debug!("output: inline image to stdout");
            let png = build_report_png(&address, &date, &report, &settings.image)?;
            emit_image_to_stdout(&png)?;
        } else {
            log::debug!("output: report text to stdout");
            print!("{report}");
        }
    } else if cli.raw {
        // ConsoleDriver::open(true) writes the ESC/POS bytes to stdout, so the
        // exact stream the printer would receive can be inspected (e.g. `| xxd`).
        log::debug!("output: raw ESC/POS byte stream to stdout");
        render(ConsoleDriver::open(true), &address, &date, &report)?;
    } else if cli.text {
        // Legacy plain-text ESC/POS printing (no image, ASCII-transliterated).
        log::debug!("output: plain-text ESC/POS to printer `{endpoint}`");
        print_report(&endpoint, &address, &date, &report)?;
    } else if cli.image_text {
        log::debug!("output: image to printer `{endpoint}`");
        print_image(&endpoint, &address, &date, &report, &settings.image)?;
    } else {
        // Default: image rendering, for consistent layout and full Unicode.
        log::debug!("output: image to printer `{endpoint}`");
        print_image(&endpoint, &address, &date, &report, &settings.image)?;
    }

    Ok(())
}

/// Initialise logging. `-v` enables this crate's debug logs, `-vv` its trace
/// logs (dependencies stay at `warn` to avoid noise). `RUST_LOG`, if set, always
/// takes precedence. Logs are written to stderr.
///
/// `rrule` is pinned to `error` because it logs a spurious `warn` about
/// `EXDATE;VALUE=DATE` (valid iCal it doesn't fully support) on every run.
fn init_logging(verbose: u8) {
    let crate_name = env!("CARGO_CRATE_NAME");
    let default = match verbose {
        0 => "warn,rrule=error".to_string(),
        1 => format!("warn,rrule=error,{crate_name}=debug"),
        _ => format!("warn,rrule=error,{crate_name}=trace"),
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default))
        .format_timestamp(None)
        .init();
}

/// Parse a `YYYYMMDD` command-line date.
fn parse_date(s: &str) -> std::result::Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s, "%Y%m%d")
        .map_err(|_| format!("`{s}` is not a valid date, expected YYYYMMDD"))
}

/// Parse a command-line time, accepting `HHMM` or `HH:MM`.
fn parse_time(s: &str) -> std::result::Result<NaiveTime, String> {
    NaiveTime::parse_from_str(s, "%H%M")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
        .map_err(|_| format!("`{s}` is not a valid time, expected HHMM"))
}

/// Append a titled section (separator + heading + body) to the report.
fn append_section(report: &str, heading: &str, body: &str) -> String {
    let mut out = report.trim_end().to_string();
    out.push_str("\n\n----------------------------------------\n");
    out.push_str(heading);
    out.push_str("\n\n");
    out.push_str(body.trim_end());
    out.push('\n');
    out
}

/// Append a meetings section to the report body under `heading`.
fn append_meetings(report: &str, heading: &str, meetings: &[Meeting]) -> String {
    let body = if meetings.is_empty() {
        "No meetings.".to_string()
    } else {
        meetings
            .iter()
            .map(Meeting::line)
            .collect::<Vec<_>>()
            .join("\n")
    };
    append_section(report, heading, &body)
}

fn load_settings(path: &Path) -> Result<Settings> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading settings file `{}`", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing settings file `{}`", path.display()))
}

/// Weather for `target`, using the cache for freshness and history:
/// - today  -> wttr.in ASCII current conditions (cached, TTL);
/// - future -> wttr.in JSON forecast for that date (cached per date);
/// - past   -> whatever was previously stored for that date, if anything.
async fn weather_report(
    cache: Option<&cache::Cache>,
    location: &str,
    days: u8,
    target: NaiveDate,
    today: NaiveDate,
) -> Result<String> {
    use std::cmp::Ordering;
    match target.cmp(&today) {
        Ordering::Equal => {
            log::debug!("weather: current conditions for today");
            cache::cached(
                cache,
                "weather",
                &weather_key_current(location, days, target),
                Some(target),
                false,
                || fetch_weather(location, days),
            )
            .await
        }
        Ordering::Greater => {
            log::debug!("weather: forecast for future date");
            Ok(
                forecast_report(cache, location, target).await?.unwrap_or_else(|| {
                    "No weather available for this date.\n(wttr.in only forecasts the next few days.)"
                        .to_string()
                }),
            )
        }
        Ordering::Less => {
            log::debug!("weather: past date, looking up stored history");
            if let Some(c) = cache {
                if let Some(hit) = c.lookup("forecast", &weather_key_forecast(location, target))? {
                    return Ok(hit);
                }
                if let Some(hit) =
                    c.lookup("weather", &weather_key_current(location, days, target))?
                {
                    return Ok(hit);
                }
            }
            Ok("No weather stored for this past date.".to_string())
        }
    }
}

/// A cached-or-fetched forecast string for `target`, or `None` when the date is
/// outside wttr.in's forecast window (and not already cached).
async fn forecast_report(
    cache: Option<&cache::Cache>,
    location: &str,
    target: NaiveDate,
) -> Result<Option<String>> {
    let key = weather_key_forecast(location, target);
    if let Some(c) = cache {
        if let Some(hit) = c.get_fresh("forecast", &key, Some(target), false)? {
            return Ok(Some(hit));
        }
    }
    match fetch_forecast(location, target).await? {
        Some(text) => {
            if let Some(c) = cache {
                c.store("forecast", &key, Some(target), &text)?;
            }
            Ok(Some(text))
        }
        None => Ok(None),
    }
}

fn weather_key_current(location: &str, days: u8, date: NaiveDate) -> String {
    format!("current|{location}|{days}|{}", date.format("%Y-%m-%d"))
}

fn weather_key_forecast(location: &str, date: NaiveDate) -> String {
    format!("forecast|{location}|{}", date.format("%Y-%m-%d"))
}

/// Fetch a compact, color-free weather report for `location` from wttr.in.
async fn fetch_weather(location: &str, days: u8) -> Result<String> {
    let url = format!("https://wttr.in/{location}?{days}T");

    // wttr.in only returns plain text for terminal-style clients, so we
    // pretend to be curl. `?{days}T` asks for the requested number of forecast
    // days with no ANSI color codes, which keeps the output printer-friendly.
    let body = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", "curl/8.0.0")
        .send()
        .await
        .with_context(|| format!("requesting weather from `{url}`"))?
        .error_for_status()
        .with_context(|| format!("weather request to `{url}` failed"))?
        .text()
        .await
        .context("reading weather response body")?;

    Ok(body)
}

/// wttr.in's `format=j1` JSON payload (only the fields we use).
#[derive(Deserialize)]
struct WttrForecast {
    weather: Vec<ForecastDay>,
}

#[derive(Deserialize)]
struct ForecastDay {
    /// Local date as `YYYY-MM-DD`.
    date: String,
    #[serde(rename = "maxtempC")]
    max_temp_c: String,
    #[serde(rename = "mintempC")]
    min_temp_c: String,
    hourly: Vec<ForecastHour>,
}

#[derive(Deserialize)]
struct ForecastHour {
    /// Slot start in wttr.in's `Hmm` form, e.g. "0", "900", "1200".
    time: String,
    #[serde(rename = "tempC")]
    temp_c: String,
    #[serde(rename = "weatherDesc")]
    weather_desc: Vec<ForecastDesc>,
    #[serde(rename = "windspeedKmph")]
    windspeed_kmph: String,
    chanceofrain: String,
}

#[derive(Deserialize)]
struct ForecastDesc {
    value: String,
}

/// Fetch a compact forecast for `target` from wttr.in's JSON API. Returns
/// `Ok(None)` when the date is outside the short-range forecast window.
async fn fetch_forecast(location: &str, target: NaiveDate) -> Result<Option<String>> {
    let url = format!("https://wttr.in/{location}?format=j1");

    let body = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", "curl/8.0.0")
        .send()
        .await
        .with_context(|| format!("requesting forecast from `{url}`"))?
        .error_for_status()
        .with_context(|| format!("forecast request to `{url}` failed"))?
        .text()
        .await
        .context("reading forecast response body")?;

    let data: WttrForecast =
        serde_json::from_str(&body).context("parsing wttr.in forecast JSON")?;

    let wanted = target.format("%Y-%m-%d").to_string();
    let Some(day) = data.weather.iter().find(|d| d.date == wanted) else {
        return Ok(None);
    };

    Ok(Some(format_forecast(location, day)))
}

/// Render one forecast day as a narrow, printer-friendly block.
fn format_forecast(location: &str, day: &ForecastDay) -> String {
    let mut out = format!("Weather forecast: {location}\n\n");
    out.push_str(&format!(
        "High {} C  /  Low {} C\n\n",
        day.max_temp_c, day.min_temp_c
    ));

    // wttr.in reports every three hours; pick four representative slots.
    let slots = [
        ("Morning", "900"),
        ("Midday", "1200"),
        ("Evening", "1800"),
        ("Night", "2100"),
    ];

    let mut max_wind = 0u32;
    let mut max_rain = 0u32;
    for (label, time) in slots {
        if let Some(hour) = day.hourly.iter().find(|h| h.time == time) {
            let desc = hour
                .weather_desc
                .first()
                .map(|d| d.value.trim())
                .unwrap_or("");
            out.push_str(&format!("{label:<8} {:>3} C  {desc}\n", hour.temp_c));
            max_wind = max_wind.max(hour.windspeed_kmph.parse().unwrap_or(0));
            max_rain = max_rain.max(hour.chanceofrain.parse().unwrap_or(0));
        }
    }

    out.push_str(&format!(
        "\nWind up to {max_wind} km/h.  Rain {max_rain}%.\n"
    ));
    out
}

/// Open the network printer and send it a formatted weather receipt.
fn print_report(endpoint: &str, address: &str, date: &str, report: &str) -> Result<()> {
    let (host, port) = parse_endpoint(endpoint)?;

    let driver = NetworkDriver::open(host, port, Some(Duration::from_secs(5)))
        .with_context(|| format!("connecting to printer at `{endpoint}`"))?;

    render(driver, address, date, report)
}

/// Build the weather receipt and send it to any ESC/POS driver.
fn render<D: Driver>(driver: D, address: &str, date: &str, report: &str) -> Result<()> {
    let mut printer = Printer::new(driver, Protocol::default(), Some(PrinterOptions::default()));
    printer.init()?;

    // The printer's default line spacing leaves large gaps; tighten it so the
    // wttr.in art lines sit together the way they do in a terminal.
    printer.line_spacing(PRINT_LINE_SPACING)?;

    printer
        .justify(JustifyMode::CENTER)?
        .bold(true)?
        .writeln(&sanitize_for_printer(address))?
        .bold(false)?
        .writeln(&sanitize_for_printer(date))?
        .feed()?
        .justify(JustifyMode::LEFT)?;

    for line in report.lines() {
        printer.writeln(&sanitize_for_printer(line))?;
    }

    printer.feed()?.print_cut()?;

    Ok(())
}

/// Vertical line spacing (in dots) used for the printed receipt.
const PRINT_LINE_SPACING: u8 = 24;

/// Open the network printer and print the weather report as an image.
fn print_image(
    endpoint: &str,
    address: &str,
    date: &str,
    report: &str,
    cfg: &ImageSettings,
) -> Result<()> {
    let (host, port) = parse_endpoint(endpoint)?;

    let driver = NetworkDriver::open(host, port, Some(Duration::from_secs(5)))
        .with_context(|| format!("connecting to printer at `{endpoint}`"))?;

    render_image(driver, address, date, report, cfg)
}

/// Rasterize the report to a bitmap and send it to any ESC/POS driver.
///
/// Unlike [`render`], this preserves full Unicode: the text is drawn with a
/// TrueType font and printed as a raster graphic, so arrows, degree signs and
/// Nerd Font icons come out exactly as the font draws them.
fn render_image<D: Driver>(
    driver: D,
    address: &str,
    date: &str,
    report: &str,
    cfg: &ImageSettings,
) -> Result<()> {
    let png = build_report_png(address, date, report, cfg)?;
    render_bit_image(driver, &png, cfg)
}

/// Send an already-encoded image to any ESC/POS driver as a raster graphic.
fn render_bit_image<D: Driver>(driver: D, png: &[u8], cfg: &ImageSettings) -> Result<()> {
    let mut printer = Printer::new(driver, Protocol::default(), Some(PrinterOptions::default()));
    printer.init()?;

    let option = BitImageOption::new(Some(cfg.width), None, BitImageSize::Normal)
        .context("building bit image options")?;

    printer
        .bit_image_from_bytes_option(png, option)?
        .feed()?
        .print_cut()?;

    Ok(())
}

/// The image to print instead of the report, if there is one.
///
/// It comes from `path` when given (`-` meaning stdin), or from stdin when
/// that's been redirected — so `receipter --stdout --imageText | receipter`
/// prints the piped receipt. `None` means there's no image and the usual report
/// should be built; stdin is left alone when it's a terminal, so an interactive
/// run never waits for input.
fn read_input_image(path: Option<&Path>) -> Result<Option<Vec<u8>>> {
    if let Some(path) = path {
        if path == Path::new("-") {
            let bytes = read_stdin()?;
            if bytes.is_empty() {
                bail!("no image data on stdin");
            }
            return Ok(Some(bytes));
        }
        let bytes =
            fs::read(path).with_context(|| format!("reading image `{}`", path.display()))?;
        return Ok(Some(bytes));
    }

    if std::io::stdin().is_terminal() {
        return Ok(None);
    }

    // Redirected but empty (e.g. `< /dev/null`, or a cron job): carry on and
    // build the report as usual.
    let bytes = read_stdin()?;
    Ok((!bytes.is_empty()).then_some(bytes))
}

fn read_stdin() -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut bytes)
        .context("reading image data from stdin")?;
    Ok(bytes)
}

/// Turn an arbitrary image into a PNG the printer can reproduce: composited
/// onto white, scaled to the full print width, and reduced to pure black and
/// white.
///
/// The printer has no grays — it inks every pixel darker than mid-gray — so the
/// reduction happens here, where the choice can be made deliberately.
/// Thresholding keeps text and line art crisp but flattens a photograph into
/// blotches; Floyd-Steinberg dithering keeps a photograph legible but stipples
/// the anti-aliased edges of text. [`Dither::Auto`] picks per image, so a
/// receipt piped straight back in prints exactly as it would have directly.
///
/// `lighten` blends the image toward paper white first, to compensate for a
/// printer that lays down more ink than the image asks for.
fn prepare_image_for_print(
    bytes: &[u8],
    cfg: &ImageSettings,
    dither: Dither,
    lighten: u8,
) -> Result<Vec<u8>> {
    check_print_width(cfg)?;

    let img = image::load_from_memory(bytes)
        .context("decoding the image (PNG, JPEG, GIF, BMP and WebP are supported)")?;
    let (src_width, src_height) = (img.width(), img.height());
    if src_width == 0 || src_height == 0 {
        bail!("the image is empty");
    }

    let mut gray = flatten_onto_white(&img);

    if src_width != cfg.width {
        // Scale to the exact print width, keeping the aspect ratio, so small
        // images fill the paper and large ones aren't clipped.
        let height = (u64::from(src_height) * u64::from(cfg.width) / u64::from(src_width)).max(1);
        let height = u32::try_from(height).unwrap_or(u32::MAX);
        log::debug!(
            "scaling image from {src_width}x{src_height} to {}x{height}",
            cfg.width
        );
        gray = image::imageops::resize(
            &gray,
            cfg.width,
            height,
            image::imageops::FilterType::Lanczos3,
        );
    }

    if gray.height() > u32::from(u16::MAX) {
        bail!(
            "the image is {} dots tall at the print width, above the printer's limit of {}",
            gray.height(),
            u16::MAX
        );
    }

    // Decide this before lightening: a heavily lightened photograph has few
    // mid-tones left, but it still needs the dithering to show anything at all.
    let dither = match dither {
        Dither::On => true,
        Dither::Off => false,
        Dither::Auto => has_midtones(&gray),
    };

    if lighten > 0 {
        log::debug!("lightening the image by {lighten}%");
        lighten_toward_white(&mut gray, lighten);
    }

    if dither {
        log::debug!("dithering the image to black and white");
        image::imageops::dither(&mut gray, &image::imageops::BiLevel);
    } else {
        // Threshold with the printer's own rule (it inks every pixel <= 128) so
        // the PNG is exactly what comes out on paper.
        for px in gray.pixels_mut() {
            px.0[0] = if px.0[0] <= 128 { 0 } else { 255 };
        }
    }

    if lighten > 0 && gray.pixels().all(|p| p.0[0] != 0) {
        // Thresholding has no half measures, so lightening solid black past the
        // half-way point wipes it out completely rather than thinning it.
        eprintln!(
            "warning: nothing is left to print after --lighten {lighten}{}",
            if dither {
                ""
            } else {
                "; try a lower percentage, or --dither on to thin the artwork instead of dropping it"
            }
        );
    }

    let mut png = Vec::new();
    DynamicImage::ImageLuma8(gray)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .context("encoding the prepared image to PNG")?;

    Ok(png)
}

/// Blend an image `percent` of the way toward paper white.
///
/// Dithering renders a tone by inking that fraction of the dots, so scaling the
/// distance from white scales the ink coverage by the same amount: at 30%, a
/// region that would have inked 60% of its dots inks 42% of them instead. That
/// is the knob to reach for when a photo prints muddy, because thermal dots
/// bleed into each other and cover more paper than the image asked for.
///
/// Thresholded artwork is barely affected until the percentage passes 50: solid
/// black has to lighten past mid-gray before it stops being inked, and then it
/// drops out all at once. `--dither on` thins such artwork smoothly instead.
fn lighten_toward_white(img: &mut GrayImage, percent: u8) {
    let percent = u32::from(percent.min(100));
    for px in img.pixels_mut() {
        let v = u32::from(px.0[0]);
        px.0[0] = (v + (255 - v) * percent / 100) as u8;
    }
}

/// The printer packs its raster data eight dots to a byte, so the configured
/// print width has to be a whole number of bytes.
fn check_print_width(cfg: &ImageSettings) -> Result<()> {
    if cfg.width == 0 || cfg.width % 8 != 0 {
        bail!(
            "image width must be a positive multiple of 8 (got {})",
            cfg.width
        );
    }
    Ok(())
}

/// The share of mid-tone pixels above which [`Dither::Auto`] dithers. Text and
/// line art sit around 5% (only their anti-aliased edges are gray), while
/// photographs and gradients are well above 80%, so anything in between is a
/// safe cut-off.
const MIDTONE_RATIO: f64 = 0.20;

/// Whether an image has enough gray in it that thresholding would wreck it.
fn has_midtones(img: &GrayImage) -> bool {
    let total = u64::from(img.width()) * u64::from(img.height());
    if total == 0 {
        return false;
    }
    let midtones = img.pixels().filter(|p| (24..232).contains(&p.0[0])).count() as f64;
    midtones / total as f64 > MIDTONE_RATIO
}

/// Composite an image onto a white background and convert it to grayscale.
///
/// Transparent areas would otherwise come out as solid black once the alpha
/// channel is dropped, so they're filled with paper white instead.
fn flatten_onto_white(img: &DynamicImage) -> GrayImage {
    let rgba = img.to_rgba8();
    let mut out = GrayImage::new(rgba.width(), rgba.height());
    for (x, y, px) in rgba.enumerate_pixels() {
        let alpha = u32::from(px[3]);
        let over = |c: u8| (u32::from(c) * alpha + 255 * (255 - alpha)) / 255;
        let luma = (299 * over(px[0]) + 587 * over(px[1]) + 114 * over(px[2])) / 1000;
        out.put_pixel(x, y, Luma([luma as u8]));
    }
    out
}

/// Send a prepared image to wherever the CLI flags point: a PNG file, stdout,
/// the raw ESC/POS stream, or the printer.
fn output_image(cli: &Cli, endpoint: &str, png: &[u8], cfg: &ImageSettings) -> Result<()> {
    if let Some(path) = cli.output.as_deref() {
        log::debug!("output: writing PNG to `{}`", path.display());
        fs::write(path, png).with_context(|| format!("writing image to `{}`", path.display()))?;
        println!("Wrote image to {}", path.display());
    } else if cli.stdout {
        log::debug!("output: image to stdout");
        emit_image_to_stdout(png)?;
    } else if cli.raw {
        log::debug!("output: raw ESC/POS byte stream to stdout");
        render_bit_image(ConsoleDriver::open(true), png, cfg)?;
    } else {
        log::debug!("output: image to printer `{endpoint}`");
        let (host, port) = parse_endpoint(endpoint)?;
        let driver = NetworkDriver::open(host, port, Some(Duration::from_secs(5)))
            .with_context(|| format!("connecting to printer at `{endpoint}`"))?;
        render_bit_image(driver, png, cfg)?;
    }

    Ok(())
}

/// Write a PNG to stdout: inline via the kitty graphics protocol when stdout is
/// a terminal, or as the raw PNG bytes when it's redirected, so the image can be
/// piped into another command (`receipter --stdout --imageText | receipter`) or
/// saved to a file.
fn emit_image_to_stdout(png: &[u8]) -> Result<()> {
    if std::io::stdout().is_terminal() {
        return print_kitty_image(png);
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(png).context("writing the image to stdout")?;
    out.flush()?;
    Ok(())
}

/// How many monospace characters fit across the printable width, from the image
/// settings. Used to shorten variable-length text (e.g. stop names) so it does
/// not overflow the printed image. Falls back to a typical monospace ratio if
/// the font can't be measured.
fn body_columns(cfg: &ImageSettings) -> usize {
    let margin = 8i32;
    let avail = (cfg.width as i32 - 2 * margin).max(0) as f32;
    let advance = monospace_advance(cfg).unwrap_or(cfg.font_size * 0.6);
    if advance <= 0.0 {
        return 32;
    }
    ((avail / advance).floor() as usize).max(8)
}

/// The per-character advance of the configured font at the body size, measured
/// from a run of `M`s (the font is monospace, so any glyph gives the same
/// width).
fn monospace_advance(cfg: &ImageSettings) -> Option<f32> {
    let bytes = fs::read(&cfg.font).ok()?;
    let font = FontVec::try_from_vec(bytes).ok()?;
    let (w, _) = text_size(PxScale::from(cfg.font_size), &font, "MMMMMMMMMM");
    (w > 0).then(|| w as f32 / 10.0)
}

/// Whether the configured font actually contains the train and bus icon glyphs.
/// When it doesn't, the Transport section uses `(train)`/`(bus)` labels instead
/// so nothing prints as a "tofu" box.
fn font_has_icons(cfg: &ImageSettings) -> bool {
    fs::read(&cfg.font)
        .ok()
        .and_then(|b| FontVec::try_from_vec(b).ok())
        .map(|f| f.glyph_id(transport::TRAIN_ICON).0 != 0 && f.glyph_id(transport::BUS_ICON).0 != 0)
        .unwrap_or(false)
}

/// Factor by which a leading icon glyph is enlarged relative to the body text.
const ICON_SCALE: f32 = 1.5;

/// Nerd Font icons live in the Unicode Private Use Areas; a body line that
/// starts with one is drawn with its icon enlarged.
fn is_icon(c: char) -> bool {
    ('\u{E000}'..='\u{F8FF}').contains(&c)
        || ('\u{F0000}'..='\u{FFFFD}').contains(&c)
        || ('\u{100000}'..='\u{10FFFD}').contains(&c)
}

/// Render the weather report to a monochrome PNG suitable for the printer.
///
/// The image is black text on a white background because the printer treats any
/// pixel with a grayscale value <= 128 as a black dot.
fn build_report_png(
    address: &str,
    date: &str,
    report: &str,
    cfg: &ImageSettings,
) -> Result<Vec<u8>> {
    check_print_width(cfg)?;

    let font_bytes = fs::read(&cfg.font)
        .with_context(|| format!("reading font file `{}`", cfg.font.display()))?;
    let font = FontVec::try_from_vec(font_bytes)
        .map_err(|e| anyhow!("`{}` is not a valid font: {e}", cfg.font.display()))?;

    let body_px = cfg.font_size;
    let title_px = cfg.font_size * 1.6;
    let margin: i32 = 8;
    let avail_width = (cfg.width as i32 - 2 * margin) as f32;

    // Shrink a header line's font size so it fits within the printable width.
    let fit_px = |text: &str, desired: f32| -> f32 {
        if text.is_empty() {
            return desired;
        }
        let (w, _) = text_size(PxScale::from(desired), &font, text);
        if w == 0 || (w as f32) <= avail_width {
            desired
        } else {
            desired * avail_width / w as f32
        }
    };

    enum Align {
        Left,
        Center,
    }
    struct Row {
        text: String,
        px: f32,
        align: Align,
    }

    let address_text = with_font_fallback(&font, address);
    let date_text = with_font_fallback(&font, date);

    let mut rows = vec![
        Row {
            px: fit_px(&address_text, title_px),
            text: address_text,
            align: Align::Center,
        },
        Row {
            px: fit_px(&date_text, body_px),
            text: date_text,
            align: Align::Center,
        },
        Row {
            text: String::new(),
            px: body_px,
            align: Align::Left,
        }, // spacer
    ];
    for line in report.lines() {
        rows.push(Row {
            text: with_font_fallback(&font, line),
            px: body_px,
            align: Align::Left,
        });
    }

    // Tight line height: font ascent-to-descent plus a small, fixed leading.
    let line_height = |px: f32| -> i32 {
        let s = font.as_scaled(PxScale::from(px));
        (s.ascent() - s.descent()).ceil() as i32 + 2
    };

    let total_height = margin * 2 + rows.iter().map(|r| line_height(r.px)).sum::<i32>();
    let width = cfg.width;
    let height = total_height.max(1) as u32;

    let mut img = GrayImage::from_pixel(width, height, Luma([255u8]));
    let black = Luma([0u8]);

    let mut y = margin;
    for row in &rows {
        let lh = line_height(row.px);
        if !row.text.is_empty() {
            let scale = PxScale::from(row.px);
            match row.align {
                Align::Center => {
                    let (text_w, _) = text_size(scale, &font, &row.text);
                    let x = ((width as i32 - text_w as i32) / 2).max(0);
                    draw_text_mut(&mut img, black, x, y, scale, &font, &row.text);
                }
                Align::Left => {
                    let first = row.text.chars().next().unwrap();
                    if is_icon(first) {
                        // Draw the icon enlarged, sharing the body baseline, then
                        // the rest of the line at the normal size beside it.
                        let icon_scale = PxScale::from(row.px * ICON_SCALE);
                        let icon = first.to_string();
                        let baseline_shift =
                            font.as_scaled(scale).ascent() - font.as_scaled(icon_scale).ascent();
                        let y_icon = y + baseline_shift.round() as i32;
                        draw_text_mut(&mut img, black, margin, y_icon, icon_scale, &font, &icon);

                        let (icon_w, _) = text_size(icon_scale, &font, &icon);
                        let rest: String = row.text.chars().skip(1).collect();
                        draw_text_mut(
                            &mut img,
                            black,
                            margin + icon_w as i32,
                            y,
                            scale,
                            &font,
                            &rest,
                        );
                    } else {
                        draw_text_mut(&mut img, black, margin, y, scale, &font, &row.text);
                    }
                }
            }
        }
        y += lh;
    }

    let mut png = Vec::new();
    DynamicImage::ImageLuma8(img)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .context("encoding the weather image to PNG")?;

    Ok(png)
}

/// Display a PNG inline using the kitty graphics protocol: base64-encode the
/// image and transmit it in <=4096-byte chunks. Works in kitty and terminals
/// that implement the same protocol (e.g. WezTerm, Ghostty); other terminals
/// will just show the raw escape codes.
fn print_kitty_image(png: &[u8]) -> Result<()> {
    let encoded = base64_encode(png);
    let bytes = encoded.as_bytes();
    if bytes.is_empty() {
        return Ok(());
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut chunks = bytes.chunks(4096).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let m = if chunks.peek().is_some() { 1 } else { 0 };
        if first {
            // f=100: PNG payload; a=T: transmit and display immediately.
            write!(out, "\x1b_Gf=100,a=T,m={m};")?;
            first = false;
        } else {
            write!(out, "\x1b_Gm={m};")?;
        }
        out.write_all(chunk)?;
        write!(out, "\x1b\\")?;
    }
    writeln!(out)?; // leave the cursor on the line below the image
    out.flush()?;
    Ok(())
}

/// Minimal standard base64 encoder (with padding), to avoid pulling in a
/// dependency solely for the kitty image transport.
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Replace characters the font cannot render (which would otherwise print as
/// "tofu" boxes) with a look-alike glyph the font has, or `?` as a last resort.
fn with_font_fallback(font: &FontVec, text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == ' ' || font.glyph_id(c).0 != 0 {
                return c;
            }
            let alt = match c {
                '\u{2015}' => '\u{2500}', // horizontal bar -> box-drawing horizontal
                _ => '?',
            };
            if font.glyph_id(alt).0 != 0 {
                alt
            } else {
                '?'
            }
        })
        .collect()
}

/// Transliterate the UTF-8 weather report to plain ASCII for the printer.
///
/// The printer interprets bytes through a single-byte code page, so the
/// multi-byte characters wttr.in uses (`°`, box-drawing dashes, smart quotes,
/// wind-direction arrows) would otherwise print as garbage. stdout keeps the
/// original Unicode; only the printed output is sanitized.
fn sanitize_for_printer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            // Wind-direction arrows -> compass bearings.
            '\u{2191}' => out.push('N'),
            '\u{2193}' => out.push('S'),
            '\u{2190}' => out.push('W'),
            '\u{2192}' => out.push('E'),
            '\u{2196}' => out.push_str("NW"),
            '\u{2197}' => out.push_str("NE"),
            '\u{2198}' => out.push_str("SE"),
            '\u{2199}' => out.push_str("SW"),
            // Dashes / bars -> hyphen.
            '\u{2015}' | '\u{2014}' | '\u{2013}' => out.push('-'),
            // Smart quotes -> ASCII quotes.
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201C}' | '\u{201D}' => out.push('"'),
            // Degree sign: keep the reading, drop the symbol ("26 °C" -> "26 C").
            '\u{00B0}' => {}
            c if c.is_ascii() => out.push(c),
            // Anything else: keep column alignment without emitting garbage.
            _ => out.push(' '),
        }
    }
    out
}

/// Split an "host:port" endpoint into its parts.
fn parse_endpoint(endpoint: &str) -> Result<(&str, u16)> {
    let (host, port) = endpoint
        .rsplit_once(':')
        .with_context(|| format!("endpoint `{endpoint}` must be in the form `host:port`"))?;

    if host.is_empty() {
        bail!("endpoint `{endpoint}` is missing a host");
    }

    let port = port
        .parse::<u16>()
        .with_context(|| format!("endpoint `{endpoint}` has an invalid port `{port}`"))?;

    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_full_width_png() {
        let cfg = ImageSettings::default();
        let report = "      \\   /     Sunny\n       .-.      26 \u{00b0}C\n    \u{2015} (   ) \u{2015}   \u{2199} 22 km/h\n";

        let png = build_report_png(
            "11 Example Street, Townsville, AB1 2CD",
            "Wednesday, 15 July 2026",
            report,
            &cfg,
        )
        .expect("png should build");
        assert!(!png.is_empty());

        let decoded = image::load_from_memory(&png).expect("png should decode");
        assert_eq!(decoded.width(), cfg.width);

        // Leave a preview on disk for manual inspection.
        let _ = fs::create_dir_all("target");
        fs::write("target/weather-preview.png", &png).unwrap();
    }

    #[test]
    fn a_piped_receipt_survives_the_round_trip() {
        // `receipter --stdout --imageText | receipter` re-prints the receipt, so
        // the PNG must come back at the print width and print identically: it is
        // already the right size, and `Dither::Auto` leaves black-and-white
        // artwork like this alone.
        let cfg = ImageSettings::default();
        let png = build_report_png(
            "11 Example Street",
            "Wednesday, 15 July 2026",
            "Sunny\n",
            &cfg,
        )
        .expect("png should build");
        let original = image::load_from_memory(&png).unwrap().to_luma8();

        let prepared =
            prepare_image_for_print(&png, &cfg, Dither::Auto, 0).expect("image should prepare");
        let prepared = image::load_from_memory(&prepared).unwrap().to_luma8();

        assert_eq!(prepared.dimensions(), original.dimensions());
        // The printer inks every pixel <= 128, so compare the dots themselves.
        let inked = |p: &Luma<u8>| p.0[0] <= 128;
        assert!(
            original
                .pixels()
                .zip(prepared.pixels())
                .all(|(before, after)| inked(before) == inked(after)),
            "the round-tripped receipt should print dot for dot"
        );
    }

    #[test]
    fn scales_an_image_to_the_print_width_in_black_and_white() {
        let cfg = ImageSettings::default();

        // A 4x2 mid-gray image with a transparent corner: too small, too gray,
        // and too transparent to print directly.
        let mut src = image::RgbaImage::from_pixel(4, 2, image::Rgba([128, 128, 128, 255]));
        src.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(src)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();

        let prepared =
            prepare_image_for_print(&bytes, &cfg, Dither::Auto, 0).expect("image should prepare");
        let out = image::load_from_memory(&prepared).unwrap().to_luma8();

        assert_eq!(out.width(), cfg.width);
        assert_eq!(out.height(), cfg.width / 2); // 4x2 keeps its 2:1 aspect ratio
        assert!(out.pixels().all(|p| p.0[0] == 0 || p.0[0] == 255));
        // The transparent corner became paper white, not solid black.
        assert_eq!(out.get_pixel(0, 0).0[0], 255);
        // All that gray is dithered rather than thresholded away, so the result
        // is a mix of inked and blank dots.
        assert!(out.pixels().any(|p| p.0[0] == 0));
        assert!(out.pixels().any(|p| p.0[0] == 255));

        // Forcing it off thresholds instead: mid-gray inks every dot.
        let flat =
            prepare_image_for_print(&bytes, &cfg, Dither::Off, 0).expect("image should prepare");
        let flat = image::load_from_memory(&flat).unwrap().to_luma8();
        assert!(
            flat.pixels().filter(|p| p.0[0] == 0).count()
                > (flat.width() * flat.height() / 2) as usize
        );
    }

    #[test]
    fn lightening_lays_down_fewer_dots() {
        let cfg = ImageSettings::default();
        let src = image::RgbaImage::from_pixel(64, 64, image::Rgba([128, 128, 128, 255]));
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(src)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();

        let inked = |percent: u8| {
            let png = prepare_image_for_print(&bytes, &cfg, Dither::On, percent)
                .expect("image should prepare");
            let img = image::load_from_memory(&png).unwrap().to_luma8();
            let total = (img.width() * img.height()) as f64;
            img.pixels().filter(|p| p.0[0] == 0).count() as f64 / total
        };

        // Dithering inks the tone's share of the dots, and lightening scales
        // that share down by the same percentage.
        let full = inked(0);
        assert!(
            (full - 0.5).abs() < 0.02,
            "mid-gray should ink ~50%: {full}"
        );
        for percent in [30, 60] {
            let expected = full * (1.0 - f64::from(percent) / 100.0);
            let got = inked(percent);
            assert!(
                (got - expected).abs() < 0.02,
                "--lighten {percent} should ink ~{expected:.2} of the dots, inked {got:.2}"
            );
        }
        assert_eq!(inked(100), 0.0, "--lighten 100 should ink nothing");
    }

    #[test]
    fn warns_instead_of_silently_printing_a_blank_receipt() {
        // Thresholded artwork drops out all at once past 50%, so the blank-page
        // check has to catch it.
        let cfg = ImageSettings::default();
        let png = build_report_png(
            "11 Example Street",
            "Wednesday, 15 July 2026",
            "Sunny\n",
            &cfg,
        )
        .expect("png should build");

        let blanked = prepare_image_for_print(&png, &cfg, Dither::Off, 60).unwrap();
        let blanked = image::load_from_memory(&blanked).unwrap().to_luma8();
        assert!(blanked.pixels().all(|p| p.0[0] != 0));

        // Dithering the same thing keeps it faint but present.
        let thinned = prepare_image_for_print(&png, &cfg, Dither::On, 60).unwrap();
        let thinned = image::load_from_memory(&thinned).unwrap().to_luma8();
        assert!(thinned.pixels().any(|p| p.0[0] == 0));
    }

    #[test]
    fn rejects_input_that_is_not_an_image() {
        let err =
            prepare_image_for_print(b"not an image", &ImageSettings::default(), Dither::Auto, 0)
                .expect_err("garbage should not decode");
        assert!(err.to_string().contains("decoding the image"));
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors, plus the classic "Man"/"Ma"/"M" padding cases.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"M"), "TQ==");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0xff, 0xff, 0xff]), "////");
    }
}
