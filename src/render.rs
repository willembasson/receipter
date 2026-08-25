use std::fs;
use std::io::{Cursor, IsTerminal, Read, Write};
use std::path::Path;
use std::time::Duration;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use anyhow::{Context, Result, anyhow, bail};
use escpos::driver::{ConsoleDriver, Driver, NetworkDriver};
use escpos::printer::Printer;
use escpos::printer_options::PrinterOptions;
use escpos::utils::*;
use image::{DynamicImage, GrayImage, Luma};
use imageproc::drawing::{draw_text_mut, text_size};

use crate::cli::Dither;
use crate::settings::ImageSettings;
use crate::{bins, transport};

/// Vertical line spacing (in dots) used for the printed receipt.
const PRINT_LINE_SPACING: u8 = 24;

/// Factor by which a leading icon glyph is enlarged relative to the body text.
const ICON_SCALE: f32 = 1.5;

/// The share of mid-tone pixels above which [`Dither::Auto`] dithers.
const MIDTONE_RATIO: f64 = 0.20;

/// Open the network printer and send it a formatted weather receipt.
pub fn print_report(endpoint: &str, address: &str, date: &str, report: &str) -> Result<()> {
    let (host, port) = parse_endpoint(endpoint)?;

    let driver = NetworkDriver::open(host, port, Some(Duration::from_secs(5)))
        .with_context(|| format!("connecting to printer at `{endpoint}`"))?;

    render(driver, address, date, report)
}

