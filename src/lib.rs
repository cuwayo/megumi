//! The commands this bot serves, and the framework they are registered with.
//!
//! `main` wires this registry into the WhatsApp client; the integration tests
//! under `tests/` drive the same registry.

use std::sync::Arc;
use std::time::Instant;

use megumi::Framework;

pub mod agent;
pub mod commands;
mod data;
pub mod events;
pub mod news;
pub mod reminders;

pub use data::Data;
pub use news::store::NewsStore;
pub use reminders::store::ReminderStore;

/// The context every command takes: the framework's [`Context`](megumi::Context)
/// carrying the bot's [`Data`].
pub type Context = megumi::Context<Data>;

/// The bot's command registry: the prefix it answers to, and every group of
/// commands it serves.
///
/// Commands are declared in groups (see [`commands`]), which is what the help
/// listing groups them by. `Data::started` is pinned here, and `main` builds the
/// framework before anything else, so `!uptime` is process lifetime rather than
/// time since the first `!uptime`. The pin happens at build time, not inside the
/// async `setup` closure, so it precedes connecting.
pub fn framework() -> Framework<Data> {
    // `NEWS_DB` overrides the default of `news.json` beside the session file. Tests
    // build a framework per assertion and never send a digest, so they get an
    // in-memory database instead of one on disk.
    let path = std::env::var("NEWS_DB").unwrap_or_else(|_| {
        if cfg!(test) {
            ":memory:".to_string()
        } else {
            "news.json".to_string()
        }
    });
    // Opened here, at build time, so an unreadable store fails the process at
    // startup rather than on the first message — `setup` runs lazily, so a panic
    // there would leave the bot alive but unable to answer anything.
    let news = Arc::new(NewsStore::open(path).unwrap_or_else(|error| panic!("{error}")));
    // `REMINDER_DB` overrides the default of `reminders.json`; tests get an
    // in-memory store instead of one on disk.
    let reminder_path = std::env::var("REMINDER_DB").unwrap_or_else(|_| {
        if cfg!(test) {
            ":memory:".to_string()
        } else {
            "reminders.json".to_string()
        }
    });
    let reminders =
        Arc::new(ReminderStore::open(reminder_path).unwrap_or_else(|error| panic!("{error}")));
    let started = Instant::now();
    let agent = build_agent();
    // The media-understanding provider is optional: without a credential it is
    // `None` and the adapter describes nothing, so media reaches the model as
    // its kind alone.
    let media = agent::media::OpenAiMedia::from_env();
    if media.is_none() {
        tracing::warn!(
            "no MEDIA_API_KEY or OPENAI_API_KEY is set; voice notes and images will not be \
             described"
        );
    }

    Framework::builder()
        .setup(move |_client| {
            let news = Arc::clone(&news);
            let reminders = Arc::clone(&reminders);
            let agent = Arc::clone(&agent);
            async move {
                Ok(Data {
                    started,
                    news,
                    news_task: tokio::sync::Mutex::new(None),
                    reminders,
                    remind_task: tokio::sync::Mutex::new(None),
                    agent,
                    media,
                })
            }
        })
        .prefix("!")
        .event_handler(events::event_handler)
        .groups([
            commands::utility(),
            commands::media(),
            commands::admin(),
            commands::owner(),
            commands::assistant(),
        ])
        .build()
}

/// Builds the agent and everything it owns, at framework-build time.
///
/// The message store, the memory store, and the trace log are opened here, so a
/// corrupt file fails startup. The model is optional: without
/// `ANTHROPIC_API_KEY` the agent still stores every message but cannot reply,
/// and that is a warning, not an error.
fn build_agent() -> Arc<megumi_agent::Agent> {
    let config = megumi_agent::AgentConfig::from_env();

    let store = Arc::new(
        megumi_agent::MessageStore::open(config.chats_dir(), config.max_stored_messages)
            .unwrap_or_else(|error| panic!("{error}")),
    );
    let memory = Arc::new(
        megumi_agent::MemoryStore::open(config.memory_path())
            .unwrap_or_else(|error| panic!("{error}")),
    );
    let traces = Arc::new(
        megumi_agent::TraceSink::open(config.trace_path(), config.trace_capacity)
            .unwrap_or_else(|error| panic!("{error}")),
    );

    let llm: Arc<dyn megumi_agent::LlmClient> =
        match megumi_agent::AnthropicLlm::from_env(&config.api_base) {
            Some(llm) => Arc::new(llm),
            None => {
                tracing::warn!(
                    "no ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY is set; the agent will store \
                     messages but not reply"
                );
                Arc::new(megumi_agent::DisabledLlm)
            }
        };

    // Web search is optional: without `TAVILY_API_KEY` the tool is simply
    // absent, and `search_memory` is the only tool a turn may call.
    let mut tools: Vec<Arc<dyn megumi_agent::Tool>> = Vec::new();
    if let Some(web_search) = megumi_agent::WebSearch::from_env(&config) {
        tools.push(Arc::new(web_search));
    }

    Arc::new(megumi_agent::Agent::new(
        store,
        memory,
        traces,
        llm,
        Arc::new(megumi_agent::ToolRegistry::new(tools)),
        config,
    ))
}
