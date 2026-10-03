//! Fetching a symbol's last 24 hours from Yahoo Finance, and reading the chart
//! out of the response.
//!
//! Yahoo's public chart endpoint needs no key and covers futures, crypto,
//! equities, and FX, which is why the price update can chart any symbol a user
//! names. It is unofficial, so the parse is defensive: an unknown symbol comes
//! back as a populated error rather than a failure status, and every figure the
//! chart wants is optional and falls back to something sensible.

use serde_json::Value;

use super::models::{ChartResult, Response};

/// The chart request: two days at fifteen-minute bars.
///
/// Yahoo's `1d` range is the current session, not a rolling day — a few hours for
/// a future like WTI — so a "24h" chart asks for two days and trims to the last
/// 24 hours below. Fifteen minutes is fine enough for the line and keeps the
/// response small.
const RANGE: &str = "2d";
const INTERVAL: &str = "15m";

/// The window the chart covers, in seconds.
const WINDOW_SECS: i64 = 24 * 60 * 60;

/// How long to wait on Yahoo before giving up, so a stalled connection cannot
/// hold the scheduler loop open.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// One symbol's snapshot: what the header shows and the intraday line.
#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    /// The display name, e.g. "Crude Oil Nov 26".
    pub name: String,
    /// The symbol as requested, used when Yahoo gives no name.
    pub symbol: String,
    /// The currency the price is quoted in, e.g. "USD".
    pub currency: String,
    /// The latest price.
    pub last: f64,
    /// The price 24 hours ago, which the change is measured against.
    pub prev_close: f64,
    /// The day's high.
    pub day_high: f64,
    /// The day's low.
    pub day_low: f64,
    /// The intraday close series, as `(unix seconds, price)`, gaps dropped.
    pub series: Vec<(i64, f64)>,
}

impl Quote {
    /// The move over the window, as a price and a percentage.
    pub fn change(&self) -> (f64, f64) {
        let change = self.last - self.prev_close;
        let percent = if self.prev_close == 0.0 {
            0.0
        } else {
            change / self.prev_close * 100.0
        };
        (change, percent)
    }

    /// The window's opening price, the first point that was actually traded.
    pub fn open(&self) -> f64 {
        self.series
            .first()
            .map(|(_, price)| *price)
            .unwrap_or(self.last)
    }
}

/// Fetches `symbol`'s 24-hour chart.
pub async fn fetch(http: &reqwest::Client, symbol: &str) -> Result<Quote, String> {
    let url = format!(
        "https://query1.finance.yahoo.com/v8/finance/chart/{}?interval={INTERVAL}&range={RANGE}",
        urlencoding(symbol)
    );

    let response = http
        .get(&url)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("Could not reach Yahoo Finance: {error}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|error| {
        format!("Yahoo Finance returned an unreadable response ({status}): {error}")
    })?;

    parse(&body, symbol)
}

/// Reads a [`Quote`] out of a Yahoo chart response.
///
/// Separated from [`fetch`] so the response shape is tested without a network
/// call. A response with no result — an unknown symbol — carries Yahoo's own
/// description, which is more useful than a generic message.
pub fn parse(body: &Value, symbol: &str) -> Result<Quote, String> {
    let response: Response = serde_json::from_value(body.clone())
        .map_err(|error| format!("Unexpected Yahoo Finance shape: {error}"))?;

    let result = response
        .chart
        .result
        .and_then(|mut results| (!results.is_empty()).then(|| results.remove(0)))
        .ok_or_else(|| {
            response
                .chart
                .error
                .and_then(|error| error.description)
                .unwrap_or_else(|| {
                    format!("Yahoo Finance has no data for `{}`.", symbol.to_uppercase())
                })
        })?;

    let series = window(series_of(&result));
    let closes: Vec<f64> = series.iter().map(|(_, price)| *price).collect();
    let meta = &result.meta;

    // The header describes the 24-hour window, not Yahoo's own day, so the high
    // and low are the window's and the previous close is its first price. That
    // makes the change the move over the last day, which is what the card claims.
    let last = meta
        .regular_market_price
        .or_else(|| closes.last().copied())
        .ok_or_else(|| format!("Yahoo Finance reported no price for `{symbol}`."))?;
    let prev_close = closes
        .first()
        .copied()
        .or(meta.chart_previous_close)
        .or(meta.previous_close)
        .unwrap_or(last);

    let day_high = closes.iter().copied().reduce(f64::max).unwrap_or(last);
    let day_low = closes.iter().copied().reduce(f64::min).unwrap_or(last);

    Ok(Quote {
        name: meta
            .short_name
            .clone()
            .or_else(|| meta.long_name.clone())
            .unwrap_or_else(|| symbol.to_uppercase()),
        symbol: symbol.to_uppercase(),
        currency: meta.currency.clone().unwrap_or_else(|| "USD".to_string()),
        last,
        prev_close,
        day_high,
        day_low,
        series,
    })
}