/// Build the weather receipt and send it to any ESC/POS driver.
pub fn render<D: Driver>(driver: D, address: &str, date: &str, report: &str) -> Result<()> {
    let mut printer = Printer::new(driver, Protocol::default(), Some(PrinterOptions::default()));
    printer.init()?;

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

/// Open the network printer and print the weather report as an image.
pub fn print_image(
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
pub fn render_bit_image<D: Driver>(driver: D, png: &[u8], cfg: &ImageSettings) -> Result<()> {
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
pub fn read_input_image(path: Option<&Path>) -> Result<Option<Vec<u8>>> {
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

/// Turn an arbitrary image into a PNG the printer can reproduce.
pub fn prepare_image_for_print(
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
        for px in gray.pixels_mut() {
            px.0[0] = if px.0[0] <= 128 { 0 } else { 255 };
        }
    }

    if lighten > 0 && gray.pixels().all(|p| p.0[0] != 0) {
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

fn lighten_toward_white(img: &mut GrayImage, percent: u8) {
    let percent = u32::from(percent.min(100));
    for px in img.pixels_mut() {
        let v = u32::from(px.0[0]);
        px.0[0] = (v + (255 - v) * percent / 100) as u8;
    }
}

fn check_print_width(cfg: &ImageSettings) -> Result<()> {
    if cfg.width == 0 || !cfg.width.is_multiple_of(8) {
        bail!(
            "image width must be a positive multiple of 8 (got {})",
            cfg.width
        );
    }
    Ok(())
}

fn has_midtones(img: &GrayImage) -> bool {
    let total = u64::from(img.width()) * u64::from(img.height());
    if total == 0 {
        return false;
    }
    let midtones = img.pixels().filter(|p| (24..232).contains(&p.0[0])).count() as f64;
    midtones / total as f64 > MIDTONE_RATIO
}

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

/// Send a prepared image to wherever the CLI flags point.
pub fn output_image(
    output_path: Option<&Path>,
    stdout: bool,
    raw: bool,
    endpoint: &str,
    png: &[u8],
    cfg: &ImageSettings,
) -> Result<()> {
    if let Some(path) = output_path {
        log::debug!("output: writing PNG to `{}`", path.display());
        fs::write(path, png).with_context(|| format!("writing image to `{}`", path.display()))?;
        println!("Wrote image to {}", path.display());
    } else if stdout {
        log::debug!("output: image to stdout");
        emit_image_to_stdout(png)?;
    } else if raw {
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
/// a terminal, or as the raw PNG bytes when it's redirected.
pub fn emit_image_to_stdout(png: &[u8]) -> Result<()> {
    if std::io::stdout().is_terminal() {
        return print_kitty_image(png);
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(png).context("writing the image to stdout")?;
    out.flush()?;
    Ok(())
}

/// How many monospace characters fit across the printable width.
pub fn body_columns(cfg: &ImageSettings) -> usize {
    let margin = 8i32;
    let avail = (cfg.width as i32 - 2 * margin).max(0) as f32;
    let advance = monospace_advance(cfg).unwrap_or(cfg.font_size * 0.6);
    if advance <= 0.0 {
        return 32;
    }
    ((avail / advance).floor() as usize).max(8)
}

fn monospace_advance(cfg: &ImageSettings) -> Option<f32> {
    let bytes = fs::read(&cfg.font).ok()?;
    let font = FontVec::try_from_vec(bytes).ok()?;
    let (w, _) = text_size(PxScale::from(cfg.font_size), &font, "MMMMMMMMMM");
    (w > 0).then(|| w as f32 / 10.0)
}

/// Whether the configured font contains the icon glyphs used by Transport and Bin sections.
pub fn font_has_icons(cfg: &ImageSettings) -> bool {
    fs::read(&cfg.font)
        .ok()
        .and_then(|b| FontVec::try_from_vec(b).ok())
        .map(|f| {
            f.glyph_id(transport::TRAIN_ICON).0 != 0
                && f.glyph_id(transport::BUS_ICON).0 != 0
                && f.glyph_id(bins::BIN_ICON).0 != 0
        })
        .unwrap_or(false)
}

fn is_icon(c: char) -> bool {
    ('\u{E000}'..='\u{F8FF}').contains(&c)
        || ('\u{F0000}'..='\u{FFFFD}').contains(&c)
        || ('\u{100000}'..='\u{10FFFD}').contains(&c)
}

/// Render the weather report to a monochrome PNG suitable for the printer.
pub fn build_report_png(
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
        },
    ];
    for line in report.lines() {
        rows.push(Row {
            text: with_font_fallback(&font, line),
            px: body_px,
            align: Align::Left,
        });
    }

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
            write!(out, "\x1b_Gf=100,a=T,m={m};")?;
            first = false;
        } else {
            write!(out, "\x1b_Gm={m};")?;
        }
        out.write_all(chunk)?;
        write!(out, "\x1b\\")?;
    }
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

pub fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
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

fn with_font_fallback(font: &FontVec, text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == ' ' || font.glyph_id(c).0 != 0 {
                return c;
            }
            let alt = match c {
                '\u{2015}' => '\u{2500}',
                _ => '?',
            };
            if font.glyph_id(alt).0 != 0 { alt } else { '?' }
        })
        .collect()
}

fn sanitize_for_printer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{2191}' => out.push('N'),
            '\u{2193}' => out.push('S'),
            '\u{2190}' => out.push('W'),
            '\u{2192}' => out.push('E'),
            '\u{2196}' => out.push_str("NW"),
            '\u{2197}' => out.push_str("NE"),
            '\u{2198}' => out.push_str("SE"),
            '\u{2199}' => out.push_str("SW"),
            '\u{2015}' | '\u{2014}' | '\u{2013}' => out.push('-'),
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201C}' | '\u{201D}' => out.push('"'),
            '\u{00B0}' => {}
            c if c.is_ascii() => out.push(c),
            _ => out.push(' '),
        }
    }
    out
}

pub fn parse_endpoint(endpoint: &str) -> Result<(&str, u16)> {
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
    use crate::settings::ImageSettings;

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

        let _ = fs::create_dir_all("target");
        fs::write("target/weather-preview.png", &png).unwrap();
    }

    #[test]
    fn a_piped_receipt_survives_the_round_trip() {
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
        assert_eq!(out.height(), cfg.width / 2);
        assert!(out.pixels().all(|p| p.0[0] == 0 || p.0[0] == 255));
        assert_eq!(out.get_pixel(0, 0).0[0], 255);
        assert!(out.pixels().any(|p| p.0[0] == 0));
        assert!(out.pixels().any(|p| p.0[0] == 255));

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
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"M"), "TQ==");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0xff, 0xff, 0xff]), "////");
    }
}
