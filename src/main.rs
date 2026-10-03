use std::sync::Arc;

use qrcode::render::unicode;
use tracing::{info, instrument};
use tracing_subscriber::EnvFilter;
use whatsapp_rust::prelude::*;

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
    let framework = framework();
    let data = framework.user_data().clone();
    // The loops a connection started, so a reconnect can stop each before
    // starting its replacement. `Connected` fires again on every reconnect, and
    // the loops run forever, so without this each reconnect would leave another
    // loop running beside the new one.
    let news_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let price_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    let bot = Bot::builder()
        .with_backend(store)
        .on_connected(move |client| {
            let data = data.clone();
            let news_task = news_task.clone();
            let price_task = price_task.clone();
            async move {
                info!("Connected with WhatsApp");
                // Stop the previous connection's loops and wait for them to be
                // gone before starting the new ones, so only one of each ever
                // runs. A digest or a chart already delivered is recorded on
                // disk, so restarting a loop does not resend it.
                {
                    let mut task = news_task.lock().await;
                    if let Some(previous) = task.take() {
                        previous.abort();
                        let _ = previous.await;
                    }
                    *task = Some(tokio::spawn(megumi_whatsapp::news::run(
                        client.clone(),
                        data.clone(),
                    )));
                }
                let mut task = price_task.lock().await;
                if let Some(previous) = task.take() {
                    previous.abort();
                    let _ = previous.await;
                }
                *task = Some(tokio::spawn(megumi_whatsapp::price::run(client, data)));
            }
        })
        .on_qr_code(|code, timeout| async move {
            let qr = qrcode::QrCode::new(code).expect("WhatsApp QR payload should encode");
            let image = qr.render::<unicode::Dense1x2>().quiet_zone(true).build();

            info!(timeout = timeout.as_secs(), "Scan this QR code to log in");
            info!("{image}");
        })
        .on_message(megumi::install(framework))
        .build()
        .await?;

    info!("WhatsApp bot is running");
    bot.run().await;
    Ok(())
}
