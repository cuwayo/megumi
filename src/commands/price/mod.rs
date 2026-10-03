//! `!price`: chart a symbol's last 24 hours on demand.
//!
//! The same chart the weekly update posts, rendered when it is asked for. With no
//! symbol the group's first watched one is used, so a group that subscribed with
//! `!group price on` gets its default by typing `!price` alone.

use megumi::{CreateAttachment, CreateReply, Error, command};

use crate::Context;
use crate::price::store::DEFAULT_SYMBOL;
use crate::price::{chart, market};

/// Charts a symbol's last 24 hours.
///
/// Names any symbol Yahoo Finance knows: `!price CL=F` (WTI crude, the default),
/// `!price GC=F`, `!price BTC-USD`. With no symbol, the group's first watched one
/// is charted.
#[command(name = "price", aliases("px"), react = "📈")]
async fn price(ctx: Context, #[rest] args: &str) -> Result<(), Error> {
    // The first word is the symbol; anything after it is ignored, the way
    // `!download` and `!shazam` read a caption.
    let symbol = args
        .split_whitespace()
        .next()
        .map(str::to_string)
        .unwrap_or_else(|| watched_symbol(&ctx));

    let http = reqwest::Client::builder()
        .user_agent("megumi-whatsapp")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let quote = market::fetch(&http, &symbol)
        .await
        .map_err(|error| format!("Could not chart `{symbol}`: {error}"))?;

    let png = chart::render(&quote, chrono::Local::now())?;
    let caption = chart::caption(&quote);
    let thumbnail = chart::thumbnail(&png).ok();

    ctx.send(
        CreateReply::new()
            .content(caption)
            .attachment(CreateAttachment::image(png).thumbnail(thumbnail))
            .reply(true),
    )
    .await
}

/// The symbol this group watches first, or [`DEFAULT_SYMBOL`] when it watches none.
///
/// A store read that fails falls back to the default rather than refusing the
/// command: the chart is still useful, and the store's own errors are reported
/// where a setting is changed.
fn watched_symbol(ctx: &Context) -> String {
    ctx.data()
        .price
        .symbols(&ctx.message.info.source.chat.to_string())
        .ok()
        .and_then(|symbols| symbols.into_iter().next())
        .unwrap_or_else(|| DEFAULT_SYMBOL.to_string())
}
