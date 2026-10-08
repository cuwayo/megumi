//! The bot's single framework event hook: one event, fanned out to the
//! subsystems that each want a different slice of it.
//!
//! The framework allows exactly one [`EventHook`](megumi::EventHook), but the
//! news digest and the agent both need one. This is that one hook: it rebuilds
//! the [`FrameworkContext`](megumi::FrameworkContext) for each consumer — its
//! fields are public — and runs them in turn.

use std::sync::Arc;

use megumi::{BoxFuture, Error, Event, FrameworkContext};
use tracing::error;

use crate::data::Data;

/// Fans every event out to the news digest and the agent.
///
/// News runs first, and a failure there is logged here rather than returned, so
/// an agent problem can never stop the digest loop from starting. The agent
/// only looks at `Event::Messages`; every other kind falls straight through.
pub fn event_handler(
    ctx: FrameworkContext<Data>,
    event: Arc<Event>,
) -> BoxFuture<Result<(), Error>> {
    Box::pin(async move {
        if let Err(error) = crate::news::event_handler(
            FrameworkContext {
                client: Arc::clone(&ctx.client),
                data: Arc::clone(&ctx.data),
            },
            Arc::clone(&event),
        )
        .await
        {
            error!(%error, "the news event handler failed");
        }

        if let Some(batch) = event.as_messages() {
            crate::agent::on_messages(&ctx.data, &ctx.client, batch).await;
        }
        Ok(())
    })
}
