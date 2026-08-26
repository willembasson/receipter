use std::fs;

use anyhow::{Context, Result, bail};
use chrono::Local;
use clap::Parser;
use escpos::driver::ConsoleDriver;

mod bins;
mod cache;
mod calendar;
mod cli;
mod render;
mod settings;
mod template;
mod transport;
mod weather;

use calendar::Meeting;
use cli::Cli;
use settings::load_settings;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let mut settings = load_settings(&cli.config)?;

    // Read stdin early (before it's consumed) when no explicit image arg is given.
    let stdin_bytes = if cli.image.is_some() {
        None
    } else {
        render::read_stdin_bytes()?
    };

    // Determine the template: piped text on stdin overrides the file.
    let tpl = match &stdin_bytes {
        Some(bytes) if !render::looks_like_image(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            log::debug!("using piped stdin as template ({} bytes)", bytes.len());
            template::from_str(&text)?
        }
        _ => template::load(&cli.template)?,
    };

    if let Some(w) = tpl.overrides.width {
        settings.image.width = w;
    }
    if let Some(f) = tpl.overrides.font.clone() {
        settings.image.font = f;
    }
    if let Some(s) = tpl.overrides.font_size {
        settings.image.font_size = s;
    }

    let location = cli
        .location
        .clone()
        .or_else(|| tpl.overrides.location.clone())
        .unwrap_or_else(|| settings.location.clone());
    let endpoint = cli
        .endpoint
        .clone()
        .or_else(|| tpl.overrides.endpoint.clone())
        .unwrap_or_else(|| settings.endpoint.clone());
    let days = cli.days.or(tpl.overrides.days).unwrap_or(0);
    let address = tpl
        .overrides
        .address
        .clone()
        .unwrap_or_else(|| settings.address.clone());
    log::debug!("loaded settings from `{}`", cli.config.display());

    // Explicit image argument, or piped image data on stdin.
    let image_bytes = if cli.image.is_some() {
        render::read_input_image(cli.image.as_deref())?
    } else {
        stdin_bytes.filter(|b| render::looks_like_image(b))
    };

    if let Some(bytes) = image_bytes {
        log::debug!("input: {} byte(s) of image data", bytes.len());
        let png =
            render::prepare_image_for_print(&bytes, &settings.image, cli.dither, cli.lighten)?;
        return render::output_image(
            cli.output.as_deref(),
            cli.stdout,
            cli.raw,
            &endpoint,
            &png,
            &settings.image,
        );
    }

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

    if cli.list_bins {
        match &settings.bins {
            Some(cfg) => {
                print!("{}", bins::list_bins(cfg, &address, today, cache).await?);
                return Ok(());
            }
            None => bail!("--list-bins needs a [bins] section with a `council`"),
        }
    }

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

    let when = cli.at.map(|t| target_date.and_time(t));
    let columns = render::body_columns(&settings.image);
    let icons = !cli.stdout && !cli.raw && !cli.text && render::font_has_icons(&settings.image);

    let weather_text = weather::weather_report(cache, &location, days, target_date, today).await?;

    let calendars_text = if settings.calendars.is_empty() {
        None
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
        let meetings = calendar::meetings_on(&settings.calendars, target_date, cache).await;
        Some(format_meetings(heading, &meetings))
    };

    let transport_text = match &settings.transport {
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
                    Some(format!("{heading}\n\n{}", body.trim_end()))
                }
                Err(e) => {
                    eprintln!("warning: could not load transport info: {e:#}");
                    None
                }
            }
        }
        _ => None,
    };

    let bins_text = match &settings.bins {
        Some(cfg) if target_date >= today => {
            log::debug!("checking bin collections ({} council)", cfg.council);
            let found = bins::bin_day(
                cfg,
                &address,
                target_date,
                today,
                cache,
                bins::Layout { columns, icons },
                cli.bins,
            )
            .await;
            match found {
                Ok(Some((due, body))) => {
                    let heading = match (due - target_date).num_days() {
                        0 => "Bin day today".to_string(),
                        1 => "Bin day tomorrow".to_string(),
                        n => format!("Bin day in {n} days"),
                    };
                    Some(format!("{heading}\n\n{}", body.trim_end()))
                }
                Ok(None) => {
                    if cli.bins {
                        eprintln!(
                            "warning: --bins: no upcoming collections found for this property"
                        );
                    }
                    None
                }
                Err(e) => {
                    eprintln!("warning: could not load bin days: {e:#}");
                    None
                }
            }
        }
        _ => {
            if cli.bins {
                let why = if settings.bins.is_none() {
                    "no [bins] section in the settings file"
                } else {
                    "bin collections are only known for today and future dates"
                };
                eprintln!("warning: --bins ignored: {why}");
            }
            None
        }
    };

    let mut segments = template::assemble(&tpl.elements, &|name| match name {
        "address" => Some(address.clone()),
        "date" => Some(date.clone()),
        "weather" => Some(weather_text.clone()),
        "calendars" => calendars_text.clone(),
        "transport" => transport_text.clone(),
        "bins" => bins_text.clone(),
        other => {
            eprintln!("warning: unknown template block `{{{{{other}}}}}`");
            None
        }
    });

    let needs_images =
        cli.output.is_some() || cli.image_text || (!cli.stdout && !cli.raw && !cli.text);
    if needs_images {
        render::resolve_images(&mut segments).await?;
    }

    output_report(&cli, &endpoint, &segments, &settings.image)?;

    Ok(())
}

fn output_report(
    cli: &Cli,
    endpoint: &str,
    segments: &[template::Segment],
    image_cfg: &settings::ImageSettings,
) -> Result<()> {
    if let Some(path) = cli.output.as_deref() {
        log::debug!("output: writing PNG preview to `{}`", path.display());
        let png = render::build_report_png(segments, image_cfg, cli.dither, cli.lighten)?;
        fs::write(path, &png).with_context(|| format!("writing image to `{}`", path.display()))?;
        println!("Wrote weather image to {}", path.display());
    } else if cli.stdout {
        if cli.image_text {
            log::debug!("output: inline image to stdout");
            let png = render::build_report_png(segments, image_cfg, cli.dither, cli.lighten)?;
            render::emit_image_to_stdout(&png)?;
        } else {
            log::debug!("output: report text to stdout");
            print!("{}", template::segments_to_text(segments));
        }
    } else if cli.raw {
        log::debug!("output: raw ESC/POS byte stream to stdout");
        let text = template::segments_to_text(segments);
        render::render(ConsoleDriver::open(true), &text)?;
    } else if cli.text {
        log::debug!("output: plain-text ESC/POS to printer `{endpoint}`");
        let text = template::segments_to_text(segments);
        render::print_report(endpoint, &text)?;
    } else {
        log::debug!("output: image to printer `{endpoint}`");
        render::print_image(endpoint, segments, image_cfg, cli.dither, cli.lighten)?;
    }
    Ok(())
}

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

fn format_meetings(heading: &str, meetings: &[Meeting]) -> String {
    let body = if meetings.is_empty() {
        "No meetings.".to_string()
    } else {
        meetings
            .iter()
            .map(Meeting::line)
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!("{heading}\n\n{body}")
}
