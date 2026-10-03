//! Turning a [`Quote`] into the price chart, and into the caption beside it.
//!
//! The chart is drawn with `plotters`. It follows the reference card: a title, the
//! time it was drawn, the last price and its change, a right-aligned
//! open/high/low/close block, and the intraday line over a direction-colored area
//! with a dashed reference line at the latest price. Dashed gridlines mark the
//! price and time scales, with a cyan line at WIB midnight and angled time labels
//! below the plot. The price axis is labelled at every tick.

use std::io::Cursor;
use std::sync::OnceLock;

use chrono::{DateTime, FixedOffset, Local, TimeZone, Utc};
use image::{ImageFormat, RgbImage};
use plotters::coord::Shift;
use plotters::element::DashedPathElement;
use plotters::prelude::*;
use plotters::style::register_font;
use plotters::style::text_anchor::{HPos, Pos, VPos};

use super::market::Quote;

/// The chart's dimensions. The height leaves a wide band under the plot for the
/// two rows of rotated time labels.
const WIDTH: u32 = 2000;
const HEIGHT: u32 = 1840;

/// The header strip above the plot, and the bands reserved for its axes.
const HEADER_HEIGHT: i32 = 610;
const LEFT_LABELS: i32 = 260;
const BOTTOM_LABELS: i32 = 290;
const MARGIN_TOP: i32 = 18;
const MARGIN_RIGHT: i32 = 100;

/// How many gridlines to aim for down the price axis, and across the time axis.
const TICKS: f64 = 8.0;
const TIME_TICKS: f64 = 26.0;
const FUTURE_HOURS: i64 = 2;

/// The price line's thickness and the dash pattern for reference lines.
const LINE_THICKNESS: u32 = 4;
const GRID_THICKNESS: u32 = 2;
const DASH: i32 = 10;
const GAP: i32 = 8;

/// The longest edge of the JPEG preview WhatsApp shows while the chart loads.
const THUMB_SIDE: u32 = 200;

/// The registered font family name, and the sizes the card draws at.
const FONT: &str = "card";
const TITLE_SIZE: f64 = 150.0;
const STAMP_SIZE: f64 = 78.0;
const PRICE_SIZE: f64 = 190.0;
const CHANGE_SIZE: f64 = 80.0;
const BLOCK_SIZE: f64 = 58.0;
const LABEL_SIZE: f64 = 34.0;

static BACKGROUND: RGBColor = RGBColor(0, 0, 0);
static TEXT: RGBColor = RGBColor(255, 255, 255);
static MUTED: RGBColor = RGBColor(255, 255, 255);
static GRID: RGBColor = RGBColor(150, 150, 150);
static LINE: RGBColor = RGBColor(255, 255, 0);
static UP_FILL: RGBColor = RGBColor(0, 38, 0);
static DOWN_FILL: RGBColor = RGBColor(38, 0, 0);
static FLAT_FILL: RGBColor = RGBColor(18, 18, 18);
/// The day boundary, where the window crosses local midnight.
static MIDNIGHT: RGBColor = RGBColor(0, 255, 255);
static REFERENCE: RGBColor = RGBColor(255, 255, 0);
static UP: RGBColor = RGBColor(0, 128, 0);
static DOWN: RGBColor = RGBColor(255, 0, 0);

/// Draws the chart and returns the encoded PNG.
pub fn render(quote: &Quote, now: DateTime<Local>) -> Result<Vec<u8>, String> {
    register_card_font();

    let mut buffer = vec![0u8; (WIDTH * HEIGHT * 3) as usize];
    // The drawing areas borrow `buffer`, so they have to be dropped before it can
    // be handed to the PNG encoder.
    {
        let root = BitMapBackend::with_buffer(&mut buffer, (WIDTH, HEIGHT)).into_drawing_area();
        root.fill(&BACKGROUND).map_err(failed)?;

        let (change, percent) = quote.change();
        let accent = if change > 0.0 {
            &UP
        } else if change < 0.0 {
            &DOWN
        } else {
            &TEXT
        };

        let (header, plot) = root.split_vertically(HEADER_HEIGHT);
        draw_header(&header, quote, now, change, percent, accent)?;
        draw_plot(&root, &plot, quote)?;
    }

    let image = RgbImage::from_raw(WIDTH, HEIGHT, buffer)
        .ok_or_else(|| "Could not build the chart image.".to_string())?;
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|error| format!("Could not encode the chart: {error}"))?;
    Ok(png)
}

