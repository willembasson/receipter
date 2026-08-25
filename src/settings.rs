use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::{bins, calendar, transport};

/// Values loaded from the settings file. CLI arguments take precedence.
#[derive(Debug, Deserialize)]
pub struct Settings {
    pub endpoint: String,
    pub location: String,
    pub address: String,
    /// Zero or more Google Calendar "secret iCal" sources. When set, today's
    /// meetings are printed below the weather.
    #[serde(default)]
    pub calendars: Vec<calendar::CalendarSource>,
    /// Optional TransportAPI config. When set, the nearest stations and bus
    /// stops with their next departures are printed below the meetings.
    #[serde(default)]
    pub transport: Option<transport::TransportSettings>,
    /// Optional council bin-collection config. When set, the bins going out are
    /// printed on the eve of a collection (and on no other day).
    #[serde(default)]
    pub bins: Option<bins::BinSettings>,
    #[serde(default)]
    pub cache: CacheSettings,
    #[serde(default)]
    pub image: ImageSettings,
}

/// Settings that control the SQLite cache for external calls.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct CacheSettings {
    /// Path to the SQLite database file.
    pub path: PathBuf,
    /// How long a live result stays fresh before being refetched, in minutes.
    pub ttl_minutes: i64,
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
pub struct ImageSettings {
    /// Print width in dots (80mm = 576, 58mm = 384). Must be a multiple of 8.
    pub width: u32,
    /// Path to the TrueType/OpenType font used to rasterize the report.
    pub font: PathBuf,
    /// Body font size, in pixels.
    pub font_size: f32,
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

pub fn load_settings(path: &Path) -> Result<Settings> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading settings file `{}`", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing settings file `{}`", path.display()))
}
