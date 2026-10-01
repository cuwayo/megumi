use megumi::{Error, command};

use crate::Context;

/// Repeats the text after the command.
#[command(name = "echo")]
async fn echo(ctx: Context, #[rest] text: &str) -> Result<(), Error> {
    if text.trim().is_empty() {
        return Err("You need to provide text after `!echo`.".into());
    }
    ctx.say(text).await
}
