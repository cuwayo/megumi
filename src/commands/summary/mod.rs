//! `!summary`: a short summary of the chat so far, written by the model.
//!
//! The summary is stored, so it also becomes the conversation's rolling summary
//! — the layer the agent's context builder carries into later turns.

use megumi::{Error, command};

use crate::Context;

/// Summarises the conversation so far.
#[command(
    name = "summary",
    aliases("summarise", "summarize"),
    channel_cooldown = 30
)]
async fn summary(ctx: Context) -> Result<(), Error> {
    let chat = megumi_agent::ChatId::new(ctx.message.info.source.chat.to_non_ad_string());
    let chat_type = if ctx.message.info.source.is_group {
        megumi_agent::ChatType::Group
    } else {
        megumi_agent::ChatType::Private
    };

    match ctx.data().agent.summarize(&chat, chat_type).await {
        Ok(Some(summary)) => ctx.say(summary).await,
        Ok(None) => ctx.say("There's nothing here to summarise yet.").await,
        Err(error) => Err(error.to_string().into()),
    }
}