/// The one-line summary that rides with the image as its caption.
pub fn caption(quote: &Quote) -> String {
    let (change, percent) = quote.change();
    let arrow = if change >= 0.0 { "▲" } else { "▼" };
    let decimals = price_decimals(quote.last);
    format!(
        "{arrow} *{}*: {:.*} {}\n24h change: {:+.*} {} ({:+.2}%)",
        quote.name, decimals, quote.last, quote.currency, decimals, change, quote.currency, percent
    )
}

/// A small JPEG preview of the rendered chart, for the `jpegThumbnail` WhatsApp
/// shows in the chat while the full image loads.
pub fn thumbnail(png: &[u8]) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory(png)
        .map_err(|error| format!("Could not decode the chart: {error}"))?
        .thumbnail(THUMB_SIDE, THUMB_SIDE);

    let mut jpeg = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
        .map_err(|error| format!("Could not encode the chart preview: {error}"))?;
    Ok(jpeg)
}

/// Registers the bundled font once. `register_font`'s error type does not
/// implement `Debug`, so the result cannot be unwrapped; a bad font would show as
/// missing text rather than a panic.
fn register_card_font() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        let _ = register_font(
            FONT,
            FontStyle::Normal,
            include_bytes!("../../assets/NotoSansMono-Regular.ttf"),
        );
    });
}

/// The title, the timestamp, the big price, and the OHLC block.
fn draw_header(
    header: &DrawingArea<BitMapBackend, Shift>,
    quote: &Quote,
    now: DateTime<Local>,
    change: f64,
    percent: f64,
    accent: &'static RGBColor,
) -> Result<(), String> {
    let decimals = price_decimals(quote.last);

    let title = quote.name.clone();
    header
        .draw_text(&title, &text_style(TITLE_SIZE, &TEXT), (264, 30))
        .map_err(failed)?;

    let stamp = wib_time(now.timestamp())
        .format("%a %b %d %Y  %H:%M:%S WIB")
        .to_string();
    header
        .draw_text(&stamp, &text_style(STAMP_SIZE, &TEXT), (264, 190))
        .map_err(failed)?;

    let price = format!(
        "{} {}",
        grouped_price(quote.last, decimals),
        quote.currency.to_uppercase()
    );
    header
        .draw_text(&price, &text_style(PRICE_SIZE, accent), (264, 300))
        .map_err(failed)?;

    let change_line = format!(
        "24h chg: {:+.*} {} ({:+.2}%)",
        decimals,
        change,
        quote.currency.to_uppercase(),
        percent
    );
    header
        .draw_text(&change_line, &text_style(CHANGE_SIZE, accent), (264, 535))
        .map_err(failed)?;

    // The open/high/low/close block sits against the right edge, one line each.
    let block = [
        ("O:", quote.open()),
        ("H:", quote.day_high),
        ("L:", quote.day_low),
        ("C:", quote.last),
    ];
    let style = text_style(BLOCK_SIZE, &TEXT).pos(Pos::new(HPos::Right, VPos::Top));
    for (row, (label, value)) in block.iter().enumerate() {
        let line = format!("{label} {}", grouped_price(*value, decimals));
        header
            .draw_text(&line, &style, (WIDTH as i32 - 100, 130 + row as i32 * 54))
            .map_err(failed)?;
    }
    Ok(())
}