/// The tail of `series` that falls inside the last 24 hours of it.
///
/// Yahoo is asked for two days so there is always at least a day of data even
/// over a weekend gap; this keeps the newest [`WINDOW_SECS`] and drops the rest.
/// A series shorter than the window is left as it is.
fn window(series: Vec<(i64, f64)>) -> Vec<(i64, f64)> {
    let Some((end, _)) = series.last().copied() else {
        return series;
    };
    let start = end - WINDOW_SECS;
    series
        .into_iter()
        .filter(|(time, _)| *time >= start)
        .collect()
}

/// Pairs each timestamp with its close, dropping the gaps in either.
fn series_of(result: &ChartResult) -> Vec<(i64, f64)> {
    let timestamps = result.timestamp.as_deref().unwrap_or_default();
    let closes = result
        .indicators
        .quote
        .first()
        .and_then(|quote| quote.close.as_deref())
        .unwrap_or_default();

    timestamps
        .iter()
        .zip(closes)
        .filter_map(|(time, close)| close.map(|close| (*time, close)))
        .collect()
}

/// Percent-encode a symbol for the URL path. `^GSPC` and `DX-Y.NYB` are the
/// shapes that need it.
fn urlencoding(symbol: &str) -> String {
    symbol
        .chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                vec![c.to_string()]
            } else {
                c.to_string().bytes().map(|b| format!("%{b:02X}")).collect()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Value {
        serde_json::json!({
            "chart": {
                "result": [{
                    "meta": {
                        "shortName": "Crude Oil Nov 26",
                        "currency": "USD",
                        "regularMarketPrice": 90.21,
                        "chartPreviousClose": 92.40
                    },
                    "timestamp": [1000, 1300, 1600, 1900],
                    "indicators": { "quote": [{ "close": [92.4, null, 91.1, 90.21] }] }
                }],
                "error": null
            }
        })
    }

    #[test]
    fn a_response_yields_the_header_and_the_series() {
        let quote = parse(&body(), "CL=F").unwrap();
        assert_eq!(quote.name, "Crude Oil Nov 26");
        assert_eq!(quote.currency, "USD");
        assert_eq!(quote.last, 90.21);
        // The header describes the window: the first close is the previous
        // close, and the high and low are the window's own.
        assert_eq!(quote.prev_close, 92.40);
        assert_eq!(quote.day_high, 92.40);
        assert_eq!(quote.day_low, 90.21);
        // The null close is dropped, leaving the points that traded.
        assert_eq!(quote.series, [(1000, 92.4), (1600, 91.1), (1900, 90.21)]);
    }

    #[test]
    fn the_window_keeps_only_the_last_day() {
        let day = 86_400;
        // The first point is more than a day before the last, so it is dropped.
        let series = vec![(-100, 1.0), (day / 2, 2.0), (day - 10, 3.0), (day, 4.0)];
        assert_eq!(
            window(series.clone()),
            [(day / 2, 2.0), (day - 10, 3.0), (day, 4.0)]
        );
        // A series shorter than a day is left alone.
        assert_eq!(window(vec![(0, 1.0), (60, 2.0)]), [(0, 1.0), (60, 2.0)]);
        assert!(window(Vec::new()).is_empty());
    }

    #[test]
    fn the_change_is_measured_against_the_previous_close() {
        let quote = parse(&body(), "CL=F").unwrap();
        let (change, percent) = quote.change();
        assert!((change + 2.19).abs() < 1e-9, "{change}");
        assert!((percent + 2.3701).abs() < 1e-3, "{percent}");
    }

    #[test]
    fn a_missing_price_falls_back_to_the_last_close() {
        let mut value = body();
        value["chart"]["result"][0]["meta"]
            .as_object_mut()
            .unwrap()
            .remove("regularMarketPrice");
        let quote = parse(&value, "CL=F").unwrap();
        assert_eq!(quote.last, 90.21, "the last traded close stands in");
    }

    #[test]
    fn an_unknown_symbol_reports_yahoos_own_description() {
        let value = serde_json::json!({
            "chart": {
                "result": null,
                "error": { "description": "No data found, symbol may be delisted" }
            }
        });
        let error = parse(&value, "NOPE").unwrap_err();
        assert!(error.contains("may be delisted"), "{error}");
    }

    #[test]
    fn a_response_with_neither_result_nor_error_still_names_the_symbol() {
        let value = serde_json::json!({ "chart": { "result": null } });
        let error = parse(&value, "nope").unwrap_err();
        assert!(error.contains("NOPE"), "{error}");
    }

    #[test]
    fn a_symbol_with_no_name_falls_back_to_the_symbol() {
        let value = serde_json::json!({
            "chart": {
                "result": [{
                    "meta": { "regularMarketPrice": 5.0, "chartPreviousClose": 4.0 },
                    "indicators": { "quote": [{ "close": [4.0, 5.0] }] }
                }]
            }
        });
        let quote = parse(&value, "abc").unwrap();
        assert_eq!(quote.name, "ABC");
        assert_eq!(quote.currency, "USD", "a missing currency reads as USD");
    }
}
