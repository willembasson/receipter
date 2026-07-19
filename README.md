# thermy

A small Rust CLI that prints a daily "morning receipt" to an 80mm ESC/POS
network thermal printer. Each receipt has three parts:

1. **Header** — your address and the report's date.
2. **Weather** — current conditions (today) or a short forecast (future days)
   from [wttr.in](https://wttr.in).
3. **Calendar** — the day's meetings, pulled from one or more Google Calendar
   "secret iCal" feeds.
4. **Transport** _(optional)_ — the nearest train stations and bus stops with
   their next departures, from [TransportAPI](https://developer.transportapi.com).

Every external call (weather, geocoding, and transport) is cached in a local
SQLite database, which doubles as a history so past dates can be reprinted.

It can print directly to the printer, or preview the output to your terminal or
a PNG file.

## Installation

```sh
cargo build --release
```

The binary is written to `target/release/thermy`. A JetBrains Mono Nerd Font
is bundled under `fonts/` for the image renderer, so no system font install is
required.

## Quick start

```sh
# Print today's receipt to the printer configured in settings.toml
thermy

# Preview today's receipt in the terminal (no printer needed)
thermy --stdout

# Preview as an image (full Unicode: arrows, degree signs, icons)
thermy --output preview.png
```

## Configuration

Settings are read from `settings.toml` by default (override with `--config`).
That file is **git-ignored** because it holds secrets (TransportAPI keys and
private calendar URLs) — copy the tracked template to create your own:

```sh
cp settings.example.toml settings.toml
```

The available keys are:

```toml
# Network endpoint of the ESC/POS printer, as "host:port".
endpoint = "192.168.1.100:9100"

# Default location used to look up the weather on https://wttr.in.
location = "London"

# Address printed as the receipt header (alongside the date).
address = "1 Random road, London, SE1 1LS"

# Zero or more Google Calendar "secret iCal" sources. When set, the day's
# meetings are printed below the weather. Get each URL from Google Calendar ->
# Settings -> [your calendar] -> "Integrate calendar" -> "Secret address in
# iCal format". `name` is optional and, when given, is shown next to that
# calendar's meetings.
[[calendars]]
name = "Work"
ics_url = "https://calendar.google.com/calendar/ical/.../basic.ics"

[[calendars]]
name = "Family"
ics_url = "https://calendar.google.com/calendar/ical/.../basic.ics"

# Optional live public-transport departures via TransportAPI. When set, the
# nearest train stations and bus stops (with their next departures) are printed
# below the meetings, for today only. A free app_id/app_key is required:
# https://developer.transportapi.com. The search location defaults to the
# postcode in `address`; override with `postcode`, or explicit `lat`/`lon`.
[transport]
app_id = "your-app-id"
app_key = "your-app-key"
# postcode = "AB1 2CD"   # optional; defaults to the postcode in `address`
# lat = 51.5074          # optional; overrides the postcode lookup
# lon = -0.1278          # optional; overrides the postcode lookup
stations = 3             # nearest train stations (default 3)
bus_stops = 3            # nearest bus stops (default 3)
departures = 3           # departures per stop (default 3)
# Pin specific stops instead of searching by location (these replace the
# nearest-stop search for that mode). Train codes are CRS; bus codes are ATCO.
# station_codes = ["ABC", "XYZ"]
# bus_stop_codes = ["490000000A"]
# routes = ["12"]   # only show these bus routes/lines (all if empty)

# Optional on-disk cache/history for every external call (weather, geocoding,
# transport). Identical requests within `ttl_minutes` are served from the
# database instead of the network, and every dated result is kept so a later run
# for a now-past date still works. TransportAPI keys are redacted before storage.
[cache]
path = "cache.sqlite"    # SQLite database file (default "cache.sqlite")
ttl_minutes = 30         # how long a live result stays fresh (default 30)

# Settings for the --imageText / --output (image) rendering modes.
[image]
# Print width in dots (80mm = 576, 58mm = 384). Must be a multiple of 8.
width = 576
# Font used to rasterize the report. A Nerd Font gives the widest glyph coverage.
font = "fonts/JetBrainsMonoNerdFontMono-Regular.ttf"
# Body font size, in pixels.
font_size = 28.0
```

## Command-line options

| Flag | Description |
| --- | --- |
| `-c, --config <FILE>` | Path to the settings file (default `settings.toml`). |
| `-v, --verbose` | Increase logging verbosity (to stderr): `-v` for this crate's debug logs, `-vv` for trace. `RUST_LOG` overrides it. |
| `-l, --location <NAME>` | Weather location (overrides the settings file). |
| `-e, --endpoint <HOST:PORT>` | Printer endpoint (overrides the settings file). |
| `-d, --days <0..=2>` | Extra forecast days to request for **today's** ASCII art (default `0`). |
| `-D, --date <YYYYMMDD>` | Date to print (default: today). See [Dates](#dates-and-forecasts). |
| `-t, --tomorrow` | Shorthand for tomorrow's date (conflicts with `--date`). |
| `--at <HHMM>` | Look up transport departures at this time (scheduled timetable) instead of now. Enables the Transport section for any `--date`. |
| `--list-stops` | Print nearby stations and bus stops with their codes (to fill in `station_codes` / `bus_stop_codes`), then exit. |
| `--refresh` | Ignore cache freshness and refetch live data (the result is still stored). Historic past-date lookups are unaffected. |
| `--no-cache` | Disable the SQLite cache entirely for this run (no reads or writes). |
| `--stdout` | Print the report text to stdout instead of the printer. |
| `--raw` | Emit the raw ESC/POS byte stream to stdout (for debugging). |
| `--text` | Print as plain ESC/POS text instead of the default image (loses icons and other non-ASCII glyphs). |
| `--imageText` | Force image rendering (the default). Combine with `--stdout` to display the image inline in a kitty-compatible terminal. |
| `-o, --output <FILE>` | Write the rendered image to a PNG file instead of printing. |

`--stdout` and `--raw` are mutually exclusive. `--output` cannot be combined
with `--stdout` or `--raw`. `--stdout --imageText` is a valid combination (see
below).

### Output modes

- **Default** — the report is rasterized to a 1-bit image and sent to the
  printer, so full Unicode (weather arrows, degree signs, and the train/bus
  icons in the Transport section) renders exactly as shown. Variable-length text
  such as stop names is shortened to fit the configured print width.
- **`--output`** — same image rendering, but saved as a PNG preview instead of
  printing.
- **`--text`** — legacy plain-text ESC/POS printing. Unicode from wttr.in is
  transliterated to ASCII, and the Transport section falls back to `(train)` /
  `(bus)` labels instead of icons.
- **`--stdout`** — prints just the report body (no header) as UTF-8 text.
- **`--stdout --imageText`** — renders the receipt image and displays it inline
  using the [kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).
  Works in kitty and other terminals that implement it (e.g. WezTerm, Ghostty);
  elsewhere it prints the raw escape codes. Handy for previewing without a
  printer or writing a PNG file:

  ```sh
  thermy --stdout --imageText
  ```
- **`--raw`** — dumps the ESC/POS command bytes (e.g. pipe to `xxd`).

## Dates and forecasts

Use `-D/--date` with a `YYYYMMDD` value to print a specific day, or the
`-t/--tomorrow` shorthand:

```sh
thermy --date 20260716 --stdout
thermy --tomorrow --stdout
```

- **Today** shows current conditions as wttr.in ASCII art.
- **Tomorrow** and the **day after** show a compact forecast summary (highs and
  lows, four times of day, wind and rain) built from wttr.in's forecast API.
- **Any other date** prints a "no weather available" note but still lists that
  day's meetings.

The meetings heading adapts to the date ("Today's meetings", "Tomorrow's
meetings", or "Meetings").

## Transport departures

When a `[transport]` block with TransportAPI credentials is present, the receipt
gains a **Transport** section listing the nearest train stations and bus stops
with their next departures:

- The search location comes from the postcode in `address` (geocoded for free
  via [postcodes.io](https://postcodes.io)), or from an explicit `postcode` /
  `lat`+`lon` in the config.
- `stations`, `bus_stops`, and `departures` control how many of each to show
  (all default to 3). Setting `departures = 0` disables the Transport section
  entirely (no lookups are made).
- **Pin specific stops** with `station_codes` (CRS) and/or `bus_stop_codes`
  (ATCO). When set, these replace the nearest-stop search for that mode — and if
  both are pinned, no geocoding or proximity lookup happens at all. Run
  `thermy --list-stops` to print the nearby stops and their codes:

  ```
  Nearest train stations  (station_codes = CRS)
    ABC    0.3 km  Central Station
    XYZ    0.9 km  Riverside

  Nearest bus stops  (bus_stop_codes = ATCO)
    490000000A    0.1 km  High Street
  ```
- **Filter bus routes** with `routes` (e.g. `["162", "54"]`); when empty, all
  routes are shown. This affects buses only.
- **Look up a time** with `--at HHMM` (e.g. `--at 0900`). Without it, live "now"
  boards are used and the section only appears for today. With it, the scheduled
  timetable for the target `--date` at that time is used, so it works for any
  date (e.g. `--date 20260716 --at 0730`).
- Each nearest-stop run makes roughly `2 + stations + bus_stops` TransportAPI
  requests (two proximity lookups, one per mode, plus one departure board per
  stop; pinned stops skip their proximity lookup), so mind the free tier's daily
  quota (~30/day).
- If the lookup fails (network, bad key, quota), a warning is printed and the
  rest of the receipt still prints. When a stop is pinned by code, its name is
  remembered from the last `--list-stops` (or successful board) and reused, so it
  still shows a friendly name even while over quota.
- In the default image output each stop is prefixed with a train or bus icon;
  in `--text` mode a `(train)` / `(bus)` label is used instead. Stop names are
  shortened to fit the configured print width (`[image].width`).

## Caching and history

Every external call — weather (wttr.in), postcode geocoding (postcodes.io), and
transport (TransportAPI) — is recorded in a local SQLite database (`cache.sqlite`
by default; set `path` under `[cache]`). Each row stores the call's kind, the
parameters (including the target date), the time it was made, and the response.
This gives you three things:

- **Cache** — an identical request made within `ttl_minutes` (default 30) is
  served from the database instead of the network, which conserves the
  TransportAPI free-tier quota. Location lookups (geocoding and nearby stops)
  never move, so they're cached indefinitely.
- **History** — because each dated result is filed under its date, a request for
  a now-past date returns the stored copy even though wttr.in and the live
  departure boards can no longer reproduce it. For example, print today's
  receipt now, then re-run with `--date` for that day next week and it still
  works.
- **Resilience** — if a live fetch fails (network, quota), the most recent stored
  response for the same request is used as a fallback.

**Secrets are not persisted.** TransportAPI embeds your `app_key` in some
response URLs, so it is redacted (replaced with `APP_KEY`) before anything is
written to the database. Geocoding and weather have no secrets.

Control caching with:

- `--refresh` — force a live refetch, ignoring freshness (the new result is
  still stored); historic past-date lookups are unaffected.
- `--no-cache` — skip the database entirely for the run.
- Delete the `cache.sqlite` file to clear everything.

## Notes and limitations

- Google's secret iCal feed is cached, so newly-added events can take a while to
  appear.
- Weather is limited to wttr.in's short-range forecast (today plus the next two
  days).
- Recurring events are expanded, but per-instance overrides
  (`RECURRENCE-ID`) are not reconciled and could occasionally double-show.
- Requesting multi-day ASCII forecasts (`--days 1`/`--days 2`) can produce
  tables wider than the 576-dot print area and may clip.
- Transport departures need a free TransportAPI key. Live boards are today-only;
  use `--at HHMM` for a scheduled-timetable lookup on any date.
- The cache database grows over time (one row per external call). It's safe to
  delete `cache.sqlite` at any point; you'll only lose the reprint history.

## Development

```sh
cargo build      # always rebuild before running the binary
cargo test       # unit tests (weather image, calendar + transport parsing)
```

To test calendars without exposing a real feed, serve a fixture `.ics` over
localhost and point a settings file at it:

```sh
python3 -m http.server 8767 --directory /tmp &
thermy --config /tmp/settings.toml --date 20261228 --stdout
```

## License

This project is licensed under the [MIT License](LICENSE).

The bundled font `fonts/JetBrainsMonoNerdFontMono-Regular.ttf` is
[JetBrains Mono](https://github.com/JetBrains/JetBrainsMono) (patched by
[Nerd Fonts](https://github.com/ryanoasis/nerd-fonts)), which is licensed under
the SIL Open Font License 1.1. See [fonts/OFL.txt](fonts/OFL.txt) for the full
text. "JetBrains Mono" is a trademark of JetBrains s.r.o.
