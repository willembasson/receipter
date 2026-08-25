use std::path::PathBuf;

use chrono::{NaiveDate, NaiveTime};
use clap::{ArgGroup, Parser};

/// Fetch the current weather from wttr.in and print it on an ESC/POS printer.
#[derive(Debug, Parser)]
#[command(version, about)]
#[command(group(ArgGroup::new("mode").args(["stdout", "raw"]).multiple(false)))]
pub struct Cli {
    /// Path to the settings file.
    #[arg(short, long, default_value = "settings.toml")]
    pub config: PathBuf,

    /// Increase logging verbosity: -v for debug, -vv for trace. Logs go to
    /// stderr, so they don't interfere with --stdout output.
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Location to look up the weather for (overrides the settings file).
    #[arg(short, long)]
    pub location: Option<String>,

    /// Printer endpoint as "host:port" (overrides the settings file).
    #[arg(short, long)]
    pub endpoint: Option<String>,

    /// Template file that controls which sections appear and in what order.
    #[arg(
        short = 'T',
        long,
        value_name = "FILE",
        default_value = "templates/default.tpl"
    )]
    pub template: PathBuf,

    /// Number of forecast days to request from wttr.in (0 = current only).
    #[arg(short, long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=2))]
    pub days: u8,

    /// Date to print as YYYYMMDD (default: today). Weather is available for
    /// today and the next couple of forecast days.
    #[arg(short = 'D', long, value_name = "YYYYMMDD", value_parser = parse_date)]
    pub date: Option<NaiveDate>,

    /// Shorthand for the date of tomorrow (conflicts with --date).
    #[arg(short = 't', long, conflicts_with = "date")]
    pub tomorrow: bool,

    /// Look up transport departures at this time (HHMM) instead of now. Enables
    /// the transport section for any --date, using the scheduled timetable.
    #[arg(long, value_name = "HHMM", value_parser = parse_time)]
    pub at: Option<NaiveTime>,

    /// List nearby stations and bus stops with their codes (to fill in
    /// `station_codes` / `bus_stop_codes`), then exit.
    #[arg(long)]
    pub list_stops: bool,

    /// Always include the Bin day section, showing the next collection even when
    /// it isn't the eve of one (which is when it normally appears).
    #[arg(long)]
    pub bins: bool,

    /// List every bin collection for the configured property, with the council's
    /// property id (to fill in `property_id`), then exit.
    #[arg(long)]
    pub list_bins: bool,

    /// Ignore cached freshness and refetch live data (still stored to the cache).
    #[arg(long)]
    pub refresh: bool,

    /// Disable the SQLite cache entirely for this run.
    #[arg(long)]
    pub no_cache: bool,

    /// Print the weather text to stdout instead of sending it to the printer.
    #[arg(long)]
    pub stdout: bool,

    /// Emit the raw ESC/POS byte stream to stdout (ConsoleDriver, for debugging).
    #[arg(long)]
    pub raw: bool,

    /// Render the report as an image so full Unicode (arrows, degrees, icons) prints.
    /// This is the default; combine with --stdout to display it inline in a
    /// kitty-compatible terminal.
    #[arg(long = "imageText")]
    pub image_text: bool,

    /// Print as plain ESC/POS text instead of the default image rendering
    /// (loses icons and other non-ASCII glyphs).
    #[arg(long)]
    pub text: bool,

    /// Write the rendered image to a PNG file instead of printing (preview mode).
    #[arg(short, long, value_name = "FILE", conflicts_with_all = ["stdout", "raw"])]
    pub output: Option<PathBuf>,

    /// How to reduce a printed image to the printer's pure black and white:
    /// `auto` dithers photographs but keeps an already black-and-white image
    /// (such as a receipt) crisp, `on` always dithers, `off` always thresholds.
    #[arg(long, value_name = "MODE", value_enum, default_value_t = Dither::Auto)]
    pub dither: Dither,

    /// Lighten a printed image by this percentage before it is reduced to black
    /// and white (0 = unchanged, 100 = blank). Thermal paper spreads each dot,
    /// so a dithered photo usually prints darker than it looks on screen; try
    /// `--lighten 30` to lay down 30% fewer dots.
    #[arg(long, value_name = "PERCENT", default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub lighten: u8,

    /// Print this image instead of the report (PNG, JPEG, GIF, BMP or WebP).
    /// Use `-` to read it from stdin; an image piped in on stdin is picked up
    /// automatically.
    #[arg(value_name = "IMAGE")]
    pub image: Option<PathBuf>,
}

/// How an image is reduced to the two levels the printer can actually put on
/// paper. See [`crate::render::prepare_image_for_print`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Dither {
    /// Dither only images with enough mid-tones to need it.
    Auto,
    /// Always dither.
    On,
    /// Never dither; threshold at mid-gray.
    Off,
}

fn parse_date(s: &str) -> std::result::Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s, "%Y%m%d")
        .map_err(|_| format!("`{s}` is not a valid date, expected YYYYMMDD"))
}

fn parse_time(s: &str) -> std::result::Result<NaiveTime, String> {
    NaiveTime::parse_from_str(s, "%H%M")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
        .map_err(|_| format!("`{s}` is not a valid time, expected HHMM"))
}
