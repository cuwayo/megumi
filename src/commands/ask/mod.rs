//! `!ask`: a question answered by the agent, on demand.
//!
//! A command message is normally left to the command layer, so the agent stays
//! silent on it. `!ask` is the exception: it stores the question as if the user
//! had spoken it, then forces a turn through the agent, so the model answers
//! with the same context, memory, and tools a mention or a DM would get.

use megumi::{Error, command};
use tracing::warn;

use crate::Context;

/// Asks the assistant a question.
#[command(name = "ask", aliases("q"), user_cooldown = 5)]
async fn ask(ctx: Context, #[rest] question: &str) -> Result<(), Error> {
    let question = question.trim();
    if question.is_empty() {
        return ctx.say("Usage: `!ask <question>`").await;
    }

    // The event the agent sees is the question, not the `!ask` syntax, and it is
    // not a command — the agent should treat it as an ordinary trigger. The id is
    // kept, so the adapter's later ingest of the raw `!ask ...` message is a
    // no-op and the question is stored exactly once.
    let mut event = crate::agent::event_from_context(&ctx.message).await;
    event.text = Some(question.to_string());
    event.is_command = false;

    if let Err(error) = ctx.data().agent.ingest(&event) {
        warn!(%error, "the agent could not store a question");
    }

    let reply = match ctx.data().agent.answer_command(&event).await {
        Ok(Some(megumi_agent::OutboundAction::SendText { text, .. })) => text,
        Ok(None) => "I couldn't come up with an answer to that.".to_string(),
        Err(error) => return Err(error.to_string().into()),
    };
    ctx.say(reply).await
}
