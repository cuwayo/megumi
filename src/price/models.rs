//! The shape of Yahoo Finance's chart response, only as far as the chart reads it.
//!
//! The response nests everything under `chart`, with the series under
//! `result[0].indicators.quote[0]`. Every field that a working response always
//! carries is optional here anyway, because a symbol Yahoo does not know answers
//! with a populated `error` and a null `result` rather than a non-200 status.

use serde::Deserialize;

/// The top-level response: `{ "chart": { ... } }`.
#[derive(Deserialize)]
pub struct Response {
    pub chart: Chart,
}

/// The chart envelope, which holds either a result or an error.
#[derive(Deserialize)]
pub struct Chart {
    #[serde(default)]
    pub result: Option<Vec<ChartResult>>,
    #[serde(default)]
    pub error: Option<ApiError>,
}

/// Why Yahoo refused the request, e.g. an unknown symbol.
#[derive(Deserialize)]
pub struct ApiError {
    pub description: Option<String>,
}

/// One symbol's data: the metadata the header shows and the intraday series.
#[derive(Deserialize)]
pub struct ChartResult {
    pub meta: Meta,
    #[serde(default)]
    pub timestamp: Option<Vec<i64>>,
    pub indicators: Indicators,
}

/// The summary figures Yahoo reports alongside the series.
#[derive(Deserialize)]
pub struct Meta {
    #[serde(rename = "shortName")]
    pub short_name: Option<String>,
    #[serde(rename = "longName")]
    pub long_name: Option<String>,
    pub currency: Option<String>,
    #[serde(rename = "regularMarketPrice")]
    pub regular_market_price: Option<f64>,
    #[serde(rename = "chartPreviousClose")]
    pub chart_previous_close: Option<f64>,
    #[serde(rename = "previousClose")]
    pub previous_close: Option<f64>,
}

/// The indicator block, whose first quote entry holds the OHLC arrays.
#[derive(Deserialize)]
pub struct Indicators {
    pub quote: Vec<QuoteSeries>,
}

/// The parallel price arrays. A gap in trading is a `null` in each, not a shorter
/// array, so they stay index-aligned with `timestamp`.
#[derive(Deserialize)]
pub struct QuoteSeries {
    #[serde(default)]
    pub close: Option<Vec<Option<f64>>>,
}