/// The price axis, the time axis, the filled area, and the price line.
fn draw_plot(
    root: &DrawingArea<BitMapBackend, Shift>,
    plot: &DrawingArea<BitMapBackend, Shift>,
    quote: &Quote,
) -> Result<(), String> {
    // The y range covers the series and the window's open, padded so the line
    // never touches the frame.
    let mut low = quote.day_low.min(quote.last);
    let mut high = quote.day_high.max(quote.last);
    for (_, price) in &quote.series {
        low = low.min(*price);
        high = high.max(*price);
    }
    if high <= low {
        // A perfectly flat window still needs a non-zero span to divide by.
        low -= 0.5;
        high += 0.5;
    }
    let pad = (high - low) * 0.04;
    let (low, high) = (low - pad, high + pad);

    let (x_min, x_max) = match (quote.series.first(), quote.series.last()) {
        (Some((first, _)), Some((last, _))) if last > first => {
            (*first as f64, (*last + FUTURE_HOURS * 3600) as f64)
        }
        // A single point, or none, still needs a span to draw against.
        _ => (0.0, 1.0),
    };

    let mut chart = ChartBuilder::on(plot)
        .margin_top(MARGIN_TOP)
        .margin_right(MARGIN_RIGHT)
        .set_label_area_size(LabelAreaPosition::Left, LEFT_LABELS)
        .set_label_area_size(LabelAreaPosition::Bottom, BOTTOM_LABELS)
        .build_cartesian_2d(x_min..x_max, low..high)
        .map_err(failed)?;

    let (x_range, y_range) = chart.plotting_area().get_pixel_range();
    let (plot_left, plot_right) = (x_range.start, x_range.end - 1);
    let (plot_top, plot_bottom) = (y_range.start, y_range.end - 1);

    let points: Vec<(f64, f64)> = quote
        .series
        .iter()
        .map(|(time, price)| (*time as f64, *price))
        .collect();

    // The filled area goes down first, so every line below reads over it.
    if points.len() >= 2 {
        let (change, _) = quote.change();
        let fill = if change > 0.0 {
            &UP_FILL
        } else if change < 0.0 {
            &DOWN_FILL
        } else {
            &FLAT_FILL
        };
        chart
            .draw_series(AreaSeries::new(points.iter().copied(), low, fill.filled()))
            .map_err(failed)?;
    }

    // Horizontal gridlines with a price label at each, so any point's value can
    // be read off the grid.
    let decimals = tick_decimals((high - low) / TICKS);
    for tick in nice_ticks(low, high, TICKS) {
        let y = chart.backend_coord(&(x_min, tick)).1;
        if !(plot_top..=plot_bottom).contains(&y) {
            continue;
        }
        root.draw(&DashedPathElement::new(
            vec![(plot_left, y), (plot_right, y)],
            DASH,
            GAP,
            GRID.stroke_width(GRID_THICKNESS),
        ))
        .map_err(failed)?;
        let label = format!("{:.*}", decimals, tick);
        let style = text_style(LABEL_SIZE, &MUTED).pos(Pos::new(HPos::Right, VPos::Center));
        root.draw_text(&label, &style, (plot_left - 12, y))
            .map_err(failed)?;
    }

    // The vertical gridlines, placed by the clock so they read as a scale. Each
    // tick's pixel column is resolved here, while the chart is borrowed, and
    // carried to the label pass below.
    let mut placed = Vec::new();
    for tick in vertical_ticks(quote, TIME_TICKS) {
        let x = chart.backend_coord(&(tick.seconds as f64, low)).0;
        if !(plot_left..=plot_right).contains(&x) {
            continue;
        }
        vline(
            root,
            x,
            plot_top,
            plot_bottom,
            GRID.stroke_width(GRID_THICKNESS),
        )?;
        placed.push((x, tick));
    }

    root.draw(&Rectangle::new(
        [(plot_left, plot_top), (plot_right, plot_bottom)],
        TEXT.stroke_width(2),
    ))
    .map_err(failed)?;

    // The latest price is marked across the plot, as in the reference card.
    let reference = quote.last;
    if (low..=high).contains(&reference) {
        chart
            .draw_series(DashedLineSeries::new(
                vec![(x_min, reference), (x_max, reference)],
                DASH,
                GAP,
                REFERENCE.stroke_width(LINE_THICKNESS),
            ))
            .map_err(failed)?;
    }

    // The midnight lines sit over the fill but under the price line, so the price
    // still reads as the front-most thing on the plot.
    for (x, tick) in &placed {
        if tick.midnight {
            vline(
                root,
                *x,
                plot_top,
                plot_bottom,
                MIDNIGHT.stroke_width(LINE_THICKNESS),
            )?;
        }
    }

    if points.len() >= 2 {
        chart
            .draw_series(LineSeries::new(points, LINE.stroke_width(LINE_THICKNESS)))
            .map_err(failed)?;
    }

    let title = format!("Price ({})", quote.currency.to_uppercase());
    let style = text_style(LABEL_SIZE, &TEXT).transform(FontTransform::Rotate270);
    let width = root.estimate_text_size(&title, &style).map_err(failed)?.0 as i32;
    let middle = (plot_top + plot_bottom) / 2;
    root.draw_text(&title, &style, (100, middle + width / 2))
        .map_err(failed)?;

    draw_time_axis(root, &placed, plot_bottom)?;
    Ok(())
}

