use megumi::{Error, command};

use crate::Context;

/// Replies with pong.
#[command(name = "ping", aliases("p"))]
async fn ping(ctx: Context) -> Result<(), Error> {
    ctx.say("pong").await
}
