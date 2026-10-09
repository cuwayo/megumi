//! The reminder scheduler.
//!
//! `!remind set 10m stretch` stores a reminder; once its time comes, the bot
//! posts it back to the chat. The loop mirrors the news digest: it starts when
//! WhatsApp connects and stops when it drops, and the reminders live in a JSON
//! file ([`store`]) so they survive a restart.

mod parse;
pub mod store;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use megumi::{BoxFuture, Error, Event, FrameworkContext};
use tracing::{info, warn};
use whatsapp_rust::Client;

use crate::data::Data;
use crate::reminders::store::ReminderStore;

pub use parse::{humanize, parse_duration};

/// How often the loop wakes to see whether a reminder is due.
const TICK: Duration = Duration::from_secs(30);

/// Reacts to the connection lifecycle by starting or stopping the reminder loop.
///
/// This is the reminders' half of the framework's single [`EventHook`], run
/// alongside the news digest and the agent. On `Connected` it stops the previous
/// connection's loop before starting its replacement, so a reconnect never
/// leaves two loops running; on `Disconnected` or `LoggedOut` it stops the loop,
/// so a dropped session does not leave a loop ticking against a dead client.
///
/// A `Connected` delivered late — its handler still waiting on the lock when a
/// later `Disconnected` has already run — starts nothing: it re-checks the
/// client's connected flag under the lock, so it cannot resurrect the loop after
/// the disconnect stopped it. This is the same discipline the news digest uses,
/// and for the same reasons.
///
/// [`EventHook`]: megumi::EventHook
pub fn event_handler(
    ctx: FrameworkContext<Data>,
    event: Arc<Event>,
) -> BoxFuture<Result<(), Error>> {
    Box::pin(async move {
        match &*event {
            Event::Connected(_) => {
                let mut slot = ctx.data.remind_task.lock().await;
                // Re-check under the lock: delivery is concurrent, so this
                // handler may have been scheduled after a later `Disconnected`
                // ran.
                if !ctx.client.is_connected() {
                    return Ok(());
                }
                stop(&mut slot).await;
                *slot = Some(tokio::spawn(run(
                    ctx.client.clone(),
                    Arc::clone(&ctx.data.reminders),
                )));
            }
            Event::Disconnected(_) | Event::LoggedOut(_) => {
                let mut slot = ctx.data.remind_task.lock().await;
                stop(&mut slot).await;
            }
            _ => {}
        }
        Ok(())
    })
}

/// Stops the loop in `slot`, aborting it and waiting for it to finish, and
/// leaves the slot empty.
async fn stop(slot: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(previous) = slot.take() {
        previous.abort();
        let _ = previous.await;
    }
}

/// Posts every due reminder, once per tick.
///
/// A reminder is removed only after its message lands, so a failure is retried
/// on a later tick and a reminder is never lost. The store lock is never held
/// across the send: the due set is read, the lock is dropped, then each message
/// is sent and the reminder marked fired.
pub async fn run(client: Arc<Client>, reminders: Arc<ReminderStore>) {
    loop {
        deliver(&client, &reminders).await;
        tokio::time::sleep(TICK).await;
    }
}

/// Sends every reminder that is due now.
async fn deliver(client: &Client, reminders: &ReminderStore) {
    let due = match reminders.due(Utc::now()) {
        Ok(due) => due,
        Err(error) => {
            warn!(%error, "could not read the due reminders");
            return;
        }
    };

    for reminder in due {
        let Ok(jid) = reminder.chat.parse::<whatsapp_rust::Jid>() else {
            warn!(
                chat = reminder.chat,
                "skipping a reminder with a bad chat id"
            );
            // A bad id would fail every tick, so drop it rather than spin.
            if let Err(error) = reminders.mark_fired(&reminder.id) {
                warn!(%error, "could not drop a reminder with a bad chat id");
            }
            continue;
        };
        let message = format!("⏰ Reminder: {}", reminder.text);
        match client.send_text(jid, message).await {
            Ok(_) => {
                if let Err(error) = reminders.mark_fired(&reminder.id) {
                    warn!(%error, "the reminder was sent but could not be recorded");
                }
                info!(chat = reminder.chat, "sent a reminder");
            }
            Err(error) => warn!(%error, chat = reminder.chat, "failed to send a reminder"),
        }
    }
}