/// A vertical gridline: the Unix second it sits at, the clock reading it stands
/// for, and whether it marks local midnight.
struct TimeTick {
    seconds: i64,
    at: DateTime<FixedOffset>,
    midnight: bool,
}

/// Converts a Unix timestamp to the chart's WIB clock, independent of the host zone.
fn wib_time(seconds: i64) -> DateTime<FixedOffset> {
    Utc.timestamp_opt(seconds, 0)
        .single()
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .with_timezone(&FixedOffset::east_opt(7 * 3600).unwrap())
}

/// The vertical gridlines across the window: a labelled line at each whole hour,
/// thinned so their labels never collide, with a cyan line kept at every local
/// midnight so the day boundary always reads.
///
/// The lines are placed by the clock rather than by the series, so an hour whose
/// bar is missing — a gap in trading — still gets its line.
fn vertical_ticks(quote: &Quote, target: f64) -> Vec<TimeTick> {
    let Some((first_time, _)) = quote.series.first().copied() else {
        return Vec::new();
    };
    let Some((last_time, _)) = quote.series.last().copied() else {
        return Vec::new();
    };
    let tick_end = last_time + FUTURE_HOURS * 3600;
    let span = tick_end - first_time;
    if span <= 0 {
        return Vec::new();
    }
    // A whole number of hours across the window, so labels land evenly.
    let step_hours = ((span as f64 / 3600.0 / target).ceil() as i64).max(1);

    // Start at the first local clock boundary that follows the window's start.
    let mut time = first_time.div_euclid(3600) * 3600;
    if time < first_time {
        time += 3600;
    }

    let mut ticks = Vec::new();
    while time <= tick_end {
        let at = wib_time(time);
        let hour = at.format("%H").to_string().parse::<i64>().unwrap_or(0);
        let midnight = hour == 0;
        // Local midnight remains marked even when coarse spacing skips its hour.
        if midnight || hour % step_hours == 0 {
            ticks.push(TimeTick {
                seconds: time,
                at,
                midnight,
            });
        }
        time += 3600;
    }
    ticks
}

