//! `!remind`: a message the bot posts later.
//!
//! Structured like `!group`: a `subcommand_required` parent that is a table of
//! contents, not a command, so a bare `!remind` tells you it needs a subcommand
//! and the parent body never runs. `set` stores a reminder, `list` shows the
//! ones this chat has set, and `cancel <id>` removes one — all three scoped to
//! the chat they are run in.

use chrono::{Duration, Utc};
use megumi::{Error, command};

use crate::Context;
use crate::reminders::{humanize, parse_duration};

/// Sets a reminder, lists the ones already set, or cancels one.
#[command(
    name = "remind",
    aliases("reminder"),
    subcommand_required,
    subcommands(set, list, cancel)
)]
async fn remind(ctx: Context) -> Result<(), Error> {
    let _ = ctx;
    Ok(())
}

/// Sets a reminder to be posted later.
#[command(name = "set")]
async fn set(ctx: Context, #[rest] args: &str) -> Result<(), Error> {
    let args = args.trim();
    let Some((when, text)) = args.split_once(char::is_whitespace) else {
        return ctx
            .say("Usage: `!remind set <when> <what>` — for example `!remind set 10m take a break`.")
            .await;
    };
    let Some(delay) = parse_duration(when) else {
        return ctx
            .say(format!(
                "I don't understand `{when}`. Try `10m`, `1h30m`, or `2d`."
            ))
            .await;
    };
    let text = text.trim();
    if text.is_empty() {
        return ctx.say("What should I remind you about?").await;
    }

    let chat = chat(&ctx);
    let due = Utc::now() + Duration::seconds(delay.as_secs() as i64);
    let reminder = ctx
        .data()
        .reminders
        .add(&chat, due, text)
        .map_err(|error| format!("Failed to save the reminder: {error}"))?;

    ctx.say(format!(
        "⏰ Okay — I'll remind you in {}.\n_(id `{}`)_",
        humanize(delay),
        short_id(&reminder.id)
    ))
    .await
}

/// Lists the reminders set in this chat.
#[command(name = "list")]
async fn list(ctx: Context) -> Result<(), Error> {
    let reminders = ctx
        .data()
        .reminders
        .list(&chat(&ctx))
        .map_err(|error| format!("Failed to read the reminders: {error}"))?;

    if reminders.is_empty() {
        return ctx.say("There are no reminders set here.").await;
    }

    let mut body = String::from("⏰ *Reminders*\n");
    for reminder in &reminders {
        let left = reminder
            .due
            .signed_duration_since(Utc::now())
            .to_std()
            .map(humanize)
            .unwrap_or_else(|_| "now".to_string());
        body.push_str(&format!(
            "\n`{}` — in {left}: {}",
            short_id(&reminder.id),
            reminder.text
        ));
    }
    ctx.say(body).await
}

/// Cancels a reminder by id.
#[command(name = "cancel")]
async fn cancel(ctx: Context, #[rest] id: &str) -> Result<(), Error> {
    let id = id.trim();
    if id.is_empty() {
        return ctx
            .say("Usage: `!remind cancel <id>` — `!remind list` shows each id.")
            .await;
    }

    let reply = match ctx
        .data()
        .reminders
        .cancel(&chat(&ctx), id)
        .map_err(|error| format!("Failed to cancel the reminder: {error}"))?
    {
        crate::reminders::store::CancelOutcome::Cancelled => "Cancelled.".to_string(),
        crate::reminders::store::CancelOutcome::NoMatch => {
            "No reminder here has that id.".to_string()
        }
        crate::reminders::store::CancelOutcome::Ambiguous { count } => format!(
            "That id matches {count} reminders. Use more of the id — `!remind list` shows each."
        ),
    };
    ctx.say(reply).await
}

/// This chat's id, as the reminder store keys it.
fn chat(ctx: &Context) -> String {
    ctx.message.info.source.chat.to_non_ad_string()
}

/// The first eight characters of an id, for display.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}
