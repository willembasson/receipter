# receipter

A small Rust CLI that prints a daily "morning receipt" to an 80mm ESC/POS
network thermal printer. Each receipt has three parts:

1. **Header** — your address and the report's date.
2. **Weather** — current conditions (today) or a short forecast (future days)
   from [wttr.in](https://wttr.in).
3. **Calendar** — the day's meetings, pulled from one or more Google Calendar
   "secret iCal" feeds.
4. **Transport** _(optional)_ — the nearest train stations and bus stops with
   their next departures, from [TransportAPI](https://developer.transportapi.com).
5. **Bin day** _(optional)_ — which bins to put out, shown only on the eve of a
   collection, from your council's collection-day service (Bromley or Hackney).

Every external call (weather, geocoding, transport, and bin days) is cached in a
local SQLite database, which doubles as a history so past dates can be
reprinted.

It can print directly to the printer, or preview the output to your terminal or
a PNG file. It can also print an image you give it instead of the report.

## Installation

```sh
cargo build --release
```

The binary is written to `target/release/receipter`. A JetBrains Mono Nerd Font
is bundled under `fonts/` for the image renderer, so no system font install is
required.

## Quick start

```sh
# Print today's receipt to the printer configured in settings.toml
receipter

# Preview today's receipt in the terminal (no printer needed)
receipter --stdout

# Preview as an image (full Unicode: arrows, degree signs, icons)
receipter --output preview.png

# Print any image (from a file, or piped in on stdin)
receipter photo.jpg
receipter --stdout --imageText | receipter
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

# Optional bin collection days. When set, the receipt gains a "Bin day" section
# on the eve of a collection (and on no other day). `council` selects the
# scraper; "Bromley" and "Hackney" are implemented. The property is found from
# the postcode and house/street in `address` — run `receipter --list-bins` to
# check the match. See "Bin days" below.
[bins]
council = "Bromley"
# postcode = "AB1 2CD"        # optional; defaults to the postcode in `address`
# property = "11 Example St"  # optional; defaults to `address`
# property_id = "3678999"     # optional; skips the address lookup entirely
# lead_days = 1               # days of notice (default 1, i.e. the day before)

# Optional on-disk cache/history for every external call (weather, geocoding,
# transport, bin days). Identical requests within `ttl_minutes` are served from
# the database instead of the network, and every dated result is kept so a later
# run for a now-past date still works. TransportAPI keys are redacted before
# storage.
[cache]
path = "cache.sqlite"    # SQLite database file (default "cache.sqlite")
ttl_minutes = 30         # how long a live result stays fresh (default 30)

# Settings for the --imageText / --output (image) rendering modes, and for any
# image printed directly (see "Printing an image").
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
| `--bins` | Always include the Bin day section, showing the next collection even when it isn't the eve of one. See [Bin days](#bin-days). |
| `--list-bins` | Print every bin collection for the configured property, with the council's property id (to fill in `property_id`), then exit. |
| `--refresh` | Ignore cache freshness and refetch live data (the result is still stored). Historic past-date lookups are unaffected. |
| `--no-cache` | Disable the SQLite cache entirely for this run (no reads or writes). |
| `--stdout` | Print the report text to stdout instead of the printer. |
| `--raw` | Emit the raw ESC/POS byte stream to stdout (for debugging). |
| `--text` | Print as plain ESC/POS text instead of the default image (loses icons and other non-ASCII glyphs). |
| `--imageText` | Force image rendering (the default). Combine with `--stdout` to display the image inline in a kitty-compatible terminal. |
| `-o, --output <FILE>` | Write the rendered image to a PNG file instead of printing. |
| `--dither <auto\|on\|off>` | How a printed **image** is reduced to black and white (default `auto`). See [Printing an image](#printing-an-image). |
| `--lighten <PERCENT>` | Lighten a printed **image** by this percentage before reducing it to black and white (default `0`). See [Printing an image](#printing-an-image). |
| `[IMAGE]` | Print this image instead of the report. `-` reads it from stdin. See [Printing an image](#printing-an-image). |

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
  receipter --stdout --imageText
  ```

  When stdout is **not** a terminal, the raw PNG bytes are written instead of
  the escape codes, so the image can be piped or redirected:

  ```sh
  receipter --stdout --imageText > receipt.png
  receipter --stdout --imageText | receipter   # render now, print it
  ```
- **`--raw`** — dumps the ESC/POS command bytes (e.g. pipe to `xxd`).

## Printing an image

Any image can be printed instead of the report, either as an argument or piped
in on stdin:

```sh
receipter photo.jpg                  # from a file
cat photo.jpg | receipter            # from a pipe
receipter - < photo.jpg              # `-` is stdin, explicitly
receipter photo.jpg -o preview.png   # preview exactly what would be printed
```

PNG, JPEG, GIF, BMP and WebP are accepted. The image is composited onto white
(so transparency doesn't come out as solid black), scaled to the full print
width from `[image].width` keeping its aspect ratio, and reduced to the pure
black and white the printer can actually put on paper. In this mode no weather,
calendar or transport lookups happen at all.

How the reduction is done matters, so `--dither` controls it:

- `auto` _(default)_ — dithers images with real mid-tones (photographs,
  gradients) and thresholds ones that are already black and white (text, line
  art, a receipt), which keeps each looking its best.
- `on` — always dither (Floyd–Steinberg).
- `off` — always threshold at mid-gray.

### Too dark?

Thermal dots bleed into each other, so they cover more paper than the image
asked for and a dithered photo usually prints darker than it looks on screen.
`--lighten <PERCENT>` blends the image toward paper white first, which scales
the ink coverage down by the same percentage:

```sh
receipter photo.jpg --lighten 30   # lay down 30% fewer dots
```

Start around 20–40 and adjust to taste; `--output` renders exactly what would be
printed, so you can compare without wasting paper. The right value depends on
your printer and paper, so it's worth calibrating once.

Lightening a **thresholded** image behaves differently, because thresholding has
no half measures: artwork barely changes up to 50%, then drops out entirely once
solid black lightens past mid-gray. If a receipt or logo vanishes, use
`--dither on` to thin it smoothly instead. `receipter` warns rather than quietly
feeding you a blank slip:

```
warning: nothing is left to print after --lighten 60; try a lower percentage, or
--dither on to thin the artwork instead of dropping it
```

Because a receipt rendered by `--imageText` is already black and white at the
print width, piping one back in reprints it dot for dot:

```sh
receipter --stdout --imageText | receipter
```

Stdin is only read when it has been redirected, so an interactive run is never
left waiting for input, and a piped-but-empty stdin (`< /dev/null`, as in a cron
job) just builds the usual report.

The other output flags work here too: `--output` writes the prepared image to a
PNG, `--stdout` displays it (or emits the PNG bytes when redirected), and
`--raw` dumps the ESC/POS bytes.

## Dates and forecasts

Use `-D/--date` with a `YYYYMMDD` value to print a specific day, or the
`-t/--tomorrow` shorthand:

```sh
receipter --date 20260716 --stdout
receipter --tomorrow --stdout
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
  `receipter --list-stops` to print the nearby stops and their codes:

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

## Bin days

When a `[bins]` block is present, the receipt gains a **Bin day** section — but
only on the eve of a collection. On every other day nothing is printed at all,
not even a heading, because a standing "your next collection is in five days"
note is noise; the reminder is only useful the evening before.

```
----------------------------------------
Bin day tomorrow

  Thursday, 13 August

    Food Waste
    Mixed Recycling (Cans, Plastics & Glass)
```

- `council` selects which scraper to use. Councils publish this data in wildly
  different ways, so each one is a separate implementation. **`"Bromley"` and
  `"Hackney"` are implemented**; any other value is a clear error rather than a
  silent no-op. The name is matched loosely, so `"Hackney"`, `"hackney"`,
  `"Hackney Council"` and `"London Borough of Hackney"` are all the same
  council.
- The property is found from the **postcode** and the **house-and-street** part
  of your `address`, so the top-level setting is usually all the config you
  need. Override either with `postcode` / `property`.
- `lead_days` (default `1`) is how much notice you want. `1` means the section
  appears the day before; `0` would make it appear on the morning of collection
  day, which is generally too late to be useful.
- **`--bins` forces the section on**, showing the next collection from the
  target date onwards even when it isn't the eve of one. Useful for checking the
  section renders without waiting for a collection to come round, or as a
  standing "what's next" if you always want it. The heading reflects the real
  distance, so it stays honest:

  ```sh
  receipter --stdout            # nothing, unless a collection is tomorrow
  receipter --stdout --bins     # "Bin day in 6 days" / "Bin day tomorrow"
  ```

  If it can't show anything (no `[bins]` block, or a past `--date`), it says so
  on stderr rather than silently doing nothing.
- Run **`receipter --list-bins`** to check the address matched and see every
  service's next collection:

  ```
  11 Example Close, Townsville, AB1 2CD (property_id = 3657977)

  2026-08-13  Thu  Food Waste
  2026-08-13  Thu  Mixed Recycling (Cans, Plastics & Glass)
  2026-08-18  Tue  Garden Waste
  2026-08-20  Thu  Non-Recyclable Refuse
  2026-08-20  Thu  Paper & Cardboard
  ```

  If the match is ambiguous or wrong, paste the id into `property_id` to skip
  the address lookup entirely.
- On-demand services (bulky waste collections, battery/textile requests) have no
  scheduled next collection, so they never appear.
- The council reports what is *next*, so the section is only produced for today
  and future dates; `--date` for a past day leaves it out. `--tomorrow` shifts
  the whole receipt, so it shows the collection two days out — whatever
  tomorrow's receipt would say.
- If the lookup fails, a warning is printed and the rest of the receipt still
  prints.
- In the default image output the date is prefixed with a bin icon; in `--text`
  mode the date stands alone. Service names are shortened to fit the configured
  print width (`[image].width`).

### How the councils are implemented

Each council lives in its own module under `src/bins/`, implementing a single
function that turns the configured address into `(service name, collection
date)` pairs. The day-before gate, formatting, address matching and caching are
shared. The two supported councils could hardly be less alike, which is rather
the point of the split:

**Bromley** — HTML scraping. The council page people are pointed at
(`bromley.gov.uk/household-waste-recycling/bin-collection-days`) is only a
signpost; the real service is FixMyStreet **WasteWorks** at
`recyclingservices.bromley.gov.uk`. Two requests get us there:

1. `POST /waste` with the postcode returns a `<select>` listing every property
   at that postcode. (It must be a POST — the same query over GET returns the
   empty form.) This mapping never changes, so it is cached indefinitely.
2. `GET /waste/<id>` returns the collections *eventually*. The first response is
   an interstitial reading "Loading your bin days..." while the backend fetches
   from the council's system; re-requesting the same URL a few seconds later
   returns the real page. `receipter` polls for it (up to 15 attempts, 2s apart;
   in practice it resolves on the third or fourth, ~6s). Only a fully loaded
   page is ever cached. Note that WasteWorks serves a 503 to obviously-automated
   clients, so this module (unlike the rest of the crate) presents a normal
   desktop browser User-Agent.

Bromley reports only the *next* collection per service, so that is all there is
to know.

**Hackney** — a JSON API, no scraping. The council page embeds a Nuxt
single-page app which talks to an unauthenticated API; its base URL and tenant
id are baked into the page and reproduced as constants. Getting from a postcode
to a set of dates takes four hops: `property/opensearch` →
`alloywastepages/getproperty` (which yields a list of *bin* ids) →
`alloywastepages/getbin` → `alloywastepages/getcollection` and
`alloywastepages/getworkflow`. That is a dozen or so requests for one property,
which is why each response is cached individually — a repeat run makes none at
all, and bins sharing a collection round share a cached workflow.

Two Hackney quirks worth knowing:

- **Labels are containers, not services.** Hackney names a collection by the bin
  it comes in ("Recycling Sack", "Wheeled Bin (180ltr)", "1 x ES_Food 240
  litres") rather than by service. The only other hint in the API is a bin-type
  id that maps to an icon image, and several of those are ambiguous, so rather
  than guess a service name and risk mislabelling a bin this uses the council's
  own wording — the same text you see on Hackney's own page. (The internal
  `ES_` estate-services marker is stripped, as it means nothing to a resident.)
- **Disabled and non-live containers are skipped**, matching what the council's
  own front end shows.

Unlike Bromley, Hackney publishes a full rolling schedule reaching about a year
out, so a run of upcoming dates is kept per container. That is what lets the
day-before gate stay correct for a future `--date` instead of only ever knowing
the next collection from today.

> **Adding another council.** Implement the same one function in a new module
> under `src/bins/` and add it to the dispatch in `src/bins/mod.rs`. If you need
> broad multi-council support rather than hand-rolled ones,
> [UKBinCollectionData][ukbcd] already covers 150+ UK councils — at some point
> this may be replaced by, or grow into, a port of what it does. For now it
> deliberately implements a couple of councils well.

[ukbcd]: https://github.com/robbrad/UKBinCollectionData

## Caching and history

Every external call — weather (wttr.in), postcode geocoding (postcodes.io),
transport (TransportAPI), and bin days (your council) — is recorded in a local
SQLite database (`cache.sqlite` by default; set `path` under `[cache]`). Each row
stores the call's kind, the parameters (including the target date), the time it
was made, and the response. This gives you three things:

- **Cache** — an identical request made within `ttl_minutes` (default 30) is
  served from the database instead of the network, which conserves the
  TransportAPI free-tier quota and keeps the bin-day polling to once a day.
  Location lookups (geocoding, nearby stops, and a postcode's address list)
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
- Bin days are read from whatever the council publishes — HTML for Bromley, an
  undocumented JSON API for Hackney — so a site or API change will break them.
  Failures warn and skip the section rather than stopping the receipt. Only
  Bromley and Hackney are implemented.
- The cache database grows over time (one row per external call). It's safe to
  delete `cache.sqlite` at any point; you'll only lose the reprint history.

## Development

```sh
cargo build      # always rebuild before running the binary
cargo test       # unit tests (weather image, calendar + transport + bin parsing)
```

To test calendars without exposing a real feed, serve a fixture `.ics` over
localhost and point a settings file at it:

```sh
python3 -m http.server 8767 --directory /tmp &
receipter --config /tmp/settings.toml --date 20261228 --stdout
```

## License

This project is licensed under the [MIT License](LICENSE).

The bundled font `fonts/JetBrainsMonoNerdFontMono-Regular.ttf` is
[JetBrains Mono](https://github.com/JetBrains/JetBrainsMono) (patched by
[Nerd Fonts](https://github.com/ryanoasis/nerd-fonts)), which is licensed under
the SIL Open Font License 1.1. See [fonts/OFL.txt](fonts/OFL.txt) for the full
text. "JetBrains Mono" is a trademark of JetBrains s.r.o.