/// The labels under the plot: diagonal hour marks and vertical dates at midnight.
fn draw_time_axis(
    root: &DrawingArea<BitMapBackend, Shift>,
    placed: &[(i32, TimeTick)],
    plot_bottom: i32,
) -> Result<(), String> {
    // The top row starts just under the plot; the date row sits below it.
    let hour_row = plot_bottom + 16;
    let date_row = plot_bottom + 88;
    for (x, tick) in placed {
        let hour = tick.at.format("%-I %p").to_string();
        draw_rotated(root, &hour, &TEXT, *x, hour_row, -55.0)?;
        if tick.midnight {
            let date = tick.at.format("%b %d").to_string();
            draw_rotated(root, &date, &TEXT, *x, date_row, -90.0)?;
        }
    }
    Ok(())
}

/// Draws one rotated time label whose run starts at `row` and extends up, so the
/// first character sits against the plot and the rest climb away from it.
///
/// The rotation turns the run's width into its height, so the anchor is moved up
/// by that width to put the run's bottom edge on `row`.
fn draw_rotated(
    root: &DrawingArea<BitMapBackend, Shift>,
    text: &str,
    color: &'static RGBColor,
    x: i32,
    row: i32,
    angle: f64,
) -> Result<(), String> {
    let style = text_style(LABEL_SIZE, color);
    let (width, height) = root.estimate_text_size(text, &style).map_err(failed)?;
    let width = (width as usize).max(1);
    let height = (height as usize).max(1);
    let mut pixels = vec![0u8; width * height * 3];
    {
        let area = BitMapBackend::with_buffer(&mut pixels, (width as u32, height as u32))
            .into_drawing_area();
        area.fill(&BACKGROUND).map_err(failed)?;
        area.draw_text(text, &style, (0, 0)).map_err(failed)?;
    }

    let (sin, cos) = angle.to_radians().sin_cos();
    let center = (width as f64 / 2.0, height as f64 / 2.0);
    let rotated_height = width as f64 * sin.abs() + height as f64 * cos.abs();
    let anchor = (x as f64, row as f64 + rotated_height / 2.0);
    for source_y in 0..height {
        for source_x in 0..width {
            let index = (source_y * width + source_x) * 3;
            let pixel = &pixels[index..index + 3];
            if pixel.iter().all(|channel| *channel < 16) {
                continue;
            }
            let dx = source_x as f64 - center.0;
            let dy = source_y as f64 - center.1;
            let target_x = (anchor.0 + dx * cos - dy * sin).round() as i32;
            let target_y = (anchor.1 + dx * sin + dy * cos).round() as i32;
            root.draw_pixel(
                (target_x, target_y),
                &RGBColor(pixel[0], pixel[1], pixel[2]),
            )
            .map_err(failed)?;
        }
    }
    Ok(())
}

/// A dashed vertical line, drawn on the full canvas by absolute coordinate.
fn vline(
    root: &DrawingArea<BitMapBackend, Shift>,
    x: i32,
    y0: i32,
    y1: i32,
    style: ShapeStyle,
) -> Result<(), String> {
    root.draw(&DashedPathElement::new(
        vec![(x, y0), (x, y1)],
        DASH,
        GAP,
        style,
    ))
    .map_err(failed)
}

/// A text style at `size` in the card font, in `color`.
fn text_style(size: f64, color: &'static RGBColor) -> TextStyle<'static> {
    TextStyle::from((FONT, size).into_font()).color(color)
}

/// The decimals a price axis tick needs, from the size of the step between them.
fn tick_decimals(step: f64) -> usize {
    if step <= 0.0 {
        return 2;
    }
    ((-step.log10().floor()) as i64).clamp(0, 4) as usize
}

/// Formats a price with the reference card's thousands separators.
fn grouped_price(price: f64, decimals: usize) -> String {
    let formatted = format!("{price:.decimals$}");
    let (whole, fraction) = formatted.split_once('.').unwrap_or((&formatted, ""));
    let mut grouped = String::with_capacity(formatted.len() + whole.len() / 3);
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if decimals > 0 {
        grouped.push('.');
        grouped.push_str(fraction);
    }
    grouped
}

/// The decimals the headline price needs: more for the small numbers FX quotes.
fn price_decimals(price: f64) -> usize {
    if price.abs() < 10.0 { 4 } else { 2 }
}

/// Rounded tick values covering `min..=max`, at a step that is 1, 2, or 5 times a
/// power of ten.
fn nice_ticks(min: f64, max: f64, target: f64) -> Vec<f64> {
    let span = max - min;
    if span <= 0.0 || target < 1.0 {
        return vec![min];
    }
    let raw = span / target;
    let magnitude = 10f64.powf(raw.log10().floor());
    let normalized = raw / magnitude;
    let step = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    } * magnitude;

    let mut ticks = Vec::new();
    let mut value = (min / step).ceil() * step;
    while value <= max + step * 0.5 {
        ticks.push(value);
        value += step;
    }
    ticks
}

