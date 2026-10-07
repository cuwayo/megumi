use qrcode::render::unicode;
use tracing::{info, instrument};
use tracing_subscriber::EnvFilter;
use whatsapp_rust::prelude::*;

use megumi::FrameworkExt;
use megumi_whatsapp::framework;

#[instrument(name = "megumi", skip_all)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("megumi=info,whatsapp_rust=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();

    let store = SqliteStore::new("whatsapp.db").await?;

    // The framework drives the client: `.framework` wires its event handler, so
    // the news digest loop and the commands both run through it. Only the QR
    // callback, which has no command context, is wired here.
    let bot = Bot::builder()
        .with_backend(store)
        .framework(framework())
        .on_qr_code(|code, timeout| async move {
            let qr = qrcode::QrCode::new(code).expect("WhatsApp QR payload should encode");
            let image = qr.render::<unicode::Dense1x2>().quiet_zone(true).build();

            info!(timeout = timeout.as_secs(), "Scan this QR code to log in");
            info!("{image}");
        })
        .build()
        .await?;

    info!("WhatsApp bot is running");
    bot.run().await;
    Ok(())
}
