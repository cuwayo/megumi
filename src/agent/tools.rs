//! The bot-side tools the agent may call, which need the bot's own stores.
//!
//! The agent core ships the tools that are self-contained — [`SearchMemory`] and
//! `web_search` — but a tool that writes to one of the bot's stores lives here,
//! beside the store. [`SetReminder`] is the first: it schedules a reminder in the
//! chat the turn is in, so the model can act on "remind me tomorrow" without the
//! user typing `!remind`.
//!
//! It is also the first *state-changing* tool in production. It overrides
//! [`Tool::confirmation`], so the agent holds the call and runs it only after the
//! user agrees — the milestone-6 gate, now exercised by a real tool rather than
//! only by a test double.
//!
//! [`SearchMemory`]: megumi_agent::SearchMemory

use std::sync::Arc;

use chrono::{Duration, Utc};
use megumi_agent::llm::BoxFuture;
use megumi_agent::{Tool, ToolContext, ToolSpec};

use crate::reminders::store::ReminderStore;
use crate::reminders::{humanize, parse_duration};

/// The tool that sets a reminder in the turn's chat.
pub struct SetReminder {
    store: Arc<ReminderStore>,
}

impl SetReminder {
    /// A tool over `store`.
    pub fn new(store: Arc<ReminderStore>) -> Self {
        Self { store }
    }
}

impl Tool for SetReminder {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "set_reminder".to_string(),
            description: "Set a reminder that the bot will post back to this chat later. Use \
                          this when someone asks to be reminded of something at a future time."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "delay": {
                        "type": "string",
                        "description": "How far ahead to remind, as a compact duration: \
                                        `30m`, `2h`, `1h30m`, `2d`. A bare number is minutes."
                    },
                    "text": {
                        "type": "string",
                        "description": "What to remind them about, in their own words."
                    }
                },
                "required": ["delay", "text"]
            }),
        }
    }

    fn call(
        &self,
        context: &ToolContext,
        arguments: &serde_json::Value,
    ) -> BoxFuture<Result<String, String>> {
        // The future is `'static`, so it owns what it needs rather than
        // borrowing the arguments, the store, or the context.
        let delay = arguments
            .get("delay")
            .and_then(|delay| delay.as_str())
            .map(str::to_string);
        let text = arguments
            .get("text")
            .and_then(|text| text.as_str())
            .map(str::to_string);
        let store = Arc::clone(&self.store);
        let chat = context.chat().as_str().to_string();
        Box::pin(async move {
            let delay = delay.ok_or_else(|| "set_reminder needs a `delay` argument".to_string())?;
            let text = text
                .ok_or_else(|| "set_reminder needs a `text` argument".to_string())?
                .trim()
                .to_string();
            if text.is_empty() {
                return Err("the reminder text is empty".to_string());
            }
            let duration = parse_duration(&delay)
                .ok_or_else(|| format!("`{delay}` is not a duration; try `30m`, `2h`, or `2d`"))?;
            let due = Utc::now() + Duration::seconds(duration.as_secs() as i64);
            let reminder = store.add(&chat, due, &text)?;
            Ok(format!(
                "Reminder set for {} from now: \"{text}\" (id {}).",
                humanize(duration),
                reminder.id.chars().take(8).collect::<String>()
            ))
        })
    }

    fn confirmation(&self, arguments: &serde_json::Value) -> Option<String> {
        let delay = arguments.get("delay").and_then(|delay| delay.as_str());
        let text = arguments.get("text").and_then(|text| text.as_str());
        // If the arguments are unusable the call will fail anyway, so there is
        // nothing to confirm; the question is only asked for a call that could
        // actually run.
        let (delay, text) = (delay?, text?);
        let when = parse_duration(delay)
            .map(humanize)
            .unwrap_or_else(|| delay.to_string());
        Some(format!("Set a reminder to \"{text}\" in {when}?"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use megumi_agent::{ChatId, ChatType, ReaderContext, SenderId};

    fn context(chat: &str) -> ToolContext {
        ToolContext::new(ReaderContext {
            chat: ChatId::new(chat),
            chat_type: ChatType::Group,
            requester: SenderId::new("u1"),
            member_of: vec![ChatId::new(chat)],
        })
    }

    #[test]
    fn the_spec_advertises_a_delay_and_text() {
        let tool = SetReminder::new(Arc::new(ReminderStore::open(":memory:").unwrap()));
        let spec = tool.spec();
        assert_eq!(spec.name, "set_reminder");
        assert_eq!(spec.parameters["required"][0], "delay");
        assert_eq!(spec.parameters["required"][1], "text");
    }

    #[test]
    fn the_confirmation_names_the_text_and_the_delay() {
        let tool = SetReminder::new(Arc::new(ReminderStore::open(":memory:").unwrap()));
        let question = tool
            .confirmation(&serde_json::json!({ "delay": "10m", "text": "take a break" }))
            .expect("a state-changing call is confirmed");
        assert!(question.contains("take a break"), "{question}");
        assert!(question.contains("10 minutes"), "{question}");
        // A call the model botched cannot run, so there is nothing to confirm.
        assert!(tool.confirmation(&serde_json::json!({})).is_none());
    }

    #[tokio::test]
    async fn the_call_sets_a_reminder_in_the_turns_chat() {
        let store = Arc::new(ReminderStore::open(":memory:").unwrap());
        let tool = SetReminder::new(Arc::clone(&store));

        let result = tool
            .call(
                &context("gA"),
                &serde_json::json!({ "delay": "10m", "text": "stretch" }),
            )
            .await
            .unwrap();
        assert!(result.contains("stretch"), "{result}");

        // It landed in the turn's chat, not anywhere else.
        let listed = store.list("gA").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].text, "stretch");
        assert!(store.list("gB").unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_bad_delay_is_an_error_not_a_reminder() {
        let store = Arc::new(ReminderStore::open(":memory:").unwrap());
        let tool = SetReminder::new(Arc::clone(&store));

        let error = tool
            .call(
                &context("gA"),
                &serde_json::json!({ "delay": "soon", "text": "x" }),
            )
            .await
            .unwrap_err();
        assert!(error.contains("soon"), "{error}");
        assert!(store.list("gA").unwrap().is_empty());
    }
}
