use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::Deserialize;

use crate::cache;

/// Weather for `target`, using the cache for freshness and history:
/// - today  -> wttr.in ASCII current conditions (cached, TTL);
/// - future -> wttr.in JSON forecast for that date (cached per date);
/// - past   -> whatever was previously stored for that date, if anything.
pub async fn weather_report(
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

async fn forecast_report(
    cache: Option<&cache::Cache>,
    location: &str,
    target: NaiveDate,
) -> Result<Option<String>> {
    let key = weather_key_forecast(location, target);
    if let Some(c) = cache
        && let Some(hit) = c.get_fresh("forecast", &key, Some(target), false)?
    {
        return Ok(Some(hit));
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

async fn fetch_weather(location: &str, days: u8) -> Result<String> {
    let url = format!("https://wttr.in/{location}?{days}T");

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

#[derive(Deserialize)]
struct WttrForecast {
    weather: Vec<ForecastDay>,
}

#[derive(Deserialize)]
struct ForecastDay {
    date: String,
    #[serde(rename = "maxtempC")]
    max_temp_c: String,
    #[serde(rename = "mintempC")]
    min_temp_c: String,
    hourly: Vec<ForecastHour>,
}

#[derive(Deserialize)]
struct ForecastHour {
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

fn format_forecast(location: &str, day: &ForecastDay) -> String {
    let mut out = format!("Weather forecast: {location}\n\n");
    out.push_str(&format!(
        "High {} C  /  Low {} C\n\n",
        day.max_temp_c, day.min_temp_c
    ));

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
