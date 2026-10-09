//! `!memory`: the facts the assistant has stored for this chat.
//!
//! A plain listing — no model call. Each fact is shown with a short id so
//! `!forget` can name one. Only facts this chat may see and could forget are
//! listed, so every id shown is one `!forget` can act on.

use megumi::{Error, command};

use crate::Context;

/// Lists what the assistant remembers about this chat.
#[command(name = "memory", aliases("memories", "mem"))]
async fn memory(ctx: Context) -> Result<(), Error> {
    let event = crate::agent::event_from_context(&ctx.message).await;
    let facts = ctx
        .data()
        .agent
        .list_memory(&event)
        .await
        .map_err(|error| error.to_string())?;

    if facts.is_empty() {
        return ctx
            .say("I don't have anything stored for this chat yet.")
            .await;
    }

    let mut body = String::from("🧠 *What I remember about this chat*\n");
    for record in &facts {
        let id: String = record.id.chars().take(8).collect();
        body.push_str(&format!("\n`{id}` — {}", record.content));
    }
    ctx.say(body).await
}
