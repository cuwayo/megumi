//! `!forget`: drop a fact the assistant stored for this chat.
//!
//! A plain operation — no model call. It takes the short id `!memory` shows, or
//! the word `all` to clear the chat's memory. A forgotten fact is never recalled
//! again, not even for a question about the past.

use megumi::{Error, command};

use crate::Context;

/// Forgets a stored fact, by id, or everything with `all`.
#[command(name = "forget", aliases("unremember"))]
async fn forget(ctx: Context, #[rest] target: &str) -> Result<(), Error> {
    let target = target.trim();
    if target.is_empty() {
        return ctx
            .say("Usage: `!forget <id>` — the id `!memory` shows — or `!forget all`.")
            .await;
    }

    let event = crate::agent::event_from_context(&ctx.message).await;
    let reply = match ctx
        .data()
        .agent
        .forget_memory(&event, target)
        .await
        .map_err(|error| error.to_string())?
    {
        megumi_agent::ForgetOutcome::Forgotten { count: 1 } => "Forgotten it.".to_string(),
        megumi_agent::ForgetOutcome::Forgotten { count } => format!("Forgotten {count} facts."),
        megumi_agent::ForgetOutcome::NoMatch => {
            "I don't have a fact with that id for this chat.".to_string()
        }
        megumi_agent::ForgetOutcome::Ambiguous { count } => {
            format!("That id matches {count} facts. Use more of the id — `!memory` shows each one.")
        }
    };
    ctx.say(reply).await
}