/// The message every plotters drawing failure is reported with.
fn failed(error: impl std::fmt::Display) -> String {
    format!("Could not draw the chart: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Local> {
        wib_time(
            Utc.with_ymd_and_hms(2026, 10, 2, 8, 18, 17)
                .unwrap()
                .timestamp(),
        )
        .with_timezone(&Local)
    }

    fn quote(last: f64, prev_close: f64) -> Quote {
        let start = Utc
            .with_ymd_and_hms(2026, 10, 1, 9, 0, 0)
            .unwrap()
            .timestamp();
        let delta = last - prev_close;
        Quote {
            name: "Crude Oil Nov 26".to_string(),
            symbol: "CL=F".to_string(),
            currency: "USD".to_string(),
            last,
            prev_close,
            day_high: last.max(prev_close) + delta.abs() * 0.12,
            day_low: last.min(prev_close) - delta.abs() * 0.12,
            series: (0..97)
                .map(|i| {
                    let progress = i as f64 / 96.0;
                    let wave = (i as f64 * 0.37).sin() * delta.abs() * 0.03;
                    (start + i * 900, prev_close + delta * progress + wave)
                })
                .collect(),
        }
    }

    #[test]
    fn the_chart_is_a_png_of_the_expected_size() {
        let png = render(&quote(90.21, 92.40), now()).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "not a png");
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (WIDTH, HEIGHT));
    }

    #[test]
    #[ignore]
    fn write_direction_previews() {
        use std::fs;

        for (name, quote) in [
            ("up", quote(2770.25, 2676.04)),
            ("down", quote(90.21, 92.40)),
        ] {
            let image = render(&quote, now()).unwrap();
            fs::write(format!("/tmp/chart-{name}.png"), image).unwrap();
        }
    }

    #[test]
    fn the_thumbnail_is_a_jpeg_scaled_to_the_preview_side() {
        let png = render(&quote(90.21, 92.40), now()).unwrap();
        let thumb = thumbnail(&png).unwrap();
        assert!(thumb.starts_with(&[0xFF, 0xD8]), "not a jpeg");
        // The longest edge is the preview side, and the chart's aspect is kept.
        let decoded = image::load_from_memory(&thumb).unwrap();
        assert_eq!(decoded.width().max(decoded.height()), THUMB_SIDE);
    }

    #[test]
    fn a_flat_window_still_renders() {
        // Every price equal, and equal to the previous close: no span to divide by.
        let flat = Quote {
            day_high: 50.0,
            day_low: 50.0,
            series: vec![(1, 50.0), (2, 50.0)],
            ..quote(50.0, 50.0)
        };
        assert!(render(&flat, now()).is_ok());
    }

    #[test]
    fn an_empty_series_still_renders() {
        let empty = Quote {
            series: Vec::new(),
            ..quote(90.21, 92.40)
        };
        assert!(render(&empty, now()).is_ok());
    }

    /// Fifteen-minute bars for `hours` hours, starting at the given local hour.
    fn bars_from(hour: u32, hours: i64) -> Vec<(i64, f64)> {
        let start = Local
            .with_ymd_and_hms(2026, 10, 2, hour, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        (0..hours * 4)
            .map(|i| (start + i * 900, 90.0 + (i as f64 * 0.05).sin()))
            .collect()
    }

    #[test]
    fn the_time_ticks_land_on_the_clock_and_keep_every_midnight() {
        let mut window = quote(90.0, 90.0);
        // Eighteen hundred to six in the morning, crossing local midnight once.
        window.series = bars_from(18, 12);
        let ticks = vertical_ticks(&window, TIME_TICKS);
        assert!(!ticks.is_empty());
        // Ascending, and a whole number of hours apart.
        assert!(
            ticks
                .windows(2)
                .all(|pair| pair[0].seconds < pair[1].seconds)
        );
        // Every line is a whole hour, and the one midnight is marked.
        assert!(
            ticks
                .iter()
                .all(|tick| tick.at.format("%M").to_string() == "00")
        );
        assert_eq!(ticks.iter().filter(|tick| tick.midnight).count(), 1);
    }

    #[test]
    fn a_midnight_is_kept_even_when_the_hour_step_is_coarse() {
        let mut window = quote(90.0, 90.0);
        // Three days, so the hour step is coarser than an hour and thins the
        // labels — but the midnights must survive the thinning.
        window.series = bars_from(12, 72);
        let ticks = vertical_ticks(&window, TIME_TICKS);
        assert_eq!(ticks.iter().filter(|tick| tick.midnight).count(), 3);
        assert!(
            ticks
                .iter()
                .all(|tick| tick.at.format("%M").to_string() == "00")
        );
    }

    #[test]
    fn a_window_shorter_than_an_hour_has_no_time_ticks() {
        let mut window = quote(90.0, 90.0);
        window.series = bars_from(10, 0);
        assert!(vertical_ticks(&window, TIME_TICKS).is_empty());
    }

    #[test]
    fn the_caption_names_the_symbol_and_the_change() {
        let falling = caption(&quote(90.21, 92.40));
        assert!(falling.contains("Crude Oil Nov 26"), "{falling}");
        assert!(falling.contains("▼"), "a fall is marked down: {falling}");
        assert!(falling.contains("-2.19"), "{falling}");
        assert!(falling.contains("-2.37%"), "{falling}");

        let rising = caption(&quote(92.40, 90.21));
        assert!(rising.contains("▲"), "{rising}");
    }

    #[test]
    fn ticks_land_on_round_numbers_within_the_range() {
        let ticks = nice_ticks(90.0, 94.0, 7.0);
        assert!(ticks.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            ticks.iter().all(|tick| (90.0..=94.0).contains(tick)),
            "{ticks:?}"
        );
        // A 1/2/5 step, so the labels read as round numbers.
        let step = ticks[1] - ticks[0];
        assert!(
            (step - 0.5).abs() < 1e-9 || (step - 1.0).abs() < 1e-9,
            "{ticks:?}"
        );
    }

    #[test]
    fn decimals_follow_the_size_of_the_numbers() {
        assert_eq!(tick_decimals(0.5), 1);
        assert_eq!(tick_decimals(20.0), 0);
        assert_eq!(tick_decimals(0.002), 3);
        assert_eq!(price_decimals(1.126), 4);
        assert_eq!(price_decimals(90.21), 2);
    }

    #[test]
    fn headline_prices_use_thousands_separators() {
        assert_eq!(grouped_price(2676.04, 2), "2,676.04");
        assert_eq!(grouped_price(-12345.6, 2), "-12,345.60");
        assert_eq!(grouped_price(90.0, 0), "90");
    }

    #[test]
    fn chart_clock_is_wib_regardless_of_the_host_timezone() {
        let timestamp = Utc
            .with_ymd_and_hms(2026, 10, 2, 8, 18, 17)
            .unwrap()
            .timestamp();
        assert_eq!(
            wib_time(timestamp).format("%H:%M:%S").to_string(),
            "15:18:17"
        );
    }
}
